use crate::couchdb::{CouchDbClient, NoteDoc};
use crate::search::{NoteEntry, SearchIndex, extract_title};
use anyhow::Result;
use futures::StreamExt;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::RwLock;
use tokio_util::sync::CancellationToken;

/// Change event from CouchDB _changes feed
#[derive(Debug, serde::Deserialize)]
pub struct ChangeEvent {
    pub seq: String,
    pub id: String,
    #[serde(default)]
    pub deleted: bool,
    pub doc: Option<serde_json::Value>,
}

/// Watches CouchDB _changes feed and updates the search index
pub struct ChangesWatcher {
    db: CouchDbClient,
    index: Arc<RwLock<SearchIndex>>,
}

impl ChangesWatcher {
    pub fn new(db: CouchDbClient, index: Arc<RwLock<SearchIndex>>) -> Self {
        Self { db, index }
    }

    /// Run the changes watcher. Reconnects automatically on errors.
    pub async fn run(&self, cancel: CancellationToken) -> Result<()> {
        loop {
            // Get current seq to resume from
            let since = {
                let index = self.index.read().await;
                index.last_seq.clone()
            };

            // Use "now" if no seq yet (we already did initial load)
            let since_param = since.as_deref().unwrap_or("now");

            tracing::info!("Starting changes watcher from seq: {}", since_param);

            match self.watch_changes(since_param, &cancel).await {
                Ok(()) => {
                    // Clean exit (cancelled)
                    tracing::info!("Changes watcher stopped");
                    break;
                }
                Err(e) => {
                    tracing::warn!("Changes feed error, reconnecting in 5s: {}", e);
                    tokio::select! {
                        _ = tokio::time::sleep(Duration::from_secs(5)) => {}
                        _ = cancel.cancelled() => {
                            tracing::info!("Changes watcher cancelled during reconnect wait");
                            break;
                        }
                    }
                }
            }
        }
        Ok(())
    }

    async fn watch_changes(&self, since: &str, cancel: &CancellationToken) -> Result<()> {
        let url = format!(
            "{}/_changes?feed=continuous&include_docs=true&since={}&heartbeat=30000",
            self.db.db_url(),
            urlencoding::encode(since)
        );

        let response = self.db.get(&url).await?;

        if !response.status().is_success() {
            let status = response.status();
            let body = response.text().await.unwrap_or_default();

            // If seq is invalid (too old/compacted), trigger full resync
            if body.contains("since") || status.as_u16() == 400 {
                tracing::warn!("Invalid seq, triggering full resync");
                self.full_resync().await?;
                return Ok(());
            }

            return Err(anyhow::anyhow!(
                "Changes feed request failed: {} - {}",
                status,
                body
            ));
        }

        let mut stream = response.bytes_stream();
        let mut buffer = String::new();

        loop {
            tokio::select! {
                chunk = stream.next() => {
                    match chunk {
                        Some(Ok(bytes)) => {
                            buffer.push_str(&String::from_utf8_lossy(&bytes));

                            // Process complete lines (CouchDB sends one JSON per line)
                            while let Some(pos) = buffer.find('\n') {
                                let line = &buffer[..pos];
                                let line = line.trim();

                                if !line.is_empty()
                                    && let Err(e) = self.process_change(line).await
                                {
                                    tracing::warn!("Error processing change: {}", e);
                                }

                                buffer = buffer[pos + 1..].to_string();
                            }
                        }
                        Some(Err(e)) => {
                            return Err(anyhow::anyhow!("Stream error: {}", e));
                        }
                        None => {
                            // Stream ended (server closed connection)
                            tracing::debug!("Changes stream ended");
                            return Ok(());
                        }
                    }
                }
                _ = cancel.cancelled() => {
                    return Ok(());
                }
            }
        }
    }

    async fn process_change(&self, line: &str) -> Result<()> {
        let change: ChangeEvent = serde_json::from_str(line)?;

        // Skip chunk documents (h:*) and system docs (_*)
        if change.id.starts_with("h:") || change.id.starts_with('_') {
            // Still update seq
            let mut index = self.index.write().await;
            index.last_seq = Some(change.seq);
            return Ok(());
        }

        if change.deleted {
            // Hard-deleted: remove from index and update seq
            let mut index = self.index.write().await;
            index.remove(&change.id);
            index.last_seq = Some(change.seq);
            tracing::debug!("Removed from search index: {}", change.id);
        } else if let Some(doc_value) = change.doc {
            // Parse the note document
            let note_doc: NoteDoc = serde_json::from_value(doc_value)?;

            if note_doc.deleted == Some(true) {
                // Soft-deleted: remove from index and update seq
                let mut index = self.index.write().await;
                index.remove(&change.id);
                index.last_seq = Some(change.seq);
                tracing::debug!("Removed soft-deleted from search index: {}", change.id);
            } else {
                // Active note: fetch content (without holding lock), then update index
                let content = self.db.decode_content(&note_doc).await?;
                let title = extract_title(&change.id, &content);

                let mut index = self.index.write().await;
                index.upsert(
                    change.id.clone(),
                    NoteEntry {
                        path: change.id.clone(),
                        title,
                        content,
                        mtime: note_doc.mtime,
                    },
                );
                index.last_seq = Some(change.seq);
                tracing::debug!("Updated search index: {}", change.id);
            }
        } else {
            // No doc included (shouldn't happen with include_docs=true, but handle gracefully)
            let mut index = self.index.write().await;
            index.last_seq = Some(change.seq);
        }

        Ok(())
    }

    /// Perform a full resync of the index
    async fn full_resync(&self) -> Result<()> {
        tracing::info!("Performing full search index resync");

        let (notes, last_seq) = self.db.get_all_notes_with_content().await?;

        let mut index = self.index.write().await;
        index.clear();

        for (path, content, mtime) in notes {
            let title = extract_title(&path, &content);
            index.upsert(
                path.clone(),
                NoteEntry {
                    path,
                    title,
                    content,
                    mtime,
                },
            );
        }

        index.last_seq = last_seq;

        tracing::info!("Full resync complete, {} notes indexed", index.len());

        Ok(())
    }
}

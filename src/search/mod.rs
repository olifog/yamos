mod watcher;

pub use watcher::ChangesWatcher;

use nucleo_matcher::{
    Config, Matcher, Utf32Str,
    pattern::{CaseMatching, Normalization, Pattern},
};
use std::collections::HashMap;

/// A single note's indexed content
#[derive(Debug, Clone)]
pub struct NoteEntry {
    pub path: String,
    pub title: String,
    pub content: String,
    #[allow(dead_code)] // Kept for potential future use (e.g., sorting by recency)
    pub mtime: u64,
}

/// Result from a search query
#[derive(Debug, Clone)]
pub struct SearchResult {
    pub path: String,
    pub title: String,
    pub score: u32,
    pub snippet: Option<String>,
}

/// Options for search queries
pub struct SearchOptions {
    pub limit: usize,
    pub search_content: bool,
}

impl Default for SearchOptions {
    fn default() -> Self {
        Self {
            limit: 20,
            search_content: true,
        }
    }
}

/// In-memory search index for all notes
pub struct SearchIndex {
    notes: HashMap<String, NoteEntry>,
    pub last_seq: Option<String>,
}

impl SearchIndex {
    pub fn new() -> Self {
        Self {
            notes: HashMap::new(),
            last_seq: None,
        }
    }

    pub fn len(&self) -> usize {
        self.notes.len()
    }

    #[allow(dead_code)]
    pub fn is_empty(&self) -> bool {
        self.notes.is_empty()
    }

    /// Insert or update a note in the index
    pub fn upsert(&mut self, path: String, entry: NoteEntry) {
        self.notes.insert(path, entry);
    }

    /// Remove a note from the index
    pub fn remove(&mut self, path: &str) {
        self.notes.remove(path);
    }

    /// Clear the index (for full resync)
    pub fn clear(&mut self) {
        self.notes.clear();
        self.last_seq = None;
    }

    /// Fuzzy search notes by title and optionally content
    pub fn search(&self, query: &str, opts: SearchOptions) -> Vec<SearchResult> {
        if query.is_empty() {
            return vec![];
        }

        let mut matcher = Matcher::new(Config::DEFAULT);
        let pattern = Pattern::parse(query, CaseMatching::Smart, Normalization::Smart);

        let mut results: Vec<SearchResult> = self
            .notes
            .values()
            .filter_map(|note| {
                // Convert strings to Utf32Str for nucleo
                let mut title_buf = Vec::new();
                let title_str = Utf32Str::new(&note.title, &mut title_buf);

                // Score title match (weighted higher)
                let title_score = pattern.score(title_str, &mut matcher);

                // Score content match if enabled
                let (content_score, snippet) = if opts.search_content {
                    let mut content_buf = Vec::new();
                    let content_str = Utf32Str::new(&note.content, &mut content_buf);
                    let score = pattern.score(content_str, &mut matcher);

                    let snippet = if score.is_some() {
                        extract_snippet(&note.content, query)
                    } else {
                        None
                    };

                    (score, snippet)
                } else {
                    (None, None)
                };

                // Combine scores: title matches are worth 2x
                let combined_score = match (title_score, content_score) {
                    (Some(t), Some(c)) => Some(t.saturating_mul(2).saturating_add(c)),
                    (Some(t), None) => Some(t.saturating_mul(2)),
                    (None, Some(c)) => Some(c),
                    (None, None) => None,
                };

                combined_score.map(|score| SearchResult {
                    path: note.path.clone(),
                    title: note.title.clone(),
                    score,
                    snippet,
                })
            })
            .collect();

        // Sort by score descending
        results.sort_by(|a, b| b.score.cmp(&a.score));
        results.truncate(opts.limit);
        results
    }
}

impl Default for SearchIndex {
    fn default() -> Self {
        Self::new()
    }
}

/// Extract the title from a note - first H1 heading or filename
pub fn extract_title(path: &str, content: &str) -> String {
    // Track if we're inside frontmatter
    let mut in_frontmatter = false;
    let mut frontmatter_started = false;

    for line in content.lines() {
        let trimmed = line.trim();

        // Handle frontmatter (YAML between --- markers)
        if trimmed == "---" {
            if !frontmatter_started {
                frontmatter_started = true;
                in_frontmatter = true;
                continue;
            } else if in_frontmatter {
                in_frontmatter = false;
                continue;
            }
        }

        // Skip if inside frontmatter
        if in_frontmatter {
            continue;
        }

        // Skip empty lines
        if trimmed.is_empty() {
            continue;
        }

        // Found H1 heading
        if let Some(title) = trimmed.strip_prefix("# ") {
            return title.trim().to_string();
        }

        // Found non-empty, non-heading content - stop looking
        break;
    }

    // Fall back to filename without .md
    path.trim_end_matches(".md")
        .rsplit('/')
        .next()
        .unwrap_or(path)
        .to_string()
}

/// Extract a snippet around the first match location
fn extract_snippet(content: &str, query: &str) -> Option<String> {
    // Simple case-insensitive search for the query
    let content_lower = content.to_lowercase();
    let query_lower = query.to_lowercase();

    // Try to find any word from the query
    let query_words: Vec<&str> = query_lower.split_whitespace().collect();

    let match_pos = query_words
        .iter()
        .filter_map(|word| content_lower.find(word))
        .min()?;

    // Extract ~50 chars on each side
    let context_size = 50;
    let start = match_pos.saturating_sub(context_size);
    let end = (match_pos + context_size).min(content.len());

    // Find word boundaries safely (handling multi-byte UTF-8 characters)
    let start = content[..start]
        .rfind(char::is_whitespace)
        .map(|i| {
            // Advance past the whitespace character (which may be multi-byte)
            let ws_char = content[i..].chars().next().unwrap();
            i + ws_char.len_utf8()
        })
        .unwrap_or(start);

    let end = content[end..]
        .find(char::is_whitespace)
        .map(|i| end + i)
        .unwrap_or(end);

    let mut snippet = content[start..end].to_string();

    // Add ellipsis if truncated
    if start > 0 {
        snippet = format!("...{}", snippet);
    }
    if end < content.len() {
        snippet = format!("{}...", snippet);
    }

    // Clean up whitespace
    let snippet = snippet.split_whitespace().collect::<Vec<_>>().join(" ");

    Some(snippet)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_extract_title_from_heading() {
        let content = "# My Great Note\n\nSome content here";
        assert_eq!(extract_title("notes/test.md", content), "My Great Note");
    }

    #[test]
    fn test_extract_title_from_path() {
        let content = "No heading here, just content";
        assert_eq!(
            extract_title("Projects/my-project.md", content),
            "my-project"
        );
    }

    #[test]
    fn test_extract_title_with_frontmatter() {
        let content = "---\ntags: [test]\n---\n\n# Actual Title\n\nContent";
        assert_eq!(extract_title("test.md", content), "Actual Title");
    }

    #[test]
    fn test_search_empty_query() {
        let index = SearchIndex::new();
        let results = index.search("", SearchOptions::default());
        assert!(results.is_empty());
    }

    #[test]
    fn test_search_basic() {
        let mut index = SearchIndex::new();
        index.upsert(
            "test.md".to_string(),
            NoteEntry {
                path: "test.md".to_string(),
                title: "Meeting Notes".to_string(),
                content: "Discussed the project roadmap".to_string(),
                mtime: 0,
            },
        );

        let results = index.search("meeting", SearchOptions::default());
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].path, "test.md");
    }

    #[test]
    fn test_extract_snippet() {
        let content = "This is some really long content that contains many words. The word meeting appears somewhere in the middle of this very long text. And then there is much more content after that which goes on and on for quite a while to make sure we have enough text to actually truncate.";
        let snippet = extract_snippet(content, "meeting").unwrap();
        assert!(snippet.contains("meeting"));
        // Snippet should be truncated (shorter than original)
        assert!(
            snippet.len() < content.len(),
            "snippet should be shorter than content. snippet: {}, content len: {}",
            snippet.len(),
            content.len()
        );
    }
}

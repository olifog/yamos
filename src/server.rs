use crate::couchdb::CouchDbClient;
use crate::search::{SearchIndex, SearchOptions};
use rmcp::{
    ErrorData as McpError, ServerHandler,
    handler::server::{router::tool::ToolRouter, wrapper::Parameters},
    model::*,
    schemars, tool, tool_handler, tool_router,
};
use serde::{Deserialize, Serialize};
use std::borrow::Cow;
use std::sync::Arc;
use tokio::sync::RwLock;

/// Validate a note path to prevent path traversal and ensure it's a valid Obsidian note path.
fn validate_note_path(path: &str) -> Result<(), McpError> {
    let check = |cond: bool, msg: &str| if cond { Err(mcp_error(msg)) } else { Ok(()) };

    check(path.is_empty(), "Note path cannot be empty")?;
    check(!path.ends_with(".md"), "Note path must end with .md")?;
    check(path.contains(".."), "Note path cannot contain '..'")?;
    check(path.starts_with('/'), "Note path cannot start with '/'")?;
    check(path.contains('\0'), "Note path cannot contain null bytes")?;

    // Allowed: alphanumeric, space, hyphen, underscore, dot, slash, parentheses
    let invalid_char = path
        .chars()
        .find(|c| !c.is_alphanumeric() && !" -_./()'".contains(*c));

    if let Some(c) = invalid_char {
        return Err(mcp_error(format!(
            "Note path contains invalid character: '{c}'"
        )));
    }

    Ok(())
}

#[derive(Clone)]
pub struct YamosServer {
    db: CouchDbClient,
    search_index: Arc<RwLock<SearchIndex>>,
    tool_router: ToolRouter<Self>,
}

// Request types for tools with parameters
#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct ListNotesRequest {
    #[schemars(description = "Optional path prefix to filter notes (e.g. 'Projects/')")]
    pub prefix: Option<String>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct ReadNoteRequest {
    #[schemars(description = "Path to the note (e.g. 'Todo.md' or 'Projects/myproject.md')")]
    pub path: String,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct WriteNoteRequest {
    #[schemars(description = "Path to the note (e.g. 'Todo.md' or 'Projects/myproject.md')")]
    pub path: String,
    #[schemars(description = "Content to write to the note")]
    pub content: String,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct AppendNoteRequest {
    #[schemars(description = "Path to the note to append to")]
    pub path: String,
    #[schemars(description = "Content to append (will be added on a new line)")]
    pub content: String,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct EditNoteRequest {
    #[schemars(description = "Path to the note")]
    pub path: String,
    #[schemars(
        description = "The exact text to find and replace. Must appear exactly once in the note. Include surrounding context (a few lines before/after) to ensure uniqueness."
    )]
    pub old_string: String,
    #[schemars(
        description = "The text to replace old_string with. Include the same surrounding context, plus your changes. Can be empty to delete the old_string."
    )]
    pub new_string: String,
}

// Batch operation request types

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct BatchReadNotesRequest {
    #[schemars(description = "List of note paths to read")]
    pub paths: Vec<String>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct BatchWriteOp {
    #[schemars(description = "Path to the note")]
    pub path: String,
    #[schemars(description = "Content to write")]
    pub content: String,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct BatchWriteNotesRequest {
    #[schemars(description = "List of notes to write")]
    pub notes: Vec<BatchWriteOp>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct BatchAppendOp {
    #[schemars(description = "Path to the note")]
    pub path: String,
    #[schemars(description = "Content to append")]
    pub content: String,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct BatchAppendNotesRequest {
    #[schemars(description = "List of notes to append to")]
    pub notes: Vec<BatchAppendOp>,
}

// Batch operation result types (for partial success reporting)

#[derive(Debug, Serialize)]
pub struct BatchReadResult {
    pub path: String,
    pub success: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub content: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

#[derive(Debug, Serialize)]
pub struct BatchWriteResult {
    pub path: String,
    pub success: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

#[derive(Debug, Serialize)]
pub struct BatchAppendResult {
    pub path: String,
    pub success: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

// Search request/response types

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct SearchNotesRequest {
    #[schemars(description = "Search query (fuzzy matching)")]
    pub query: String,

    #[schemars(description = "Search note content in addition to titles (default: true)")]
    pub search_content: Option<bool>,

    #[schemars(description = "Maximum number of results (default: 20)")]
    pub limit: Option<usize>,
}

#[derive(Debug, Serialize)]
pub struct SearchResultResponse {
    pub path: String,
    pub title: String,
    pub score: u32,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub snippet: Option<String>,
}

fn mcp_error(msg: impl Into<String>) -> McpError {
    McpError {
        code: ErrorCode::INTERNAL_ERROR,
        message: Cow::Owned(msg.into()),
        data: None,
    }
}

#[tool_router]
impl YamosServer {
    pub fn new(db: CouchDbClient, search_index: Arc<RwLock<SearchIndex>>) -> Self {
        Self {
            db,
            search_index,
            tool_router: Self::tool_router(),
        }
    }

    #[tool(
        description = "List all notes in the Obsidian vault, optionally filtered by path prefix"
    )]
    async fn list_notes(
        &self,
        Parameters(req): Parameters<ListNotesRequest>,
    ) -> Result<CallToolResult, McpError> {
        let notes = self
            .db
            .list_notes()
            .await
            .map_err(|e| mcp_error(e.to_string()))?;

        let filtered: Vec<_> = match &req.prefix {
            Some(prefix) => notes
                .into_iter()
                .filter(|n| n.starts_with(prefix))
                .collect(),
            None => notes,
        };

        let result = filtered.join("\n");
        Ok(CallToolResult::success(vec![Content::text(result)]))
    }

    #[tool(description = "Read the content of a note from the Obsidian vault")]
    async fn read_note(
        &self,
        Parameters(req): Parameters<ReadNoteRequest>,
    ) -> Result<CallToolResult, McpError> {
        validate_note_path(&req.path)?;

        let doc = self
            .db
            .get_note(&req.path)
            .await
            .map_err(|e| mcp_error(e.to_string()))?;

        let content = self
            .db
            .decode_content(&doc)
            .await
            .map_err(|e| mcp_error(e.to_string()))?;

        Ok(CallToolResult::success(vec![Content::text(content)]))
    }

    #[tool(description = "Create or update a note in the Obsidian vault")]
    async fn write_note(
        &self,
        Parameters(req): Parameters<WriteNoteRequest>,
    ) -> Result<CallToolResult, McpError> {
        validate_note_path(&req.path)?;

        self.db
            .save_note(&req.path, &req.content)
            .await
            .map_err(|e| mcp_error(e.to_string()))?;

        Ok(CallToolResult::success(vec![Content::text(format!(
            "Successfully wrote to {}",
            req.path
        ))]))
    }

    #[tool(description = "Append content to an existing note (adds a newline before the content)")]
    async fn append_to_note(
        &self,
        Parameters(req): Parameters<AppendNoteRequest>,
    ) -> Result<CallToolResult, McpError> {
        validate_note_path(&req.path)?;

        self.db
            .append_to_note(&req.path, &req.content)
            .await
            .map_err(|e| mcp_error(e.to_string()))?;

        Ok(CallToolResult::success(vec![Content::text(format!(
            "Successfully appended to {}",
            req.path
        ))]))
    }

    #[tool(
        description = "Edit a note by replacing old_string with new_string. The old_string must appear exactly once in the note - include enough surrounding context to make it unique. To insert text, include the surrounding lines in both old_string and new_string, with your new content added in new_string. To delete text, include it in old_string with surrounding context, and omit it from new_string."
    )]
    async fn edit_note(
        &self,
        Parameters(req): Parameters<EditNoteRequest>,
    ) -> Result<CallToolResult, McpError> {
        validate_note_path(&req.path)?;

        if req.old_string.is_empty() {
            return Err(mcp_error(
                "old_string cannot be empty - include surrounding context to identify where to make changes",
            ));
        }

        if req.old_string == req.new_string {
            return Err(mcp_error("old_string and new_string are identical"));
        }

        let doc = self
            .db
            .get_note(&req.path)
            .await
            .map_err(|e| mcp_error(e.to_string()))?;

        let content = self
            .db
            .decode_content(&doc)
            .await
            .map_err(|e| mcp_error(e.to_string()))?;

        // Find all occurrences of old_string
        let matches: Vec<_> = content.match_indices(&req.old_string).collect();

        match matches.len() {
            0 => Err(mcp_error(
                "old_string not found in note - make sure it matches exactly, including whitespace",
            )),
            1 => {
                let new_content = content.replacen(&req.old_string, &req.new_string, 1);
                self.db
                    .save_note(&req.path, &new_content)
                    .await
                    .map_err(|e| mcp_error(e.to_string()))?;

                Ok(CallToolResult::success(vec![Content::text(format!(
                    "Successfully edited {}",
                    req.path
                ))]))
            }
            n => Err(mcp_error(format!(
                "old_string appears {} times in the note - include more surrounding context to make it unique",
                n
            ))),
        }
    }

    #[tool(
        description = "Read multiple notes at once. Returns content for each note, with per-note success/failure reporting."
    )]
    async fn batch_read_notes(
        &self,
        Parameters(req): Parameters<BatchReadNotesRequest>,
    ) -> Result<CallToolResult, McpError> {
        let mut results = Vec::with_capacity(req.paths.len());

        for path in req.paths {
            let result = match validate_note_path(&path) {
                Err(e) => BatchReadResult {
                    path,
                    success: false,
                    content: None,
                    error: Some(e.message.to_string()),
                },
                Ok(()) => match self.db.get_note(&path).await {
                    Err(e) => BatchReadResult {
                        path,
                        success: false,
                        content: None,
                        error: Some(e.to_string()),
                    },
                    Ok(doc) => match self.db.decode_content(&doc).await {
                        Err(e) => BatchReadResult {
                            path,
                            success: false,
                            content: None,
                            error: Some(e.to_string()),
                        },
                        Ok(content) => BatchReadResult {
                            path,
                            success: true,
                            content: Some(content),
                            error: None,
                        },
                    },
                },
            };
            results.push(result);
        }

        let json = serde_json::to_string_pretty(&results).map_err(|e| mcp_error(e.to_string()))?;
        Ok(CallToolResult::success(vec![Content::text(json)]))
    }

    #[tool(
        description = "Write multiple notes at once. Each note is created or updated independently, with per-note success/failure reporting."
    )]
    async fn batch_write_notes(
        &self,
        Parameters(req): Parameters<BatchWriteNotesRequest>,
    ) -> Result<CallToolResult, McpError> {
        let mut results = Vec::with_capacity(req.notes.len());

        for note in req.notes {
            let result = match validate_note_path(&note.path) {
                Err(e) => BatchWriteResult {
                    path: note.path,
                    success: false,
                    error: Some(e.message.to_string()),
                },
                Ok(()) => match self.db.save_note(&note.path, &note.content).await {
                    Err(e) => BatchWriteResult {
                        path: note.path,
                        success: false,
                        error: Some(e.to_string()),
                    },
                    Ok(_) => BatchWriteResult {
                        path: note.path,
                        success: true,
                        error: None,
                    },
                },
            };
            results.push(result);
        }

        let json = serde_json::to_string_pretty(&results).map_err(|e| mcp_error(e.to_string()))?;
        Ok(CallToolResult::success(vec![Content::text(json)]))
    }

    #[tool(
        description = "Append content to multiple notes at once. Each append adds a newline before the content. Per-note success/failure reporting."
    )]
    async fn batch_append_to_notes(
        &self,
        Parameters(req): Parameters<BatchAppendNotesRequest>,
    ) -> Result<CallToolResult, McpError> {
        let mut results = Vec::with_capacity(req.notes.len());

        for note in req.notes {
            let result = match validate_note_path(&note.path) {
                Err(e) => BatchAppendResult {
                    path: note.path,
                    success: false,
                    error: Some(e.message.to_string()),
                },
                Ok(()) => match self.db.append_to_note(&note.path, &note.content).await {
                    Err(e) => BatchAppendResult {
                        path: note.path,
                        success: false,
                        error: Some(e.to_string()),
                    },
                    Ok(_) => BatchAppendResult {
                        path: note.path,
                        success: true,
                        error: None,
                    },
                },
            };
            results.push(result);
        }

        let json = serde_json::to_string_pretty(&results).map_err(|e| mcp_error(e.to_string()))?;
        Ok(CallToolResult::success(vec![Content::text(json)]))
    }

    #[tool(
        description = "Fuzzy search notes by title and/or content. Returns ranked results with relevance scores. Use this to find notes when you don't know the exact path."
    )]
    async fn search_notes(
        &self,
        Parameters(req): Parameters<SearchNotesRequest>,
    ) -> Result<CallToolResult, McpError> {
        let index = self.search_index.read().await;

        let results = index.search(
            &req.query,
            SearchOptions {
                limit: req.limit.unwrap_or(20),
                search_content: req.search_content.unwrap_or(true),
            },
        );

        let response: Vec<SearchResultResponse> = results
            .into_iter()
            .map(|r| SearchResultResponse {
                path: r.path,
                title: r.title,
                score: r.score,
                snippet: r.snippet,
            })
            .collect();

        let json = serde_json::to_string_pretty(&response).map_err(|e| mcp_error(e.to_string()))?;
        Ok(CallToolResult::success(vec![Content::text(json)]))
    }
}

#[tool_handler]
impl ServerHandler for YamosServer {
    fn get_info(&self) -> ServerInfo {
        ServerInfo {
            protocol_version: ProtocolVersion::V_2024_11_05,
            capabilities: ServerCapabilities::builder().enable_tools().build(),
            server_info: Implementation::from_build_env(),
            instructions: Some(
                "Obsidian vault access via CouchDB/LiveSync. Use search_notes to find notes by fuzzy matching on titles and content. Use tools to list, read, write, edit, or append notes. For edit_note, include surrounding context in old_string to ensure uniqueness. Batch operations available for multi-note ops.".to_string(),
            ),
        }
    }
}

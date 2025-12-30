use super::handlers::OAuthAppState;
use super::traits::{CodeChallengeMethod, ResponseType};
use axum::{
    extract::{Query, State},
    http::{header, HeaderMap, StatusCode},
    response::{Html, IntoResponse, Redirect, Response},
};
use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine};
use serde::Deserialize;
use sha2::{Digest, Sha256};
use std::collections::{HashMap, VecDeque};
use std::sync::Arc;
use tokio::sync::RwLock;
use url::Url;
use uuid::Uuid;

/// max pending authorisations before we start evicting old ones
const MAX_PENDING_AUTHORISATIONS: usize = 1000;

/// stores pending auth requests (in-memory, doesn't persist)
#[derive(Clone, Default)]
pub struct AuthorizationStore {
    pending: Arc<RwLock<HashMap<String, PendingAuthorization>>>,
    /// track insertion order for LRU eviction
    insertion_order: Arc<RwLock<VecDeque<String>>>,
}

#[derive(Clone, Debug)]
pub struct PendingAuthorization {
    pub client_id: String,
    pub redirect_uri: String,
    pub code_challenge: String,
    pub code_challenge_method: CodeChallengeMethod,
    pub state: Option<String>,
    pub created_at: std::time::Instant,
}

/// registry of clients and their allowed redirect URIs
#[derive(Clone, Default)]
pub struct ClientRegistry {
    /// map of client_id -> allowed redirect URIs
    clients: Arc<RwLock<HashMap<String, RegisteredClient>>>,
}

#[derive(Clone, Debug)]
pub struct RegisteredClient {
    pub client_id: String,
    pub redirect_uris: Vec<String>,
    pub created_at: std::time::Instant,
}

impl ClientRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    /// register a client with its allowed redirect URIs
    pub async fn register(&self, client_id: String, redirect_uris: Vec<String>) {
        let mut clients = self.clients.write().await;
        clients.insert(
            client_id.clone(),
            RegisteredClient {
                client_id,
                redirect_uris,
                created_at: std::time::Instant::now(),
            },
        );
    }

    /// check if a redirect_uri is valid for the given client
    /// returns Ok(()) if valid, Err with reason if not
    pub async fn validate_redirect_uri(
        &self,
        client_id: &str,
        redirect_uri: &str,
    ) -> Result<(), String> {
        // first, validate the redirect_uri is a valid URL
        let parsed = Url::parse(redirect_uri)
            .map_err(|_| "invalid redirect_uri: not a valid URL".to_string())?;

        // reject dangerous schemes
        match parsed.scheme() {
            "https" => {} // always allowed
            "http" => {
                // only allow http for localhost (development)
                if let Some(host) = parsed.host_str() {
                    if host != "localhost" && host != "127.0.0.1" && host != "[::1]" {
                        return Err(
                            "invalid redirect_uri: http only allowed for localhost".to_string()
                        );
                    }
                }
            }
            scheme => {
                // allow custom schemes for native apps (e.g., myapp://)
                // but reject javascript:, data:, etc.
                if scheme == "javascript" || scheme == "data" || scheme == "vbscript" {
                    return Err(format!(
                        "invalid redirect_uri: {} scheme not allowed",
                        scheme
                    ));
                }
            }
        }

        // check if client is registered
        let clients = self.clients.read().await;
        if let Some(client) = clients.get(client_id) {
            // check if redirect_uri matches any registered URI
            for registered_uri in &client.redirect_uris {
                if Self::redirect_uri_matches(registered_uri, redirect_uri) {
                    return Ok(());
                }
            }
            Err("invalid redirect_uri: not registered for this client".to_string())
        } else {
            // client not registered - for backwards compat with static clients,
            // we allow the request but log a warning. in strict mode, this would be an error.
            tracing::warn!(
                "client '{}' not found in registry, allowing redirect_uri '{}'",
                client_id,
                redirect_uri
            );
            Ok(())
        }
    }

    /// check if a redirect_uri matches a registered pattern
    /// supports exact match and localhost port wildcards
    fn redirect_uri_matches(registered: &str, requested: &str) -> bool {
        // exact match
        if registered == requested {
            return true;
        }

        // for localhost, allow any port if registered with port 0 or without port
        if let (Ok(reg_url), Ok(req_url)) = (Url::parse(registered), Url::parse(requested)) {
            if reg_url.scheme() == req_url.scheme() {
                if let (Some(reg_host), Some(req_host)) = (reg_url.host_str(), req_url.host_str()) {
                    // allow localhost port flexibility for development
                    if (reg_host == "localhost" || reg_host == "127.0.0.1")
                        && reg_host == req_host
                        && reg_url.path() == req_url.path()
                    {
                        return true;
                    }
                }
            }
        }

        false
    }
}

impl AuthorizationStore {
    pub fn new() -> Self {
        Self::default()
    }

    pub async fn store_pending(&self, code: String, auth: PendingAuthorization) {
        let mut pending = self.pending.write().await;
        let mut order = self.insertion_order.write().await;

        // evict oldest entries if at capacity
        while pending.len() >= MAX_PENDING_AUTHORISATIONS {
            if let Some(oldest_code) = order.pop_front() {
                pending.remove(&oldest_code);
                tracing::debug!(
                    "evicted oldest pending authorisation due to capacity limit: {}",
                    oldest_code
                );
            } else {
                break;
            }
        }

        pending.insert(code.clone(), auth);
        order.push_back(code);
    }

    pub async fn take_pending(&self, code: &str) -> Option<PendingAuthorization> {
        let mut pending = self.pending.write().await;
        let mut order = self.insertion_order.write().await;

        // remove from insertion order tracking
        order.retain(|c| c != code);

        pending.remove(code)
    }

    /// boot out anything older than 10 mins
    pub async fn cleanup_expired(&self) {
        let mut pending = self.pending.write().await;
        let mut order = self.insertion_order.write().await;
        let now = std::time::Instant::now();

        // collect expired codes
        let expired: Vec<String> = pending
            .iter()
            .filter(|(_, auth)| now.duration_since(auth.created_at).as_secs() >= 600)
            .map(|(code, _)| code.clone())
            .collect();

        // remove expired entries
        for code in &expired {
            pending.remove(code);
        }

        // clean up insertion order
        order.retain(|code| !expired.contains(code));

        if !expired.is_empty() {
            tracing::debug!("cleaned up {} expired pending authorisations", expired.len());
        }
    }

    /// get current count of pending authorisations (for monitoring)
    pub async fn len(&self) -> usize {
        self.pending.read().await.len()
    }
}

#[derive(Debug, Deserialize)]
pub struct AuthorizationRequest {
    pub client_id: String,
    pub redirect_uri: String,
    pub response_type: ResponseType,
    pub code_challenge: String,
    pub code_challenge_method: Option<CodeChallengeMethod>,
    pub state: Option<String>,
    pub scope: Option<String>,
    pub resource: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct AuthorizationApproval {
    pub code: String,
    pub approve: Option<String>,
}

/// Shows consent page - user clicks approve/deny
pub async fn authorize_handler(
    State(state): State<OAuthAppState>,
    Query(req): Query<AuthorizationRequest>,
) -> Response {
    let store = &state.auth_store;
    tracing::info!(
        "Authorization request from client_id={}, redirect_uri={}",
        req.client_id,
        req.redirect_uri
    );

    // validate redirect_uri BEFORE we redirect anywhere
    // this prevents open redirect attacks
    if let Err(e) = state
        .client_registry
        .validate_redirect_uri(&req.client_id, &req.redirect_uri)
        .await
    {
        tracing::warn!(
            "rejected invalid redirect_uri '{}' for client '{}': {}",
            req.redirect_uri,
            req.client_id,
            e
        );
        // DON'T redirect to the invalid URI - return an error page instead
        return (
            StatusCode::BAD_REQUEST,
            format!("invalid redirect_uri: {}", e),
        )
            .into_response();
    }

    // Generate a temporary code for this authorization session
    let temp_code = Uuid::new_v4().to_string();

    // Store the pending authorization
    let pending = PendingAuthorization {
        client_id: req.client_id.clone(),
        redirect_uri: req.redirect_uri.clone(),
        code_challenge: req.code_challenge.clone(),
        code_challenge_method: req.code_challenge_method.unwrap_or_default(),
        state: req.state.clone(),
        created_at: std::time::Instant::now(),
    };
    store.store_pending(temp_code.clone(), pending).await;

    // Clean up old authorizations
    store.cleanup_expired().await;

    // Show consent page with security headers
    let html = consent_page(&req.client_id, &temp_code);
    let mut headers = HeaderMap::new();
    headers.insert(
        header::CONTENT_SECURITY_POLICY,
        "default-src 'self'; style-src 'unsafe-inline'".parse().unwrap(),
    );
    headers.insert(header::X_CONTENT_TYPE_OPTIONS, "nosniff".parse().unwrap());
    headers.insert(header::X_FRAME_OPTIONS, "DENY".parse().unwrap());

    (headers, Html(html)).into_response()
}

/// Handles the approve/deny button click
pub async fn authorize_approval_handler(
    State(state): State<OAuthAppState>,
    Query(approval): Query<AuthorizationApproval>,
) -> Response {
    let store = &state.auth_store;
    let code = &approval.code;

    // Look up the pending authorization
    let pending = match store.take_pending(code).await {
        Some(p) => p,
        None => {
            tracing::warn!("Authorization code not found or expired: {}", code);
            return (StatusCode::BAD_REQUEST, "Authorization session expired or invalid").into_response();
        }
    };

    // Check if user approved
    if approval.approve.as_deref() != Some("true") {
        return error_redirect(
            &pending.redirect_uri,
            "access_denied",
            "User denied the authorization request",
            pending.state.as_deref(),
        );
    }

    // Generate the actual authorization code
    let auth_code = Uuid::new_v4().to_string();

    // Store the authorization code (reuse temp code storage)
    store.store_pending(auth_code.clone(), pending.clone()).await;

    // redirect back with the authorization code
    let mut redirect_url = pending.redirect_uri.clone();
    redirect_url.push_str(if redirect_url.contains('?') { "&" } else { "?" });
    redirect_url.push_str(&format!("code={}", urlencoding::encode(&auth_code)));
    if let Some(state) = &pending.state {
        redirect_url.push_str(&format!("&state={}", urlencoding::encode(state)));
    }

    tracing::info!(
        "Authorization approved for client_id={}, redirecting to {}",
        pending.client_id,
        redirect_url
    );

    Redirect::temporary(&redirect_url).into_response()
}

/// PKCE verification - S256 only (as per OAuth 2.1)
pub fn verify_pkce(code_verifier: &str, code_challenge: &str) -> bool {
    let mut hasher = Sha256::new();
    hasher.update(code_verifier.as_bytes());
    let hash = hasher.finalize();
    URL_SAFE_NO_PAD.encode(hash) == code_challenge
}

fn error_redirect(redirect_uri: &str, error: &str, description: &str, state: Option<&str>) -> Response {
    let mut url = redirect_uri.to_string();
    url.push_str(if url.contains('?') { "&" } else { "?" });
    url.push_str(&format!(
        "error={}&error_description={}",
        error,
        urlencoding::encode(description)
    ));
    if let Some(s) = state {
        url.push_str(&format!("&state={}", s));
    }
    Redirect::temporary(&url).into_response()
}

fn consent_page(client_id: &str, code: &str) -> String {
    format!(
        r#"<!DOCTYPE html>
<html>
<head>
    <title>Authorize MCP Client</title>
    <meta name="viewport" content="width=device-width, initial-scale=1">
    <style>
        body {{
            font-family: -apple-system, BlinkMacSystemFont, 'Segoe UI', Roboto, sans-serif;
            max-width: 400px;
            margin: 100px auto;
            padding: 20px;
            text-align: center;
        }}
        h1 {{ color: #333; }}
        .client-id {{
            background: #f5f5f5;
            padding: 10px;
            border-radius: 4px;
            font-family: monospace;
            word-break: break-all;
        }}
        .buttons {{ margin-top: 30px; }}
        button {{
            padding: 12px 24px;
            margin: 5px;
            border: none;
            border-radius: 4px;
            cursor: pointer;
            font-size: 16px;
        }}
        .approve {{
            background: #0066cc;
            color: white;
        }}
        .deny {{
            background: #666;
            color: white;
        }}
    </style>
</head>
<body>
    <h1>Authorize Application</h1>
    <p>The following application is requesting access to your MCP server:</p>
    <div class="client-id">{}</div>
    <p>Do you want to allow this application to access your Obsidian notes?</p>
    <div class="buttons">
        <a href="/authorize/callback?code={}&approve=true">
            <button class="approve">Approve</button>
        </a>
        <a href="/authorize/callback?code={}&approve=false">
            <button class="deny">Deny</button>
        </a>
    </div>
</body>
</html>"#,
        html_escape(client_id),
        code,
        code
    )
}

fn html_escape(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
}

mod auth;
mod couchdb;
mod server;

use anyhow::Result;
use clap::{Parser, ValueEnum};
use rmcp::ServiceExt;
use server::YamosServer;
use std::sync::Arc;
use tracing_subscriber::{layer::SubscriberExt, util::SubscriberInitExt};

#[derive(Debug, Clone, Copy, ValueEnum)]
enum TransportMode {
    Stdio,
    Sse,
}

// could this use enums/groups so that we're not offering sse-only flags when using stdio transport? yep.
// do i care? no.
#[derive(Parser, Debug)]
#[command(name = "yamos")]
#[command(about = "yet another mcp obsidian server, for obsidian livesync via couchdb")]
struct Args {
    /// Transport mode to use
    #[arg(short, long, value_enum, env = "MCP_TRANSPORT", default_value = "sse")]
    transport: TransportMode,

    /// Host to bind to (SSE mode only)
    #[arg(long, env = "MCP_HOST", default_value = "localhost")]
    host: String,

    /// Port to bind to (SSE mode only)
    #[arg(short, long, env = "MCP_PORT", default_value = "3000")]
    port: u16,

    /// CouchDB URL
    #[arg(long, env = "COUCHDB_URL", default_value = "http://localhost:5984")]
    couchdb_url: String,

    /// CouchDB database name
    #[arg(long, env = "COUCHDB_DATABASE", default_value = "obsidian")]
    couchdb_database: String,

    /// CouchDB username
    #[arg(long, env = "COUCHDB_USER")]
    couchdb_user: String,

    /// CouchDB password
    #[arg(long, env = "COUCHDB_PASSWORD")]
    couchdb_password: String,

    /// Enable OAuth 2.0 authentication (disables legacy bearer token auth)
    #[arg(long, env = "OAUTH_ENABLED", default_value = "false")]
    oauth_enabled: bool,

    /// JWT signing secret for OAuth tokens
    #[arg(long, env = "OAUTH_JWT_SECRET")]
    oauth_jwt_secret: Option<String>,

    /// Token expiration in seconds (0 = no expiration)
    #[arg(long, env = "OAUTH_TOKEN_EXPIRATION", default_value = "3600")]
    oauth_token_expiration: u64,

    /// OAuth client ID
    #[arg(long, env = "OAUTH_CLIENT_ID")]
    oauth_client_id: Option<String>,

    /// OAuth client secret
    #[arg(long, env = "OAUTH_CLIENT_SECRET")]
    oauth_client_secret: Option<String>,

    /// Authentication token for bearer SSE mode (OAuth is better)
    #[arg(long, env = "MCP_AUTH_TOKEN")]
    auth_token: Option<String>,

    /// Public base URL for OAuth metadata (e.g., https://your-domain.com)
    /// If not set, defaults to http://HOST:PORT
    #[arg(long, env = "PUBLIC_URL")]
    public_url: Option<String>,
}

#[tokio::main]
async fn main() -> Result<()> {
    // Load environment variables from .env file if present
    let _ = dotenvy::dotenv();

    let args = Args::parse();

    // Initialise logging to stderr (so it doesn't interfere with stdio transport)
    tracing_subscriber::registry()
        .with(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "yamos=info".into()),
        )
        .with(tracing_subscriber::fmt::layer().with_writer(std::io::stderr))
        .init();

    tracing::info!(
        "Connecting to CouchDB at {}/{}",
        args.couchdb_url,
        args.couchdb_database
    );

    // Create CouchDB client
    let db = couchdb::CouchDbClient::new(
        &args.couchdb_url,
        &args.couchdb_database,
        &args.couchdb_user,
        &args.couchdb_password,
    )?;

    // Test connection
    db.test_connection().await?;
    tracing::info!("Successfully connected to CouchDB");

    // Create the MCP server
    let server = YamosServer::new(db);

    match args.transport {
        TransportMode::Stdio => {
            tracing::info!("Starting in stdio mode");
            let service = server.serve(rmcp::transport::stdio()).await?;
            service.waiting().await?;
        }
        TransportMode::Sse => {
            tracing::info!("Starting in SSE mode on {}:{}", args.host, args.port);

            let auth_mode = determine_auth_mode(&args)?;

            match auth_mode {
                AuthMode::OAuth(config) => {
                    tracing::info!("OAuth 2.0 authentication enabled");
                    run_sse_server_with_oauth(
                        server,
                        &args.host,
                        args.port,
                        config,
                        args.public_url.as_deref(),
                    )
                    .await?;
                }
                AuthMode::Legacy(token) => {
                    tracing::info!(
                        "Bearer token authentication enabled (consider migrating to OAuth)"
                    );
                    run_sse_server_legacy(server, &args.host, args.port, token).await?;
                }
                AuthMode::None => {
                    tracing::warn!(
                        "WARNING: No authentication enabled. Server is publicly accessible!"
                    );
                    run_sse_server_no_auth(server, &args.host, args.port).await?;
                }
            }
        }
    }

    Ok(())
}

enum AuthMode {
    OAuth(auth::AuthConfig),
    Legacy(String),
    None,
}

fn determine_auth_mode(args: &Args) -> Result<AuthMode> {
    if args.oauth_enabled {
        let jwt_secret = args
            .oauth_jwt_secret
            .as_ref()
            .ok_or_else(|| anyhow::anyhow!("OAUTH_JWT_SECRET required when OAuth is enabled"))?;

        let client_id = args
            .oauth_client_id
            .as_ref()
            .ok_or_else(|| anyhow::anyhow!("OAUTH_CLIENT_ID required when OAuth is enabled"))?;

        let client_secret = args
            .oauth_client_secret
            .as_ref()
            .ok_or_else(|| anyhow::anyhow!("OAUTH_CLIENT_SECRET required when OAuth is enabled"))?;

        Ok(AuthMode::OAuth(auth::AuthConfig {
            jwt_secret: jwt_secret.clone(),
            client_id: client_id.clone(),
            client_secret: client_secret.clone(),
            token_expiration: if args.oauth_token_expiration == 0 {
                None
            } else {
                Some(std::time::Duration::from_secs(args.oauth_token_expiration))
            },
        }))
    } else if let Some(token) = &args.auth_token {
        Ok(AuthMode::Legacy(token.clone()))
    } else {
        Ok(AuthMode::None)
    }
}

async fn run_sse_server_with_oauth(
    server: YamosServer,
    host: &str,
    port: u16,
    config: auth::AuthConfig,
    public_url: Option<&str>,
) -> Result<()> {
    use axum::{
        middleware,
        routing::{get, post},
        Router,
    };
    use rmcp::transport::streamable_http_server::session::local::LocalSessionManager;
    use rmcp::transport::streamable_http_server::tower::{
        StreamableHttpServerConfig, StreamableHttpService,
    };
    use std::net::SocketAddr;

    let addr: SocketAddr = format!("{}:{}", host, port).parse()?;

    // Use public URL if provided, otherwise use local address
    let base_url = public_url
        .map(|url| url.trim_end_matches('/').to_string())
        .unwrap_or_else(|| format!("http://{}", addr));

    tracing::info!("MCP server listening on {}", addr);
    if let Some(public) = public_url {
        tracing::info!("Public URL: {}", public);
    }
    tracing::info!("MCP endpoint: {}/", base_url);
    tracing::info!(
        "Protected resource metadata: {}/.well-known/oauth-protected-resource",
        base_url
    );
    tracing::info!(
        "Authorization server metadata: {}/.well-known/oauth-authorization-server",
        base_url
    );
    tracing::info!("Token endpoint: {}/token", base_url);
    tracing::info!("Registration endpoint: {}/register", base_url);

    let session_manager = Arc::new(LocalSessionManager::default());

    let http_service = StreamableHttpService::new(
        move || Ok(server.clone()),
        session_manager,
        StreamableHttpServerConfig::default(),
    );

    let oauth_service = Arc::new(auth::OAuthService::new(config));
    let auth_store = Arc::new(auth::AuthorizationStore::new());
    let client_registry = Arc::new(auth::ClientRegistry::new());

    // Combined OAuth state for all handlers
    let oauth_state = auth::OAuthAppState {
        oauth_service: oauth_service.clone(),
        auth_store: auth_store.clone(),
        client_registry: client_registry.clone(),
        base_url: base_url.clone(),
    };

    // public oauth endpoints - no auth required (that's the whole point)
    let oauth_routes = Router::new()
        .route(
            "/.well-known/oauth-protected-resource",
            get(auth::protected_resource_metadata_handler),
        )
        .route(
            "/.well-known/oauth-authorization-server",
            get(auth::metadata_handler),
        )
        .route("/authorize", get(auth::authorize_handler))
        .route("/authorize/callback", get(auth::authorize_approval_handler))
        .route("/token", post(auth::oauth_token_handler))
        .route("/register", post(auth::register_handler))
        .with_state(oauth_state);

    let auth_config = auth::AuthMiddlewareConfig {
        oauth_service: oauth_service.clone(),
        base_url: base_url.clone(),
    };

    // protected routes - jwt required
    let protected_routes =
        Router::new()
            .route_service("/", http_service)
            .layer(middleware::from_fn_with_state(
                auth_config,
                auth::jwt_auth_middleware,
            ));

    let app = oauth_routes.merge(protected_routes);

    let listener = tokio::net::TcpListener::bind(addr).await?;
    tracing::info!("Server ready at {}", base_url);

    axum::serve(listener, app).await?;

    Ok(())
}

async fn run_sse_server_legacy(
    server: YamosServer,
    host: &str,
    port: u16,
    token: String,
) -> Result<()> {
    use axum::{middleware, Router};
    use rmcp::transport::streamable_http_server::session::local::LocalSessionManager;
    use rmcp::transport::streamable_http_server::tower::{
        StreamableHttpServerConfig, StreamableHttpService,
    };
    use std::net::SocketAddr;

    let addr: SocketAddr = format!("{}:{}", host, port).parse()?;

    tracing::info!("MCP server listening on http://{}", addr);

    let session_manager = Arc::new(LocalSessionManager::default());

    let http_service = StreamableHttpService::new(
        move || Ok(server.clone()),
        session_manager,
        StreamableHttpServerConfig::default(),
    );

    let token_arc = Arc::new(token);
    let app = Router::new()
        .route_service("/", http_service)
        .layer(middleware::from_fn(move |req, next| {
            auth::legacy_auth_middleware(req, next, token_arc.clone())
        }));

    let listener = tokio::net::TcpListener::bind(addr).await?;
    tracing::info!("Server ready at http://{}", addr);

    axum::serve(listener, app).await?;

    Ok(())
}

async fn run_sse_server_no_auth(server: YamosServer, host: &str, port: u16) -> Result<()> {
    use axum::Router;
    use rmcp::transport::streamable_http_server::session::local::LocalSessionManager;
    use rmcp::transport::streamable_http_server::tower::{
        StreamableHttpServerConfig, StreamableHttpService,
    };
    use std::net::SocketAddr;

    let addr: SocketAddr = format!("{}:{}", host, port).parse()?;

    tracing::info!("MCP server listening on http://{}", addr);

    let session_manager = Arc::new(LocalSessionManager::default());

    let http_service = StreamableHttpService::new(
        move || Ok(server.clone()),
        session_manager,
        StreamableHttpServerConfig::default(),
    );

    let app = Router::new().route_service("/", http_service);

    let listener = tokio::net::TcpListener::bind(addr).await?;
    tracing::info!("Server ready at http://{}", addr);

    axum::serve(listener, app).await?;

    Ok(())
}

use super::OAuthService;
use axum::{
    extract::{Request, State},
    http::{header, HeaderMap, StatusCode},
    middleware::Next,
    response::{IntoResponse, Response},
};
use std::sync::Arc;
use subtle::ConstantTimeEq;

// If you're perusing this code, you can probably tell I'm pretty new to this stuff. There are like
// 50 bajillion RFCs to read and and they're all like 50 bajillion lines long. technology

#[derive(Clone)]
pub struct AuthMiddlewareConfig {
    pub oauth_service: Arc<OAuthService>,
    pub base_url: String,
}

/// JWT authentication middleware - validates Bearer tokens as JWTs
/// Returns WWW-Authenticate header on 401 as required by RFC 9728
pub async fn jwt_auth_middleware(
    State(config): State<AuthMiddlewareConfig>,
    req: Request,
    next: Next,
) -> Response {
    let method = req.method().clone();
    let uri = req.uri().clone();
    let headers = req.headers().clone();

    tracing::debug!(
        "Incoming request: {} {} (headers: {:?})",
        method,
        uri,
        headers.keys().collect::<Vec<_>>()
    );

    let auth_header = headers.get("Authorization").and_then(|h| h.to_str().ok());

    match auth_header {
        Some(header) if header.starts_with("Bearer ") => {
            // There is a split function for this and I will not use it
            let token = &header[7..]; // Skip "Bearer "

            match config.oauth_service.validate_token(token) {
                Ok(claims) => {
                    tracing::debug!("Valid JWT token for client: {}", claims.sub);
                    next.run(req).await
                }
                Err(e) => {
                    tracing::warn!("Invalid JWT token: {}", e);
                    unauthorized_response(&config.base_url, Some("invalid_token"))
                }
            }
        }
        _ => {
            tracing::warn!(
                "Missing or invalid Authorization header for {} {} (got: {:?})",
                method,
                uri,
                auth_header.map(|h| if h.len() > 20 { &h[..20] } else { h })
            );
            unauthorized_response(&config.base_url, None)
        }
    }
}

/// Create a 401 response with WWW-Authenticate header per RFC 9728
fn unauthorized_response(base_url: &str, error: Option<&str>) -> Response {
    let mut headers = HeaderMap::new();

    // Thank uou Claude for figuring this bit out I had no idea why the connector kept erroring
    // Turns out that in the Modern Web(tm) 401 errors are sometimes just part of the happy path
    // WWW-Authenticate header tells clients where to find the protected resource metadata
    let www_auth = if let Some(err) = error {
        format!(
            "Bearer realm=\"{}\", resource_metadata=\"{}/.well-known/oauth-protected-resource\", error=\"{}\"",
            base_url, base_url, err
        )
    } else {
        format!(
            "Bearer realm=\"{}\", resource_metadata=\"{}/.well-known/oauth-protected-resource\"",
            base_url, base_url
        )
    };

    headers.insert(
        header::WWW_AUTHENTICATE,
        www_auth
            .parse()
            .expect("WWW-Authenticate header value should be valid ASCII"),
    );

    (StatusCode::UNAUTHORIZED, headers).into_response()
}

/// for """backward compatibility"""
pub async fn legacy_auth_middleware(
    req: Request,
    next: Next,
    expected_token: Arc<String>,
) -> Result<Response, StatusCode> {
    let auth_header = req
        .headers()
        .get("Authorization")
        .and_then(|h| h.to_str().ok());

    match auth_header {
        Some(header) if header.starts_with("Bearer ") => {
            let token = &header[7..];
            // Use constant-time comparison to prevent timing attacks
            if token.as_bytes().ct_eq(expected_token.as_bytes()).into() {
                Ok(next.run(req).await)
            } else {
                tracing::warn!("Invalid legacy authentication token");
                Err(StatusCode::UNAUTHORIZED)
            }
        }
        _ => {
            tracing::warn!("Missing or invalid Authorization header");
            Err(StatusCode::UNAUTHORIZED)
        }
    }
}

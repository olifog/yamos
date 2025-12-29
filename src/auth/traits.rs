use anyhow::Result;
use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use std::time::Duration;

// async_trait my beloved. this shit rocks

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ClientInfo {
    pub client_id: String,
    pub scopes: Vec<String>, // For future use
}

#[async_trait]
pub trait CredentialValidator {
    async fn validate(&self, client_id: &str, client_secret: &str) -> Result<ClientInfo>;
}

pub trait TokenIssuer {
    fn issue_token(
        &self,
        client_id: &str,
        custom_duration: Option<Duration>,
    ) -> Result<TokenResponse>;
}

pub trait TokenValidator {
    fn validate_token(&self, token: &str) -> Result<Claims>;
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TokenResponse {
    pub access_token: String,
    pub token_type: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub expires_in: Option<u64>, // seconds
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Claims {
    pub sub: String,      // Subject (client_id)
    pub iat: i64,         // Issued at
    #[serde(skip_serializing_if = "Option::is_none")]
    pub exp: Option<i64>, // Expiration time
    pub jti: String,      // JWT ID (unique identifier)
    pub iss: String,      // Issuer
}

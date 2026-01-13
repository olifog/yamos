use super::authorization_code::ClientRegistry;
use super::traits::{ClientInfo, CredentialValidator};
use anyhow::{Result, anyhow};
use async_trait::async_trait;
use std::sync::Arc;
use subtle::ConstantTimeEq;

/// Validates client credentials against both static config and dynamic registry
pub struct ClientValidator {
    /// static credentials from config (fallback)
    expected_client_id: String,
    expected_client_secret: String,
    /// dynamic client registry
    client_registry: Arc<ClientRegistry>,
}

impl ClientValidator {
    pub fn new(
        client_id: String,
        client_secret: String,
        client_registry: Arc<ClientRegistry>,
    ) -> Self {
        Self {
            expected_client_id: client_id,
            expected_client_secret: client_secret,
            client_registry,
        }
    }
}

#[async_trait]
impl CredentialValidator for ClientValidator {
    async fn validate(&self, client_id: &str, client_secret: &str) -> Result<ClientInfo> {
        // First, try dynamic client registry
        if self
            .client_registry
            .validate_credentials(client_id, client_secret)
            .await
            .is_ok()
        {
            tracing::debug!("Validated dynamic client: {}", client_id);
            return Ok(ClientInfo {
                client_id: client_id.to_string(),
                scopes: vec![],
            });
        }

        // Fall back to static credentials
        let id_matches: bool = client_id
            .as_bytes()
            .ct_eq(self.expected_client_id.as_bytes())
            .into();
        let secret_matches: bool = client_secret
            .as_bytes()
            .ct_eq(self.expected_client_secret.as_bytes())
            .into();

        if id_matches && secret_matches {
            tracing::debug!("Validated static client: {}", client_id);
            Ok(ClientInfo {
                client_id: client_id.to_string(),
                scopes: vec![],
            })
        } else {
            tracing::warn!(
                "Invalid client credentials attempted for client_id: {}",
                client_id
            );
            Err(anyhow!("Invalid client credentials"))
        }
    }
}

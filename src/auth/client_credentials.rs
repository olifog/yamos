use super::traits::{ClientInfo, CredentialValidator};
use anyhow::{Result, anyhow};
use async_trait::async_trait;
use subtle::ConstantTimeEq;

/// Static single-client validator (v1 implementation)
/// Future: DatabaseClientValidator, LdapClientValidator, etc.
pub struct StaticClientValidator {
    expected_client_id: String,
    expected_client_secret: String,
}

impl StaticClientValidator {
    pub fn new(client_id: String, client_secret: String) -> Self {
        Self {
            expected_client_id: client_id,
            expected_client_secret: client_secret,
        }
    }
}

#[async_trait]
impl CredentialValidator for StaticClientValidator {
    async fn validate(&self, client_id: &str, client_secret: &str) -> Result<ClientInfo> {
        // Constant-time comparison to prevent timing attacks
        let id_matches: bool = client_id
            .as_bytes()
            .ct_eq(self.expected_client_id.as_bytes())
            .into();
        let secret_matches: bool = client_secret
            .as_bytes()
            .ct_eq(self.expected_client_secret.as_bytes())
            .into();

        if id_matches && secret_matches {
            Ok(ClientInfo {
                client_id: client_id.to_string(),
                scopes: vec![], // No scopes for now
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

use super::traits::{ClientInfo, CredentialValidator};
use anyhow::{anyhow, Result};
use async_trait::async_trait;

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
        if constant_time_compare(client_id, &self.expected_client_id)
            && constant_time_compare(client_secret, &self.expected_client_secret)
        {
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

/// Constant-time string comparison to prevent timing attacks
fn constant_time_compare(a: &str, b: &str) -> bool {
    if a.len() != b.len() {
        return false;
    }

    // can i just give a quick shoutout to fold. gotta be one of my favourite methods. you're
    // telling me i can take everything i learned from python list comprehensions and do them to
    // iterators in rust? coolest shit ever
    a.bytes()
        .zip(b.bytes())
        .fold(0, |acc, (a, b)| acc | (a ^ b))
        == 0
}

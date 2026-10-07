use std::collections::HashMap;
use std::time::Duration;

use crate::{ProviderKind, Result};

const LOAD_ATTEMPTS: u32 = 4;

/// Persists the refresh token in the desktop keyring (Secret Service, which
/// KWallet provides on Plasma). The token never touches a plain file.
pub struct TokenStore {
    provider: ProviderKind,
    client_id: String,
}

impl TokenStore {
    pub fn new(provider: ProviderKind, client_id: impl Into<String>) -> Self {
        Self {
            provider,
            client_id: client_id.into(),
        }
    }

    fn attributes(&self) -> HashMap<&str, &str> {
        HashMap::from([
            ("application", "skydock"),
            ("kind", "refresh-token"),
            ("provider", self.provider.id()),
            ("client-id", self.client_id.as_str()),
        ])
    }

    async fn keyring() -> Result<oo7::Keyring> {
        let keyring = oo7::Keyring::new().await?;
        keyring.unlock().await?;
        Ok(keyring)
    }

    pub async fn load(&self) -> Result<Option<String>> {
        let keyring = Self::keyring().await?;
        let mut attempt = 1;
        loop {
            let items = keyring.search_items(&self.attributes()).await?;
            let Some(item) = items.first() else {
                return Ok(None);
            };
            match item.secret().await {
                Ok(secret) => {
                    return Ok(Some(
                        String::from_utf8_lossy(secret.as_bytes()).into_owned(),
                    ));
                }
                // Saving replaces the entry, which gives it a new address.
                // If another Skydock process saved between our search and
                // this read, the entry we found is gone: look again.
                Err(_) if attempt < LOAD_ATTEMPTS => {
                    tokio::time::sleep(Duration::from_millis(100 * u64::from(attempt))).await;
                    attempt += 1;
                }
                Err(error) => return Err(error.into()),
            }
        }
    }

    pub async fn save(&self, refresh_token: &str) -> Result<()> {
        Self::keyring()
            .await?
            .create_item(
                &format!("Skydock {} sign-in", self.provider.display_name()),
                &self.attributes(),
                refresh_token,
                true,
            )
            .await?;
        Ok(())
    }

    pub async fn clear(&self) -> Result<()> {
        Self::keyring().await?.delete(&self.attributes()).await?;
        Ok(())
    }
}

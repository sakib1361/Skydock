use std::collections::HashMap;

use crate::Result;

/// Persists the refresh token in the desktop keyring (Secret Service, which
/// KWallet provides on Plasma). The token never touches a plain file.
pub struct TokenStore {
    client_id: String,
}

impl TokenStore {
    pub fn new(client_id: impl Into<String>) -> Self {
        Self {
            client_id: client_id.into(),
        }
    }

    fn attributes(&self) -> HashMap<&str, &str> {
        HashMap::from([
            ("application", "odl"),
            ("kind", "refresh-token"),
            ("client-id", self.client_id.as_str()),
        ])
    }

    async fn keyring() -> Result<oo7::Keyring> {
        let keyring = oo7::Keyring::new().await?;
        keyring.unlock().await?;
        Ok(keyring)
    }

    pub async fn load(&self) -> Result<Option<String>> {
        let items = Self::keyring()
            .await?
            .search_items(&self.attributes())
            .await?;
        let Some(item) = items.first() else {
            return Ok(None);
        };
        let secret = item.secret().await?;
        Ok(Some(
            String::from_utf8_lossy(secret.as_bytes()).into_owned(),
        ))
    }

    pub async fn save(&self, refresh_token: &str) -> Result<()> {
        Self::keyring()
            .await?
            .create_item(
                "odl OneDrive sign-in",
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

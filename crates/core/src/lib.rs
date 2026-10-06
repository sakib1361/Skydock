//! Provider-neutral foundation: the [`Provider`] trait every cloud service
//! implements, the item and change types the rest of the application works
//! with, and the sign-in and HTTP plumbing providers share.
//!
//! Adding a provider means: a [`ProviderKind`] variant, a crate implementing
//! [`Provider`], and one arm in the service crate's provider factory.

pub mod error;
pub mod hash;
pub mod http;
pub mod oauth;
pub mod token_store;

use std::fmt;
use std::path::Path;
use std::str::FromStr;

use async_trait::async_trait;

pub use error::{Error, Result};
pub use hash::HashKind;

/// Sent on every request. Microsoft prioritises traffic that identifies
/// itself in this format; other providers ignore the shape.
pub const USER_AGENT: &str = concat!("NONISV|skydock|skydock/", env!("CARGO_PKG_VERSION"));

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ProviderKind {
    OneDrive,
    GoogleDrive,
}

impl ProviderKind {
    pub const ALL: [Self; 2] = [Self::OneDrive, Self::GoogleDrive];

    /// Stable identifier used in config, the keyring and the state database.
    pub fn id(self) -> &'static str {
        match self {
            Self::OneDrive => "onedrive",
            Self::GoogleDrive => "gdrive",
        }
    }

    pub fn display_name(self) -> &'static str {
        match self {
            Self::OneDrive => "OneDrive",
            Self::GoogleDrive => "Google Drive",
        }
    }

    /// Name of this provider's folder inside the user's sync folder. Each
    /// provider gets its own top-level folder; providers never share a path.
    pub fn folder_name(self) -> &'static str {
        self.display_name()
    }
}

impl fmt::Display for ProviderKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.display_name())
    }
}

impl FromStr for ProviderKind {
    type Err = String;

    fn from_str(s: &str) -> std::result::Result<Self, String> {
        Self::ALL
            .into_iter()
            .find(|kind| kind.id() == s)
            .ok_or_else(|| {
                let known: Vec<_> = Self::ALL.iter().map(|kind| kind.id()).collect();
                format!(
                    "unknown provider '{s}', expected one of: {}",
                    known.join(", ")
                )
            })
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Account {
    /// Stable identifier of the account's drive at the provider.
    pub id: String,
    pub display_name: Option<String>,
    pub email: Option<String>,
    pub quota_used: Option<u64>,
    pub quota_total: Option<u64>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RemoteItem {
    pub id: String,
    /// `None` only for the drive root.
    pub parent_id: Option<String>,
    pub name: String,
    pub is_folder: bool,
    pub size: Option<u64>,
    /// Changes whenever anything about the item changes.
    pub version: Option<String>,
    /// Content hash in the provider's own algorithm (QuickXor for OneDrive,
    /// MD5 for Google Drive). Absent for folders and provider-native
    /// documents that have no downloadable content.
    pub hash: Option<String>,
    /// RFC 3339 timestamp as reported by the provider.
    pub modified: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Change {
    Upsert(RemoteItem),
    Delete { id: String },
}

impl Change {
    pub fn item_id(&self) -> &str {
        match self {
            Self::Upsert(item) => &item.id,
            Self::Delete { id } => id,
        }
    }
}

/// One complete batch of remote changes.
#[derive(Debug)]
pub struct ChangeSet {
    pub changes: Vec<Change>,
    /// Opaque position to pass to the next [`Provider::changes`] call. Store
    /// it only together with the applied changes.
    pub cursor: String,
    /// The set lists every item in the drive rather than changes since a
    /// cursor, so anything it does not mention no longer exists remotely.
    pub full: bool,
}

/// Receives the sign-in URL so it can be shown if no browser opens.
pub type UrlCallback = dyn for<'a> Fn(&'a str) + Send + Sync;

/// Receives the number of entries fetched so far during a long enumeration.
pub type ProgressCallback = dyn Fn(usize) + Send + Sync;

/// A finished download: what was written and its hash as computed locally.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Downloaded {
    pub bytes: u64,
    pub hash: String,
}

#[async_trait]
pub trait Provider: Send + Sync {
    fn kind(&self) -> ProviderKind;

    /// Interactive sign-in through the browser.
    async fn sign_in(&self, on_url: &UrlCallback) -> Result<()>;

    async fn sign_out(&self) -> Result<()>;

    /// Whether a session is stored locally. Does not contact the provider.
    async fn is_signed_in(&self) -> Result<bool>;

    async fn account(&self) -> Result<Account>;

    /// Remote changes since `cursor`, or the whole drive when `cursor` is
    /// `None`. If the provider rejects the cursor, the implementation falls
    /// back to a full enumeration and says so through [`ChangeSet::full`].
    async fn changes(&self, cursor: Option<&str>, progress: &ProgressCallback)
    -> Result<ChangeSet>;

    /// The algorithm behind [`RemoteItem::hash`] for this provider.
    fn hash_kind(&self) -> HashKind;

    /// Write an item's content to `dest`, replacing it. The caller compares
    /// the returned hash with the one it expects; this only transfers.
    async fn download(&self, item_id: &str, dest: &Path) -> Result<Downloaded>;
}

/// Collapse changes to one entry per item. An item can appear more than once
/// in a set and only its last occurrence is authoritative.
pub fn latest_per_item(changes: Vec<Change>) -> Vec<Change> {
    let mut slot_by_id = std::collections::HashMap::new();
    let mut latest: Vec<Option<Change>> = Vec::with_capacity(changes.len());
    for change in changes {
        if let Some(previous) = slot_by_id.insert(change.item_id().to_owned(), latest.len()) {
            latest[previous] = None;
        }
        latest.push(Some(change));
    }
    latest.into_iter().flatten().collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn provider_ids_round_trip() {
        for kind in ProviderKind::ALL {
            assert_eq!(kind.id().parse::<ProviderKind>(), Ok(kind));
        }
        assert!("dropbox".parse::<ProviderKind>().is_err());
    }

    #[test]
    fn providers_have_distinct_folders() {
        assert_eq!(ProviderKind::OneDrive.folder_name(), "OneDrive");
        assert_eq!(ProviderKind::GoogleDrive.folder_name(), "Google Drive");
    }

    #[test]
    fn last_occurrence_of_an_item_wins() {
        let delete = |id: &str| Change::Delete { id: id.to_owned() };
        let upsert = |id: &str| {
            Change::Upsert(RemoteItem {
                id: id.to_owned(),
                parent_id: Some("root".to_owned()),
                name: id.to_owned(),
                is_folder: false,
                size: None,
                version: None,
                hash: None,
                modified: None,
            })
        };
        assert_eq!(
            latest_per_item(vec![upsert("1"), upsert("2"), delete("1")]),
            vec![upsert("2"), delete("1")]
        );
    }
}

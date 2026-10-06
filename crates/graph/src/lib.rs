//! Microsoft Graph access for personal OneDrive accounts: sign-in, token
//! handling, and the small set of REST calls the sync engine needs.

pub mod auth;
pub mod client;
pub mod delta;
pub mod error;
pub mod model;
pub mod token_store;

pub use auth::Authenticator;
pub use client::GraphClient;
pub use error::{Error, Result};

/// Sent on every request. Microsoft prioritises traffic that identifies
/// itself in this format.
pub const USER_AGENT: &str = concat!("NONISV|odl|odl/", env!("CARGO_PKG_VERSION"));

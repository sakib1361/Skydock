use reqwest::StatusCode;

pub type Result<T> = std::result::Result<T, Error>;

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("not signed in")]
    NotSignedIn,

    #[error("sign-in failed: {0}")]
    SignIn(String),

    #[error("sign-in timed out waiting for the browser")]
    SignInTimeout,

    /// The refresh token was rejected; the user has to sign in again.
    #[error("session expired, sign in again: {0}")]
    SessionExpired(String),

    /// The provider has no client ID (or other required setting).
    #[error("{0} is not configured")]
    NotConfigured(String),

    #[error("the service returned {status}: {body}")]
    Api { status: StatusCode, body: String },

    /// The remote content is no longer the one the caller started from.
    #[error("the item was changed by someone else")]
    Conflict,

    #[error("still throttled after {0} attempts")]
    Throttled(u32),

    #[error(transparent)]
    Http(#[from] reqwest::Error),

    #[error(transparent)]
    Io(#[from] std::io::Error),

    #[error(transparent)]
    Json(#[from] serde_json::Error),

    #[error("keyring: {0}")]
    Keyring(String),
}

impl Error {
    /// HTTP status of the provider's answer, if this is one.
    pub fn status(&self) -> Option<u16> {
        match self {
            Self::Api { status, .. } => Some(status.as_u16()),
            _ => None,
        }
    }

    /// The provider does not have the item the request named.
    pub fn is_not_found(&self) -> bool {
        self.status() == Some(404)
    }
}

// Kept as text: `oo7::Error` is large enough to bloat every `Result`.
impl From<oo7::Error> for Error {
    fn from(error: oo7::Error) -> Self {
        Self::Keyring(error.to_string())
    }
}

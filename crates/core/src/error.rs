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

// Kept as text: `oo7::Error` is large enough to bloat every `Result`.
impl From<oo7::Error> for Error {
    fn from(error: oo7::Error) -> Self {
        Self::Keyring(error.to_string())
    }
}

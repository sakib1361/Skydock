//! OAuth 2.0 authorization code flow with PKCE for a desktop client. The
//! browser redirects to a loopback listener. Shared by every provider; the
//! provider supplies endpoints and scopes through [`OAuthConfig`].

use std::collections::HashMap;
use std::io::Read;
use std::net::{Ipv4Addr, Ipv6Addr};
use std::sync::{Arc, OnceLock};
use std::time::{Duration, Instant};

use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use serde::Deserialize;
use sha2::{Digest, Sha256};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::Mutex;
use url::Url;

use crate::http::send_with_retry;
use crate::token_store::TokenStore;
use crate::{Error, ProviderKind, Result, USER_AGENT, UrlCallback};

const SIGN_IN_TIMEOUT: Duration = Duration::from_secs(300);
/// Refresh this long before the access token actually expires.
const EXPIRY_MARGIN: Duration = Duration::from_secs(60);

#[derive(Clone)]
pub struct OAuthConfig {
    pub provider: ProviderKind,
    pub authorize_url: &'static str,
    pub token_url: &'static str,
    pub client_id: String,
    /// Some providers issue a "secret" even to desktop apps and require it
    /// at the token endpoint. It is not confidential there.
    pub client_secret: Option<String>,
    pub scopes: &'static str,
    /// Host part of the loopback redirect URI as the provider expects it,
    /// `localhost` or `127.0.0.1`.
    pub redirect_host: &'static str,
    pub extra_authorize_params: &'static [(&'static str, &'static str)],
}

struct AccessToken {
    value: String,
    expires_at: Instant,
}

#[derive(Deserialize)]
struct TokenResponse {
    access_token: String,
    expires_in: u64,
    refresh_token: Option<String>,
}

#[derive(Deserialize)]
struct TokenError {
    error: String,
    #[serde(default)]
    error_description: String,
}

pub struct Authenticator {
    http: reqwest::Client,
    config: OAuthConfig,
    store: TokenStore,
    cached: Session,
}

/// The access token of one sign-in, and the lock that makes refreshing it
/// one at a time.
type Session = Arc<Mutex<Option<AccessToken>>>;

/// One [`Session`] per provider and client for the whole process. Front
/// ends create an authenticator per operation; if each kept its own token
/// they would all refresh, and race each other on the keyring entry.
fn shared_session(provider: ProviderKind, client_id: &str) -> Session {
    static SESSIONS: OnceLock<std::sync::Mutex<HashMap<(ProviderKind, String), Session>>> =
        OnceLock::new();
    SESSIONS
        .get_or_init(Default::default)
        .lock()
        .unwrap()
        .entry((provider, client_id.to_owned()))
        .or_default()
        .clone()
}

impl Authenticator {
    pub fn new(config: OAuthConfig) -> Result<Self> {
        Ok(Self {
            http: reqwest::Client::builder().user_agent(USER_AGENT).build()?,
            store: TokenStore::new(config.provider, config.client_id.clone()),
            cached: shared_session(config.provider, &config.client_id),
            config,
        })
    }

    pub(crate) fn http(&self) -> &reqwest::Client {
        &self.http
    }

    /// Whether a session is stored. It may still turn out to be revoked.
    pub async fn is_signed_in(&self) -> Result<bool> {
        Ok(self.store.load().await?.is_some())
    }

    /// Interactive sign-in through the system browser. `on_url` receives the
    /// authorization URL so the caller can show it in case no browser opens.
    pub async fn sign_in(&self, on_url: &UrlCallback) -> Result<()> {
        let config = &self.config;
        let listeners = LoopbackListeners::bind().await?;
        let redirect_uri = format!("http://{}:{}", config.redirect_host, listeners.port);
        let verifier = random_token()?;
        let state = random_token()?;
        let challenge = pkce_challenge(&verifier);

        let mut params = vec![
            ("client_id", config.client_id.as_str()),
            ("response_type", "code"),
            ("redirect_uri", &redirect_uri),
            ("scope", config.scopes),
            ("state", &state),
            ("code_challenge", &challenge),
            ("code_challenge_method", "S256"),
        ];
        params.extend_from_slice(config.extra_authorize_params);
        let url = Url::parse_with_params(config.authorize_url, &params)
            .expect("provider authorize URL is valid");

        on_url(url.as_str());
        // Best effort: the URL has already been handed to the caller.
        let _ = tokio::process::Command::new("xdg-open")
            .arg(url.as_str())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn();

        let code = tokio::time::timeout(SIGN_IN_TIMEOUT, listeners.wait_for_code(&state))
            .await
            .map_err(|_| Error::SignInTimeout)??;

        let tokens = self
            .request_tokens(&[
                ("grant_type", "authorization_code"),
                ("code", &code),
                ("redirect_uri", &redirect_uri),
                ("code_verifier", &verifier),
            ])
            .await
            .map_err(|e| match e {
                Error::SessionExpired(msg) => Error::SignIn(msg),
                other => other,
            })?;
        let mut cached = self.cached.lock().await;
        self.store_tokens(&mut cached, tokens).await
    }

    pub async fn sign_out(&self) -> Result<()> {
        *self.cached.lock().await = None;
        self.store.clear().await
    }

    /// A valid access token, refreshed if it is missing or about to expire.
    pub async fn access_token(&self) -> Result<String> {
        let mut cached = self.cached.lock().await;
        if let Some(token) = cached.as_ref()
            && Instant::now() + EXPIRY_MARGIN < token.expires_at
        {
            return Ok(token.value.clone());
        }
        // Hold the lock across the refresh so concurrent callers do not each
        // spend the refresh token.
        let refresh_token = self.store.load().await?.ok_or(Error::NotSignedIn)?;
        let tokens = self
            .request_tokens(&[
                ("grant_type", "refresh_token"),
                ("refresh_token", &refresh_token),
            ])
            .await?;
        let value = tokens.access_token.clone();
        self.store_tokens(&mut cached, tokens).await?;
        Ok(value)
    }

    /// Drop the cached access token, forcing a refresh on next use. Called
    /// when the API answers 401 for a token we believed valid.
    pub async fn invalidate(&self) {
        *self.cached.lock().await = None;
    }

    async fn store_tokens(
        &self,
        cached: &mut Option<AccessToken>,
        tokens: TokenResponse,
    ) -> Result<()> {
        // A response may rotate the refresh token; the old one must then be
        // replaced or the session eventually dies.
        if let Some(refresh_token) = &tokens.refresh_token {
            self.store.save(refresh_token).await?;
        }
        *cached = Some(AccessToken {
            value: tokens.access_token,
            expires_at: Instant::now() + Duration::from_secs(tokens.expires_in),
        });
        Ok(())
    }

    async fn request_tokens(&self, grant: &[(&str, &str)]) -> Result<TokenResponse> {
        let config = &self.config;
        let mut form = vec![
            ("client_id", config.client_id.as_str()),
            ("scope", config.scopes),
        ];
        if let Some(secret) = &config.client_secret {
            form.push(("client_secret", secret));
        }
        form.extend_from_slice(grant);

        let response = send_with_retry(|| self.http.post(config.token_url).form(&form)).await?;
        let status = response.status();
        let body = response.text().await?;
        if status.is_success() {
            return Ok(serde_json::from_str(&body)?);
        }
        match serde_json::from_str::<TokenError>(&body) {
            // Anything the identity service reports at 400 means this grant
            // is unusable; only the user can fix it by signing in again.
            Ok(err) if status == reqwest::StatusCode::BAD_REQUEST => Err(Error::SessionExpired(
                format!("{}: {}", err.error, first_line(&err.error_description)),
            )),
            _ => Err(Error::Api { status, body }),
        }
    }
}

fn first_line(s: &str) -> &str {
    s.lines().next().unwrap_or_default()
}

/// 32 random bytes as base64url: 43 characters, valid as both a PKCE
/// verifier and a `state` value.
fn random_token() -> std::io::Result<String> {
    let mut bytes = [0u8; 32];
    std::fs::File::open("/dev/urandom")?.read_exact(&mut bytes)?;
    Ok(URL_SAFE_NO_PAD.encode(bytes))
}

fn pkce_challenge(verifier: &str) -> String {
    URL_SAFE_NO_PAD.encode(Sha256::digest(verifier.as_bytes()))
}

/// Browsers may resolve `localhost` to either address family, so listen on
/// both where possible.
struct LoopbackListeners {
    v4: TcpListener,
    v6: Option<TcpListener>,
    port: u16,
}

impl LoopbackListeners {
    async fn bind() -> Result<Self> {
        let v4 = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).await?;
        let port = v4.local_addr()?.port();
        let v6 = TcpListener::bind((Ipv6Addr::LOCALHOST, port)).await.ok();
        Ok(Self { v4, v6, port })
    }

    async fn accept(&self) -> std::io::Result<TcpStream> {
        let (stream, _) = match &self.v6 {
            Some(v6) => tokio::select! {
                conn = self.v4.accept() => conn?,
                conn = v6.accept() => conn?,
            },
            None => self.v4.accept().await?,
        };
        Ok(stream)
    }

    async fn wait_for_code(&self, expected_state: &str) -> Result<String> {
        loop {
            let mut stream = self.accept().await?;
            let Some(request_line) = read_request_line(&mut stream).await else {
                continue;
            };
            match parse_callback(&request_line, expected_state) {
                Callback::Code(code) => {
                    respond(&mut stream, "200 OK", "Signed in. You can close this tab.").await;
                    return Ok(code);
                }
                Callback::Failed(reason) => {
                    respond(&mut stream, "400 Bad Request", "Sign-in failed.").await;
                    return Err(Error::SignIn(reason));
                }
                // Favicon requests and stray local connections.
                Callback::Unrelated => respond(&mut stream, "404 Not Found", "Not found.").await,
            }
        }
    }
}

async fn read_request_line(stream: &mut TcpStream) -> Option<String> {
    let mut buf = Vec::new();
    let mut chunk = [0u8; 1024];
    while !buf.contains(&b'\n') && buf.len() < 16 * 1024 {
        let read = tokio::time::timeout(Duration::from_secs(5), stream.read(&mut chunk))
            .await
            .ok()?
            .ok()?;
        if read == 0 {
            break;
        }
        buf.extend_from_slice(&chunk[..read]);
    }
    let line = buf.split(|b| *b == b'\n').next()?;
    Some(String::from_utf8_lossy(line).trim_end().to_owned())
}

async fn respond(stream: &mut TcpStream, status: &str, message: &str) {
    let body = format!("<!doctype html><meta charset=utf-8><title>skydock</title><p>{message}</p>");
    let response = format!(
        "HTTP/1.1 {status}\r\nContent-Type: text/html; charset=utf-8\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    );
    let _ = stream.write_all(response.as_bytes()).await;
    let _ = stream.shutdown().await;
}

#[derive(Debug, PartialEq)]
enum Callback {
    Code(String),
    Failed(String),
    Unrelated,
}

fn parse_callback(request_line: &str, expected_state: &str) -> Callback {
    let mut parts = request_line.split(' ');
    let (Some("GET"), Some(target)) = (parts.next(), parts.next()) else {
        return Callback::Unrelated;
    };
    let Some(query) = target.strip_prefix("/?") else {
        return Callback::Unrelated;
    };

    let (mut code, mut state, mut error, mut description) = (None, None, None, None);
    for (key, value) in url::form_urlencoded::parse(query.as_bytes()) {
        match key.as_ref() {
            "code" => code = Some(value.into_owned()),
            "state" => state = Some(value.into_owned()),
            "error" => error = Some(value.into_owned()),
            "error_description" => description = Some(value.into_owned()),
            _ => {}
        }
    }

    if let Some(error) = error {
        let description = description.unwrap_or_default();
        return Callback::Failed(format!("{error}: {}", first_line(&description)));
    }
    match code {
        None => Callback::Unrelated,
        // A code without our state did not come from the request we started.
        Some(_) if state.as_deref() != Some(expected_state) => {
            Callback::Failed("state mismatch in sign-in response".to_owned())
        }
        Some(code) => Callback::Code(code),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pkce_challenge_matches_rfc7636_example() {
        assert_eq!(
            pkce_challenge("dBjftJeZ4CVP-mB92K27uhbUJU1p1r_wW1gFWFOEjXk"),
            "E9Melhoa2OwvFrEMTJguCHaoeK1t8URWbuGJSstw-cM"
        );
    }

    #[test]
    fn random_tokens_are_verifier_sized_and_distinct() {
        let (a, b) = (random_token().unwrap(), random_token().unwrap());
        assert_eq!(a.len(), 43);
        assert_ne!(a, b);
    }

    #[test]
    fn callback_with_matching_state_yields_code() {
        assert_eq!(
            parse_callback("GET /?code=M.C5%21abc&state=s1 HTTP/1.1", "s1"),
            Callback::Code("M.C5!abc".to_owned())
        );
    }

    #[test]
    fn callback_with_wrong_or_missing_state_is_rejected() {
        for line in [
            "GET /?code=abc&state=other HTTP/1.1",
            "GET /?code=abc HTTP/1.1",
        ] {
            assert!(matches!(parse_callback(line, "s1"), Callback::Failed(_)));
        }
    }

    #[test]
    fn callback_error_is_reported() {
        assert_eq!(
            parse_callback(
                "GET /?error=access_denied&error_description=The+user+denied&state=s1 HTTP/1.1",
                "s1"
            ),
            Callback::Failed("access_denied: The user denied".to_owned())
        );
    }

    #[tokio::test]
    async fn listener_skips_stray_requests_then_returns_the_code() {
        let listeners = LoopbackListeners::bind().await.unwrap();
        let port = listeners.port;
        let browser = tokio::spawn(async move {
            let mut replies = Vec::new();
            for target in ["/favicon.ico", "/?code=abc&state=s1"] {
                let mut stream = TcpStream::connect((Ipv4Addr::LOCALHOST, port))
                    .await
                    .unwrap();
                let request = format!("GET {target} HTTP/1.1\r\nHost: localhost\r\n\r\n");
                stream.write_all(request.as_bytes()).await.unwrap();
                let mut reply = String::new();
                stream.read_to_string(&mut reply).await.unwrap();
                replies.push(reply);
            }
            replies
        });

        assert_eq!(listeners.wait_for_code("s1").await.unwrap(), "abc");
        let replies = browser.await.unwrap();
        assert!(replies[0].starts_with("HTTP/1.1 404"));
        assert!(replies[1].starts_with("HTTP/1.1 200"));
    }

    #[test]
    fn unrelated_requests_are_ignored() {
        for line in [
            "GET /favicon.ico HTTP/1.1",
            "POST /?code=abc&state=s1 HTTP/1.1",
            "GET / HTTP/1.1",
            "",
        ] {
            assert_eq!(parse_callback(line, "s1"), Callback::Unrelated);
        }
    }
}

//! OAuth 2.0 authorization code flow with PKCE for a public desktop client.
//! The browser redirects to a loopback listener; no client secret exists.

use std::io::Read;
use std::net::{Ipv4Addr, Ipv6Addr};
use std::time::{Duration, Instant};

use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use serde::Deserialize;
use sha2::{Digest, Sha256};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::Mutex;
use url::Url;

use crate::token_store::TokenStore;
use crate::{Error, Result, USER_AGENT};

// v1 is personal accounts only, hence the `consumers` tenant.
const AUTHORIZE_URL: &str = "https://login.microsoftonline.com/consumers/oauth2/v2.0/authorize";
const TOKEN_URL: &str = "https://login.microsoftonline.com/consumers/oauth2/v2.0/token";
const SCOPES: &str = "Files.ReadWrite User.Read offline_access";

const SIGN_IN_TIMEOUT: Duration = Duration::from_secs(300);
/// Refresh this long before the access token actually expires.
const EXPIRY_MARGIN: Duration = Duration::from_secs(60);

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
    client_id: String,
    store: TokenStore,
    cached: Mutex<Option<AccessToken>>,
}

impl Authenticator {
    pub fn new(client_id: impl Into<String>) -> Result<Self> {
        let client_id = client_id.into();
        Ok(Self {
            http: reqwest::Client::builder().user_agent(USER_AGENT).build()?,
            store: TokenStore::new(client_id.clone()),
            client_id,
            cached: Mutex::new(None),
        })
    }

    pub(crate) fn http(&self) -> &reqwest::Client {
        &self.http
    }

    /// Interactive sign-in through the system browser. `on_url` receives the
    /// authorization URL so the caller can show it in case no browser opens.
    pub async fn sign_in(&self, on_url: impl FnOnce(&str)) -> Result<()> {
        let listeners = LoopbackListeners::bind().await?;
        let redirect_uri = format!("http://localhost:{}", listeners.port);
        let verifier = random_token()?;
        let state = random_token()?;

        let url = Url::parse_with_params(
            AUTHORIZE_URL,
            &[
                ("client_id", self.client_id.as_str()),
                ("response_type", "code"),
                ("redirect_uri", &redirect_uri),
                ("response_mode", "query"),
                ("scope", SCOPES),
                ("state", &state),
                ("code_challenge", &pkce_challenge(&verifier)),
                ("code_challenge_method", "S256"),
                ("prompt", "select_account"),
            ],
        )
        .expect("static authorize URL is valid");

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
                ("client_id", self.client_id.as_str()),
                ("grant_type", "authorization_code"),
                ("code", &code),
                ("redirect_uri", &redirect_uri),
                ("code_verifier", &verifier),
                ("scope", SCOPES),
            ])
            .await
            .map_err(|e| match e {
                Error::SessionExpired(msg) => Error::SignIn(msg),
                other => other,
            })?;
        self.accept(tokens).await
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
                ("client_id", self.client_id.as_str()),
                ("grant_type", "refresh_token"),
                ("refresh_token", &refresh_token),
                ("scope", SCOPES),
            ])
            .await?;
        let value = tokens.access_token.clone();
        self.store_tokens(&mut cached, tokens).await?;
        Ok(value)
    }

    /// Drop the cached access token, forcing a refresh on next use. Called
    /// when Graph answers 401 for a token we believed valid.
    pub async fn invalidate(&self) {
        *self.cached.lock().await = None;
    }

    async fn accept(&self, tokens: TokenResponse) -> Result<()> {
        let mut cached = self.cached.lock().await;
        self.store_tokens(&mut cached, tokens).await
    }

    async fn store_tokens(
        &self,
        cached: &mut Option<AccessToken>,
        tokens: TokenResponse,
    ) -> Result<()> {
        // Each response may rotate the refresh token; the old one must be
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

    async fn request_tokens(&self, form: &[(&str, &str)]) -> Result<TokenResponse> {
        let response = self.http.post(TOKEN_URL).form(form).send().await?;
        let status = response.status();
        let body = response.text().await?;
        if status.is_success() {
            return Ok(serde_json::from_str(&body)?);
        }
        match serde_json::from_str::<TokenError>(&body) {
            // Anything the identity platform reports at 400 means this grant
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
    let body = format!("<!doctype html><meta charset=utf-8><title>odl</title><p>{message}</p>");
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

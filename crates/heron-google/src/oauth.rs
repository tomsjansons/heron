//! OAuth 2.0 for installed apps: the authorization URL with PKCE, the token
//! endpoint, and access tokens refreshed on demand from the stored refresh
//! token. Tokens are never logged.

use std::fmt;
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use base64::Engine as _;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use heron_core::config::GoogleClient;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use tokio::sync::Mutex;
use url::Url;

pub const AUTH_URL: &str = "https://accounts.google.com/o/oauth2/v2/auth";
pub const TOKEN_URL: &str = "https://oauth2.googleapis.com/token";
pub const REVOKE_URL: &str = "https://oauth2.googleapis.com/revoke";
pub const USERINFO_URL: &str = "https://openidconnect.googleapis.com/v1/userinfo";

/// Full IMAP access; Gmail offers no narrower scope that works over IMAP.
pub const MAIL_SCOPE: &str = "https://mail.google.com/";
pub const CALENDAR_SCOPE: &str = "https://www.googleapis.com/auth/calendar.readonly";
pub const SCOPES: [&str; 4] = [MAIL_SCOPE, CALENDAR_SCOPE, "openid", "email"];

/// What `heron auth google` stores under [`secret_name`].
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Stored {
    pub refresh_token: String,
    /// Scopes the user actually granted, which may be fewer than requested.
    pub scopes: Vec<String>,
    pub email: String,
}

impl Stored {
    pub fn has(&self, scope: &str) -> bool {
        self.scopes.iter().any(|s| s == scope)
    }
}

pub fn secret_name(account_id: &str) -> String {
    format!("google-{account_id}")
}

/// The stored token no longer works, or lacks a scope; the user has to run
/// `heron auth google`. Carries a short reason for the source status.
#[derive(Debug)]
pub struct NeedsAuth(pub String);

impl fmt::Display for NeedsAuth {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for NeedsAuth {}

/// Finds a [`NeedsAuth`] anywhere in the error's context chain.
pub fn needs_auth(err: &anyhow::Error) -> Option<&NeedsAuth> {
    err.downcast_ref::<NeedsAuth>()
}

/// Random URL-safe string from `bytes` bytes of entropy.
pub fn random_token(bytes: usize) -> String {
    let mut buf = vec![0u8; bytes];
    rand::fill(&mut buf[..]);
    URL_SAFE_NO_PAD.encode(buf)
}

/// Proof Key for Code Exchange with the S256 method (RFC 7636).
pub struct Pkce {
    pub verifier: String,
    pub challenge: String,
}

impl Pkce {
    pub fn new() -> Self {
        // 48 bytes encode to 64 characters; RFC 7636 allows 43 to 128.
        Self::from_verifier(random_token(48))
    }

    pub fn from_verifier(verifier: String) -> Self {
        let challenge = URL_SAFE_NO_PAD.encode(Sha256::digest(verifier.as_bytes()));
        Self { verifier, challenge }
    }
}

/// The consent page URL. `prompt=consent` makes Google return a refresh token
/// even when the user granted access before.
pub fn authorize_url(client_id: &str, redirect_uri: &str, challenge: &str, state: &str, login_hint: &str) -> Url {
    let mut url = Url::parse(AUTH_URL).expect("valid constant URL");
    url.query_pairs_mut()
        .append_pair("client_id", client_id)
        .append_pair("redirect_uri", redirect_uri)
        .append_pair("response_type", "code")
        .append_pair("scope", &SCOPES.join(" "))
        .append_pair("code_challenge", challenge)
        .append_pair("code_challenge_method", "S256")
        .append_pair("state", state)
        .append_pair("access_type", "offline")
        .append_pair("prompt", "consent")
        .append_pair("include_granted_scopes", "true")
        .append_pair("login_hint", login_hint);
    url
}

#[derive(Debug, Deserialize)]
pub struct TokenResponse {
    pub access_token: String,
    #[serde(default = "default_expiry")]
    pub expires_in: u64,
    pub refresh_token: Option<String>,
    /// Space-separated granted scopes.
    #[serde(default)]
    pub scope: String,
    pub id_token: Option<String>,
}

fn default_expiry() -> u64 {
    3600
}

impl TokenResponse {
    pub fn scopes(&self) -> Vec<String> {
        self.scope.split_whitespace().map(str::to_owned).collect()
    }
}

/// An error answer from Google's token endpoint, with advice for the codes a
/// user can do something about.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct OAuthError {
    pub error: String,
    pub error_description: Option<String>,
}

impl fmt::Display for OAuthError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let detail = self.error_description.as_deref().map(|d| format!(": {d}")).unwrap_or_default();
        match self.error.as_str() {
            "invalid_client" | "unauthorized_client" => write!(
                f,
                "Google rejected the OAuth client ({}{detail}). Check client_id and client_secret under \
                 [google_client] in config.toml. The client must be of type \"Desktop app\": Google Cloud \
                 console → APIs & Services → Credentials → Create credentials → OAuth client ID → Desktop app",
                self.error
            ),
            "invalid_grant" => write!(
                f,
                "Google refused the token (invalid_grant{detail}). It was revoked, has expired, or the \
                 account password changed. Apps whose OAuth consent screen is in \"Testing\" get refresh \
                 tokens that expire after 7 days; set the publishing status to \"In production\" to keep \
                 logins working"
            ),
            "redirect_uri_mismatch" => write!(
                f,
                "Google rejected the redirect address (redirect_uri_mismatch{detail}). Use a \"Desktop app\" \
                 OAuth client; \"Web application\" clients do not accept loopback redirects"
            ),
            other => write!(f, "Google token endpoint: {other}{detail}"),
        }
    }
}

impl std::error::Error for OAuthError {}

/// Parses the token endpoint's answer: a token on success, an [`OAuthError`] otherwise.
pub fn parse_token_response(status: u16, body: &[u8]) -> Result<TokenResponse> {
    if (200..300).contains(&status) {
        return serde_json::from_slice(body).context("unexpected token response from Google");
    }
    match serde_json::from_slice::<OAuthError>(body) {
        Ok(err) => Err(err.into()),
        Err(_) => anyhow::bail!("Google token endpoint answered HTTP {status}"),
    }
}

pub fn http_client() -> Result<reqwest::Client> {
    reqwest::Client::builder()
        .user_agent(concat!("heron/", env!("CARGO_PKG_VERSION")))
        .timeout(Duration::from_secs(30))
        .build()
        .context("building HTTP client")
}

async fn token_request(http: &reqwest::Client, form: &[(&str, &str)]) -> Result<TokenResponse> {
    let response = http.post(TOKEN_URL).form(form).send().await.context("contacting Google")?;
    let status = response.status().as_u16();
    let body = response.bytes().await.context("reading Google's answer")?;
    parse_token_response(status, &body)
}

pub async fn exchange_code(
    http: &reqwest::Client,
    client: &GoogleClient,
    code: &str,
    verifier: &str,
    redirect_uri: &str,
) -> Result<TokenResponse> {
    token_request(http, &[
        ("grant_type", "authorization_code"),
        ("code", code),
        ("code_verifier", verifier),
        ("redirect_uri", redirect_uri),
        ("client_id", &client.client_id),
        ("client_secret", &client.client_secret),
    ])
    .await
}

pub async fn refresh(http: &reqwest::Client, client: &GoogleClient, refresh_token: &str) -> Result<TokenResponse> {
    token_request(http, &[
        ("grant_type", "refresh_token"),
        ("refresh_token", refresh_token),
        ("client_id", &client.client_id),
        ("client_secret", &client.client_secret),
    ])
    .await
}

/// Revokes a refresh or access token, and with it the user's grant.
pub async fn revoke(http: &reqwest::Client, token: &str) -> Result<()> {
    let response = http.post(REVOKE_URL).form(&[("token", token)]).send().await.context("contacting Google")?;
    let status = response.status();
    if status.is_success() {
        return Ok(());
    }
    let body = response.bytes().await.unwrap_or_default();
    match serde_json::from_slice::<OAuthError>(&body) {
        // Already revoked or expired: nothing left to do.
        Ok(err) if err.error == "invalid_token" => Ok(()),
        Ok(err) => Err(err.into()),
        Err(_) => anyhow::bail!("revoke answered HTTP {status}"),
    }
}

/// The `email` claim of an ID token. The token came straight from Google's
/// token endpoint over TLS, so its signature need not be checked.
pub fn id_token_email(id_token: &str) -> Result<String> {
    #[derive(Deserialize)]
    struct Claims {
        email: Option<String>,
    }
    let payload = id_token.split('.').nth(1).context("malformed ID token")?;
    let bytes = URL_SAFE_NO_PAD.decode(payload.trim_end_matches('=')).context("malformed ID token")?;
    let claims: Claims = serde_json::from_slice(&bytes).context("malformed ID token")?;
    claims.email.context("ID token has no email claim")
}

/// The signed-in address, from the ID token or else the userinfo endpoint.
pub async fn signed_in_email(http: &reqwest::Client, token: &TokenResponse) -> Result<String> {
    if let Some(email) = token.id_token.as_deref().and_then(|t| id_token_email(t).ok()) {
        return Ok(email);
    }
    #[derive(Deserialize)]
    struct UserInfo {
        email: String,
    }
    let info: UserInfo = http.get(USERINFO_URL).bearer_auth(&token.access_token).send().await?
        .error_for_status()?
        .json().await?;
    Ok(info.email)
}

/// Hands out access tokens, refreshing shortly before they expire.
pub struct Tokens {
    http: reqwest::Client,
    client: GoogleClient,
    refresh_token: String,
    account_id: String,
    cached: Mutex<Option<(String, Instant)>>,
}

impl Tokens {
    pub fn new(http: reqwest::Client, client: GoogleClient, refresh_token: String, account_id: String) -> Self {
        Self { http, client, refresh_token, account_id, cached: Mutex::new(None) }
    }

    /// A valid access token. Fails with [`NeedsAuth`] when Google refuses the
    /// refresh token.
    pub async fn access_token(&self) -> Result<String> {
        let mut cached = self.cached.lock().await;
        if let Some((token, expires)) = cached.as_ref()
            && Instant::now() + Duration::from_secs(60) < *expires
        {
            return Ok(token.clone());
        }
        let response = match refresh(&self.http, &self.client, &self.refresh_token).await {
            Ok(response) => response,
            Err(err) => {
                if let Some(oauth) = err.downcast_ref::<OAuthError>()
                    && oauth.error == "invalid_grant"
                {
                    tracing::warn!("google/{}: {oauth}", self.account_id);
                    return Err(NeedsAuth(format!(
                        "login expired or revoked; run `heron auth google {}`",
                        self.account_id
                    ))
                    .into());
                }
                return Err(err.context("refreshing the Google access token"));
            }
        };
        let expires = Instant::now() + Duration::from_secs(response.expires_in);
        *cached = Some((response.access_token.clone(), expires));
        Ok(response.access_token)
    }

    /// Drops the cached access token after the server rejected it.
    pub async fn invalidate(&self) {
        *self.cached.lock().await = None;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pkce_rfc7636_vector() {
        // Appendix B of RFC 7636.
        let pkce = Pkce::from_verifier("dBjftJeZ4CVP-mB92K27uhbUJU1p1r_wW1gFWFOEjXk".into());
        assert_eq!(pkce.challenge, "E9Melhoa2OwvFrEMTJguCHaoeK1t8URWbuGJSstw-cM");
    }

    #[test]
    fn pkce_verifier_length() {
        let pkce = Pkce::new();
        assert!((43..=128).contains(&pkce.verifier.len()));
        assert!(pkce.verifier.chars().all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_'));
    }

    #[test]
    fn auth_url_has_every_parameter() {
        let url = authorize_url("id.apps.googleusercontent.com", "http://127.0.0.1:4242", "chal", "st", "me@gmail.com");
        assert!(url.as_str().starts_with(AUTH_URL));
        let pairs: std::collections::HashMap<_, _> = url.query_pairs().into_owned().collect();
        assert_eq!(pairs["client_id"], "id.apps.googleusercontent.com");
        assert_eq!(pairs["redirect_uri"], "http://127.0.0.1:4242");
        assert_eq!(pairs["response_type"], "code");
        assert_eq!(
            pairs["scope"],
            "https://mail.google.com/ https://www.googleapis.com/auth/calendar.readonly openid email"
        );
        assert_eq!(pairs["code_challenge"], "chal");
        assert_eq!(pairs["code_challenge_method"], "S256");
        assert_eq!(pairs["state"], "st");
        assert_eq!(pairs["access_type"], "offline");
        assert_eq!(pairs["prompt"], "consent");
        assert_eq!(pairs["login_hint"], "me@gmail.com");
    }

    #[test]
    fn token_response_success() {
        let body = br#"{"access_token":"ya29.x","expires_in":3599,"refresh_token":"1//r",
            "scope":"https://www.googleapis.com/auth/calendar.readonly openid https://mail.google.com/ https://www.googleapis.com/auth/userinfo.email",
            "token_type":"Bearer","id_token":"a.b.c"}"#;
        let token = parse_token_response(200, body).unwrap();
        assert_eq!(token.access_token, "ya29.x");
        assert_eq!(token.expires_in, 3599);
        assert_eq!(token.refresh_token.as_deref(), Some("1//r"));
        let scopes = token.scopes();
        assert!(scopes.iter().any(|s| s == MAIL_SCOPE));
        assert!(scopes.iter().any(|s| s == CALENDAR_SCOPE));
    }

    #[test]
    fn token_response_errors() {
        let err = parse_token_response(401, br#"{"error":"invalid_client","error_description":"Unauthorized"}"#).unwrap_err();
        let oauth = err.downcast_ref::<OAuthError>().unwrap();
        assert_eq!(oauth.error, "invalid_client");
        assert!(err.to_string().contains("Desktop app"));

        let err = parse_token_response(400, br#"{"error":"invalid_grant","error_description":"Token has been expired or revoked."}"#).unwrap_err();
        assert!(err.to_string().contains("7 days"));

        let err = parse_token_response(502, b"<html>bad gateway</html>").unwrap_err();
        assert!(err.to_string().contains("502"));
    }

    #[test]
    fn email_from_id_token() {
        let payload = URL_SAFE_NO_PAD.encode(br#"{"iss":"https://accounts.google.com","email":"Me@Gmail.com","email_verified":true}"#);
        let token = format!("eyJhbGciOiJSUzI1NiJ9.{payload}.sig");
        assert_eq!(id_token_email(&token).unwrap(), "Me@Gmail.com");
        assert!(id_token_email("garbage").is_err());
    }

    #[test]
    fn needs_auth_survives_context() {
        let err = anyhow::Error::from(NeedsAuth("x".into())).context("outer").context("outermost");
        assert_eq!(needs_auth(&err).map(|n| n.0.as_str()), Some("x"));
    }
}

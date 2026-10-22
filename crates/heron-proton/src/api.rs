//! HTTP client for the Proton API: app-version and session headers, API error
//! codes, and access-token refresh on 401.
//!
//! Endpoints, headers and field names follow go-proton-api (the client used by
//! Proton Bridge) and the WebClients repository.

use std::fmt;

use anyhow::{Context, Result};
use reqwest::{Method, StatusCode};
use serde::de::DeserializeOwned;
use serde::{Deserialize, Deserializer, Serialize};
use serde_json::Value;
use tokio::sync::Mutex;
use tracing::{debug, warn};

pub const BASE_URL: &str = "https://mail.proton.me/api";

/// Sent as `x-pm-appversion`. Proton only issues app ids to its own clients;
/// third-party clients such as hydroxide and proton-core identify themselves as
/// "Other", which the API accepts. heron does the same rather than pretending
/// to be Bridge or the web client, and says who it is in the User-Agent.
pub const APP_VERSION: &str = "Other";

const USER_AGENT: &str = concat!("heron/", env!("CARGO_PKG_VERSION"), " (+https://github.com/tomsjansons/heron)");

/// API codes (go-proton-api `response.go`).
pub const CODE_OK: i64 = 1000;
pub const CODE_MULTI: i64 = 1001;
pub const CODE_WRONG_PASSWORD: i64 = 8002;
pub const CODE_HUMAN_VERIFICATION: i64 = 9001;
pub const CODE_REFRESH_TOKEN_INVALID: i64 = 10013;

/// What `run` needs from `heron auth proton`, stored as `proton-<id>.json`.
#[derive(Clone, Serialize, Deserialize)]
pub struct Session {
    pub uid: String,
    pub access_token: String,
    pub refresh_token: String,
    /// Salted mailbox password (bcrypt hash part) that unlocks the user keys.
    /// Never the account password itself.
    pub key_passphrase: Option<String>,
}

impl fmt::Debug for Session {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("Session { [redacted] }")
    }
}

/// A non-success response from the API.
#[derive(Debug, Clone)]
pub struct ApiError {
    pub status: u16,
    pub code: i64,
    pub message: String,
    pub details: Value,
}

impl fmt::Display for ApiError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{} (code {}, HTTP {})", self.message, self.code, self.status)
    }
}

impl std::error::Error for ApiError {}

/// The `Details` of a human-verification error (code 9001).
#[derive(Debug, Clone, Deserialize)]
pub struct HumanVerification {
    #[serde(rename = "HumanVerificationToken")]
    pub token: String,
    #[serde(rename = "HumanVerificationMethods", default)]
    pub methods: Vec<String>,
    #[serde(rename = "WebUrl")]
    pub web_url: Option<String>,
}

impl ApiError {
    pub fn human_verification(&self) -> Option<HumanVerification> {
        (self.code == CODE_HUMAN_VERIFICATION).then(|| serde_json::from_value(self.details.clone()).ok()).flatten()
    }
}

/// The stored session is gone or revoked; the user must run `heron auth proton`.
#[derive(Debug)]
pub struct NeedsAuth(pub String);

impl fmt::Display for NeedsAuth {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for NeedsAuth {}

pub fn needs_auth(err: &anyhow::Error) -> Option<&NeedsAuth> {
    err.downcast_ref::<NeedsAuth>()
}

pub fn api_error(err: &anyhow::Error) -> Option<&ApiError> {
    err.downcast_ref::<ApiError>()
}

/// Extra headers for one request, such as human verification.
pub type Headers = Vec<(&'static str, String)>;

pub struct Api {
    http: reqwest::Client,
    session: Mutex<Option<Session>>,
    /// Secret name the session is written back to after a refresh.
    store_as: Option<String>,
}

impl Api {
    pub fn new(session: Option<Session>, store_as: Option<String>) -> Result<Self> {
        let http = reqwest::Client::builder()
            .user_agent(USER_AGENT)
            .timeout(std::time::Duration::from_secs(60))
            .build()?;
        Ok(Self { http, session: Mutex::new(session), store_as })
    }

    pub async fn session(&self) -> Option<Session> {
        self.session.lock().await.clone()
    }

    pub async fn set_session(&self, session: Session) {
        *self.session.lock().await = Some(session);
    }

    pub async fn get<T: DeserializeOwned>(&self, path: &str, query: &[(&str, String)]) -> Result<T> {
        self.call(Method::GET, path, query, None, &[]).await
    }

    pub async fn post<T: DeserializeOwned>(&self, path: &str, body: &impl Serialize) -> Result<T> {
        self.call(Method::POST, path, &[], Some(serde_json::to_value(body)?), &[]).await
    }

    pub async fn put<T: DeserializeOwned>(&self, path: &str, body: &impl Serialize) -> Result<T> {
        self.call(Method::PUT, path, &[], Some(serde_json::to_value(body)?), &[]).await
    }

    pub async fn delete(&self, path: &str) -> Result<()> {
        self.call::<serde::de::IgnoredAny>(Method::DELETE, path, &[], None, &[]).await.map(drop)
    }

    /// One request, refreshing the access token and retrying once on 401.
    pub async fn call<T: DeserializeOwned>(
        &self,
        method: Method,
        path: &str,
        query: &[(&str, String)],
        body: Option<Value>,
        headers: &[(&'static str, String)],
    ) -> Result<T> {
        let used = self.session.lock().await.as_ref().map(|s| s.access_token.clone());
        match self.execute(method.clone(), path, query, body.as_ref(), headers).await {
            Err(err) if used.is_some() && api_error(&err).is_some_and(|e| e.status == 401) => {
                debug!("{path}: access token expired, refreshing");
                self.refresh(used.as_deref().unwrap_or_default()).await?;
                self.execute(method, path, query, body.as_ref(), headers).await
            }
            result => result,
        }
        .with_context(|| path.to_owned())
    }

    async fn execute<T: DeserializeOwned>(
        &self,
        method: Method,
        path: &str,
        query: &[(&str, String)],
        body: Option<&Value>,
        headers: &[(&'static str, String)],
    ) -> Result<T> {
        let mut request = self.http.request(method, format!("{BASE_URL}{path}"))
            .header("x-pm-appversion", APP_VERSION);
        if let Some(session) = self.session.lock().await.as_ref() {
            request = request.header("x-pm-uid", &session.uid).bearer_auth(&session.access_token);
        }
        if !query.is_empty() {
            request = request.query(query);
        }
        if let Some(body) = body {
            request = request.json(body);
        }
        for (name, value) in headers {
            request = request.header(*name, value);
        }
        parse(request.send().await?).await
    }

    /// Exchanges the refresh token for a new pair and persists it. `used` is
    /// the access token that got the 401; if another task already replaced it,
    /// there is nothing to do.
    async fn refresh(&self, used: &str) -> Result<()> {
        let mut guard = self.session.lock().await;
        let Some(session) = guard.as_mut() else {
            return Err(NeedsAuth("not logged in".into()).into());
        };
        if session.access_token != used {
            return Ok(());
        }

        // Body as in go-proton-api `authRefresh`.
        let state: [u8; 16] = proton_crypto_account::proton_crypto::generate_secure_random_bytes();
        let body = serde_json::json!({
            "UID": session.uid,
            "RefreshToken": session.refresh_token,
            "ResponseType": "token",
            "GrantType": "refresh_token",
            "RedirectURI": "https://protonmail.ch",
            "State": state.iter().map(|b| format!("{b:02x}")).collect::<String>(),
        });
        let response = self.http.post(format!("{BASE_URL}/auth/v4/refresh"))
            .header("x-pm-appversion", APP_VERSION)
            .header("x-pm-uid", &session.uid)
            .json(&body)
            .send()
            .await?;
        let tokens: RefreshResponse = match parse(response).await {
            Ok(tokens) => tokens,
            Err(err) => {
                // go-proton-api treats 400 and 422 as a revoked session.
                if let Some(api) = api_error(&err)
                    && (matches!(api.status, 400 | 401 | 422) || api.code == CODE_REFRESH_TOKEN_INVALID)
                {
                    return Err(NeedsAuth(format!("session expired ({})", api.message)).into());
                }
                return Err(err.context("refreshing the session"));
            }
        };
        session.access_token = tokens.access_token;
        session.refresh_token = tokens.refresh_token;
        if let Some(name) = &self.store_as
            && let Err(err) = heron_core::secrets::store(name, &*session)
        {
            warn!("saving refreshed session: {err:#}");
        }
        Ok(())
    }
}

#[derive(Deserialize)]
#[serde(rename_all = "PascalCase")]
struct RefreshResponse {
    access_token: String,
    refresh_token: String,
}

/// Turns a response into `T`, or an [`ApiError`] when the HTTP status or the
/// body's `Code` says it failed.
async fn parse<T: DeserializeOwned>(response: reqwest::Response) -> Result<T> {
    let status = response.status();
    let bytes = response.bytes().await?;
    let value: Value = serde_json::from_slice(&bytes).unwrap_or(Value::Null);
    check(status, &value)?;
    serde_json::from_value(value).context("unexpected response from the Proton API")
}

fn check(status: StatusCode, value: &Value) -> Result<(), ApiError> {
    let code = value.get("Code").and_then(Value::as_i64).unwrap_or(0);
    if status.is_success() && matches!(code, 0 | CODE_OK | CODE_MULTI) {
        return Ok(());
    }
    let message = value.get("Error").and_then(Value::as_str)
        .map(str::to_owned)
        .unwrap_or_else(|| status.canonical_reason().unwrap_or("request failed").to_owned());
    Err(ApiError {
        status: status.as_u16(),
        code,
        message,
        details: value.get("Details").cloned().unwrap_or(Value::Null),
    })
}

/// Treats `null` like a missing field.
pub fn or_default<'de, D: Deserializer<'de>, T: Default + Deserialize<'de>>(deserializer: D) -> Result<T, D::Error> {
    Ok(Option::<T>::deserialize(deserializer)?.unwrap_or_default())
}

/// Proton encodes booleans as 0/1; accept both forms (and `null` as false).
pub fn flag<'de, D: Deserializer<'de>>(deserializer: D) -> Result<bool, D::Error> {
    Ok(match Value::deserialize(deserializer)? {
        Value::Bool(b) => b,
        Value::Number(n) => n.as_i64().unwrap_or(0) != 0,
        _ => false,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn errors_from_status_and_code() {
        let ok = serde_json::json!({ "Code": 1000 });
        assert!(check(StatusCode::OK, &ok).is_ok());

        let hv = serde_json::json!({
            "Code": 9001,
            "Error": "Human verification required",
            "Details": { "HumanVerificationToken": "tok", "HumanVerificationMethods": ["captcha", "email"], "WebUrl": "https://verify.proton.me/?x" }
        });
        let err = check(StatusCode::UNPROCESSABLE_ENTITY, &hv).unwrap_err();
        assert_eq!(err.code, 9001);
        let details = err.human_verification().unwrap();
        assert_eq!(details.token, "tok");
        assert_eq!(details.methods, ["captcha", "email"]);

        // A failure code inside a 200 still counts as an error.
        let soft = serde_json::json!({ "Code": 2001, "Error": "Invalid value" });
        assert_eq!(check(StatusCode::OK, &soft).unwrap_err().message, "Invalid value");
        assert_eq!(check(StatusCode::UNAUTHORIZED, &Value::Null).unwrap_err().status, 401);
    }
}

//! How heron presents itself: a desktop Chrome session of the Discord web
//! client, the same fingerprint for the gateway and the REST calls.
//!
//! Property names and values are adapted from concord
//! (github.com/chojs23/concord, GPL-3.0), `src/discord/fingerprint.rs`.

use std::time::Duration;

use anyhow::{Context, Result, bail};
use base64::Engine;
use serde::Deserialize;
use serde_json::{Value, json};

pub const API: &str = "https://discord.com/api/v9";
pub const ORIGIN: &str = "https://discord.com";
pub const BROWSER_VERSION: &str = "150.0.0.0";
pub const USER_AGENT: &str =
    "Mozilla/5.0 (X11; Linux x86_64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/150.0.0.0 Safari/537.36";
/// A recent web client build. Discord accepts older builds; concord uses the
/// same value as its fallback when it cannot scrape the live one.
pub const CLIENT_BUILD_NUMBER: u64 = 580_004;
pub const LOCALE: &str = "en-US";

/// The `properties` object of Identify, also sent base64-encoded as
/// `X-Super-Properties` on REST requests.
pub fn properties() -> Value {
    json!({
        "os": "Linux",
        "browser": "Chrome",
        "device": "",
        "system_locale": LOCALE,
        "browser_user_agent": USER_AGENT,
        "browser_version": BROWSER_VERSION,
        "os_version": "",
        "referrer": "",
        "referring_domain": "",
        "referrer_current": "",
        "referring_domain_current": "",
        "release_channel": "stable",
        "client_build_number": CLIENT_BUILD_NUMBER,
        "client_event_source": null,
    })
}

fn super_properties() -> String {
    base64::engine::general_purpose::STANDARD.encode(properties().to_string())
}

#[derive(Debug, Deserialize)]
pub struct User {
    pub id: String,
    pub username: String,
    pub global_name: Option<String>,
}

/// A REST client with the web client's user agent.
pub fn http() -> Result<reqwest::Client> {
    Ok(reqwest::Client::builder().user_agent(USER_AGENT).timeout(Duration::from_secs(30)).build()?)
}

/// A GET with the headers the web client sends, `referer` being the page
/// the request would be made from.
pub fn get(client: &reqwest::Client, token: &str, url: &str, referer: &str) -> reqwest::RequestBuilder {
    client.get(url)
        .header("Authorization", token)
        .header("Accept", "*/*")
        .header("Accept-Language", "en-US,en;q=0.9")
        .header("Referer", referer)
        .header("Origin", ORIGIN)
        .header("X-Discord-Locale", LOCALE)
        .header("X-Super-Properties", super_properties())
}

/// A POST with the same headers as [`get`] and a JSON body.
pub fn post(client: &reqwest::Client, token: &str, url: &str, referer: &str, body: &Value) -> reqwest::RequestBuilder {
    client.post(url)
        .header("Authorization", token)
        .header("Accept", "*/*")
        .header("Accept-Language", "en-US,en;q=0.9")
        .header("Referer", referer)
        .header("Origin", ORIGIN)
        .header("X-Discord-Locale", LOCALE)
        .header("X-Super-Properties", super_properties())
        .json(body)
}

/// `GET /users/@me`; `Ok(None)` when Discord rejects the token.
pub async fn current_user(token: &str) -> Result<Option<User>> {
    let response = get(&http()?, token, &format!("{API}/users/@me"), &format!("{ORIGIN}/channels/@me"))
        .send()
        .await
        .context("contacting discord.com")?;
    match response.status().as_u16() {
        401 | 403 => Ok(None),
        200 => Ok(Some(response.json().await.context("parsing /users/@me")?)),
        status => bail!("discord.com answered HTTP {status}"),
    }
}

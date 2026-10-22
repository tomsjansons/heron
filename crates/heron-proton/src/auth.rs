//! Interactive login (SRP, TOTP, two-password mode, human verification) and
//! logout. Flow and field names follow go-proton-api `manager_auth.go`.

use std::io::Write;

use anyhow::{Context, Result, bail};
use heron_core::config::ProtonAccount;
use heron_core::secrets;
use proton_srp::{SRPAuth, SRPProofB64, SrpHashVersion};
use reqwest::Method;
use serde::Deserialize;
use serde::de::IgnoredAny;
use serde_json::json;
use tracing::warn;

use crate::api::{self, Api, Headers, HumanVerification, Session};

/// Name of the secrets file holding the session.
pub fn secret_name(account: &ProtonAccount) -> String {
    format!("proton-{}", account.id)
}

#[derive(Deserialize)]
#[serde(rename_all = "PascalCase")]
struct AuthInfo {
    version: u8,
    modulus: String,
    server_ephemeral: String,
    salt: String,
    #[serde(rename = "SRPSession")]
    srp_session: String,
}

#[derive(Deserialize)]
#[serde(rename_all = "PascalCase")]
struct AuthResponse {
    #[serde(rename = "UID")]
    uid: String,
    access_token: String,
    refresh_token: String,
    server_proof: String,
    #[serde(rename = "2FA", default)]
    two_fa: TwoFa,
    #[serde(default)]
    password_mode: u8,
}

#[derive(Default, Deserialize)]
#[serde(rename_all = "PascalCase")]
struct TwoFa {
    /// Bit 1: TOTP, bit 2: FIDO2.
    #[serde(default)]
    enabled: u8,
}

const TWO_FA_TOTP: u8 = 1;
const TWO_PASSWORD_MODE: u8 = 2;

pub async fn login(account: &ProtonAccount) -> Result<()> {
    let api = Api::new(None, None)?;
    let password = prompt(format!("Proton password for {}: ", account.username), true).await?;

    let mut verification: Headers = Vec::new();
    let auth = loop {
        match srp_login(&api, &account.username, &password, &verification).await {
            Ok(auth) => break auth,
            Err(err) => match api::api_error(&err) {
                Some(e) if e.code == api::CODE_WRONG_PASSWORD => bail!("wrong username or password"),
                Some(e) if e.code == api::CODE_HUMAN_VERIFICATION => {
                    let challenge = e.human_verification().context("human verification requested without details")?;
                    verification = human_verification(&challenge).await?;
                }
                _ => return Err(err),
            },
        }
    };

    api.set_session(Session {
        uid: auth.uid,
        access_token: auth.access_token,
        refresh_token: auth.refresh_token,
        key_passphrase: None,
    })
    .await;

    if auth.two_fa.enabled != 0 {
        if auth.two_fa.enabled & TWO_FA_TOTP == 0 {
            bail!(
                "this account only offers a security key (FIDO2) as second factor, which heron cannot use; \
                 add an authenticator app (TOTP) in Proton's security settings and try again"
            );
        }
        let code = prompt("Two-factor code: ".into(), false).await?;
        api.post::<IgnoredAny>("/auth/v4/2fa", &json!({ "TwoFactorCode": code.trim() }))
            .await
            .context("two-factor code rejected")?;
    }

    let mailbox_password = if auth.password_mode == TWO_PASSWORD_MODE {
        prompt("Mailbox password: ".into(), true).await?
    } else {
        password
    };
    let provider = proton_crypto_account::proton_crypto::new_pgp_provider();
    let key_passphrase = crate::keys::derive_passphrase(&provider, &api, &mailbox_password).await?;

    let mut session = api.session().await.context("lost the session")?;
    session.key_passphrase = Some(key_passphrase);
    secrets::store(&secret_name(account), &session)?;
    println!("Logged in to Proton as {}.", account.username);
    Ok(())
}

/// `/auth/v4/info` then `/auth/v4` with the SRP proofs, checking the server proof.
async fn srp_login(api: &Api, username: &str, password: &str, verification: &Headers) -> Result<AuthResponse> {
    let info: AuthInfo = api.post("/auth/v4/info", &json!({ "Username": username })).await?;
    let version = SrpHashVersion::try_from(info.version).context("unsupported SRP version")?;
    let srp = SRPAuth::with_pgp(Some(username), password, version, &info.salt, &info.modulus, &info.server_ephemeral)
        .context("SRP setup failed (bad modulus signature?)")?;
    let proofs: SRPProofB64 = srp.generate_proofs().context("SRP proof generation failed")?.into();

    let body = json!({
        "Username": username,
        "ClientEphemeral": proofs.client_ephemeral,
        "ClientProof": proofs.client_proof,
        "SRPSession": info.srp_session,
    });
    let auth: AuthResponse = api.call(Method::POST, "/auth/v4", &[], Some(body), verification).await?;
    anyhow::ensure!(proofs.compare_server_proof(&auth.server_proof), "the server's SRP proof does not match");
    Ok(auth)
}

/// Explains the challenge and returns the headers for the retry. Pressing
/// Enter after finishing the verify page retries with the challenge token, as
/// Bridge does; pasting a CAPTCHA token sends that token with type "captcha",
/// as proton-core does.
async fn human_verification(challenge: &HumanVerification) -> Result<Headers> {
    let url = challenge.web_url.clone().unwrap_or_else(|| {
        format!("https://verify.proton.me/?methods={}&token={}", challenge.methods.join(","), challenge.token)
    });
    let captcha = format!("https://mail-api.proton.me/core/v4/captcha?Token={}&ForceWebMessaging=1", challenge.token);
    println!(
        "\nProton wants to verify that you are human before allowing this login.\n\n\
         Either open this page, complete the check, then come back and press Enter:\n  {url}\n\n\
         Or, for a CAPTCHA token: open\n  {captcha}\n\
         paste this into the browser console (F12) before solving it:\n  \
         addEventListener('message',e=>{{let d=e.data;try{{d=typeof d=='string'?JSON.parse(d):d}}catch(x){{return}}\
         if(d&&d.type=='pm_captcha')prompt('Token for heron',d.token)}})\n\
         then copy the token it shows and paste it here.\n"
    );
    let answer = prompt("Token (or Enter once verified): ".into(), false).await?;
    let answer = answer.trim();
    Ok(if answer.is_empty() {
        vec![
            ("x-pm-human-verification-token", challenge.token.clone()),
            ("x-pm-human-verification-token-type", challenge.methods.join(",")),
        ]
    } else {
        vec![
            ("x-pm-human-verification-token", answer.to_owned()),
            ("x-pm-human-verification-token-type", "captcha".to_owned()),
        ]
    })
}

pub async fn logout(account: &ProtonAccount) -> Result<()> {
    let name = secret_name(account);
    if let Some(session) = secrets::load::<Session>(&name)? {
        let api = Api::new(Some(session), None)?;
        if let Err(err) = api.delete("/auth/v4").await {
            warn!("revoking the Proton session: {err:#}");
        }
    }
    secrets::remove(&name)
}

/// Reads a line from the terminal, hidden for passwords.
async fn prompt(text: String, hidden: bool) -> Result<String> {
    tokio::task::spawn_blocking(move || -> Result<String> {
        if hidden {
            return Ok(rpassword::prompt_password(text)?);
        }
        print!("{text}");
        std::io::stdout().flush()?;
        let mut line = String::new();
        std::io::stdin().read_line(&mut line)?;
        Ok(line.trim_end_matches(['\r', '\n']).to_owned())
    })
    .await?
}

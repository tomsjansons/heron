//! User and address keys: fetching them and unlocking them with the stored
//! key passphrase through `proton-crypto-account`.

use std::collections::HashMap;

use anyhow::{Context, Result};
use proton_crypto_account::keys::{
    AddressKeys, ArmoredPrivateKey, EncryptedKeyToken, KeyFlag, KeyId, KeyTokenSignature, LockedKey,
    UnlockedAddressKey, UnlockedUserKey, UserKeys,
};
use proton_crypto_account::proton_crypto::crypto::PGPProviderSync;
use proton_crypto_account::salts::{KeySecret, Salt, Salts};
use serde::Deserialize;
use tracing::{debug, warn};

use crate::api::{Api, NeedsAuth, flag, or_default};

/// A locked key as the API returns it, parsed leniently and converted to the
/// crate's [`LockedKey`].
#[derive(Debug, Deserialize)]
#[serde(rename_all = "PascalCase")]
pub struct ApiKey {
    #[serde(rename = "ID")]
    pub id: String,
    #[serde(default, deserialize_with = "or_default")]
    pub version: u32,
    pub private_key: String,
    pub token: Option<String>,
    pub signature: Option<String>,
    #[serde(default, deserialize_with = "flag")]
    pub primary: bool,
    #[serde(default, deserialize_with = "flag")]
    pub active: bool,
    pub flags: Option<u32>,
}

impl From<ApiKey> for LockedKey {
    fn from(key: ApiKey) -> Self {
        LockedKey {
            id: KeyId(key.id),
            version: key.version,
            private_key: ArmoredPrivateKey(key.private_key),
            token: key.token.map(EncryptedKeyToken),
            signature: key.signature.map(KeyTokenSignature),
            activation: None,
            primary: key.primary,
            active: key.active,
            flags: key.flags.map(KeyFlag::from),
            recovery_secret: None,
            recovery_secret_signature: None,
            address_forwarding_id: None,
        }
    }
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "PascalCase")]
pub struct User {
    #[serde(default, deserialize_with = "or_default")]
    pub keys: Vec<ApiKey>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "PascalCase")]
pub struct Address {
    #[serde(rename = "ID")]
    pub id: String,
    #[serde(default, deserialize_with = "or_default")]
    pub email: String,
    #[serde(default, deserialize_with = "or_default")]
    pub keys: Vec<ApiKey>,
}

pub async fn user(api: &Api) -> Result<User> {
    #[derive(Deserialize)]
    struct Response {
        #[serde(rename = "User")]
        user: User,
    }
    Ok(api.get::<Response>("/core/v4/users", &[]).await?.user)
}

pub async fn addresses(api: &Api) -> Result<Vec<Address>> {
    #[derive(Deserialize)]
    struct Response {
        #[serde(rename = "Addresses")]
        addresses: Vec<Address>,
    }
    Ok(api.get::<Response>("/core/v4/addresses", &[]).await?.addresses)
}

/// Derives the key passphrase from the (mailbox) password: bcrypt with the
/// primary user key's salt, as Bridge's `SaltForKey`. Checks that it unlocks a
/// user key before returning it.
pub async fn derive_passphrase<P: PGPProviderSync>(provider: &P, api: &Api, password: &str) -> Result<String> {
    #[derive(Deserialize)]
    struct Response {
        #[serde(rename = "KeySalts")]
        salts: Vec<Salt>,
    }
    let user = user(api).await?;
    let salts = api.get::<Response>("/core/v4/keys/salts", &[]).await?.salts;
    let primary = user.keys.iter().find(|k| k.primary).or(user.keys.first())
        .context("the account has no user keys")?;
    let srp = proton_crypto_account::proton_crypto::new_srp_provider();
    let secret = Salts::new(salts)
        .salt_for_key(&srp, &KeyId(primary.id.clone()), password.as_bytes())
        .context("deriving the key passphrase")?;

    let unlocked = UserKeys::new(user.keys.into_iter().map(LockedKey::from)).unlock(provider, &secret);
    anyhow::ensure!(!unlocked.unlocked_keys.is_empty(), "the mailbox password does not unlock the account keys");
    String::from_utf8(secret.as_bytes().to_vec()).context("key passphrase is not text")
}

/// Unlocked keys of the user and of every address, by address id.
pub struct Keyring<P: PGPProviderSync> {
    pub addresses: HashMap<String, Vec<UnlockedAddressKey<P>>>,
    /// Every address of the account, lowercase, unlocked or not.
    pub emails: Vec<String>,
}

impl<P: PGPProviderSync> Keyring<P> {
    pub async fn unlock(provider: &P, api: &Api, passphrase: &str) -> Result<Self> {
        let secret = KeySecret::new(passphrase.as_bytes().to_vec());
        let user = user(api).await?;
        let user_keys: Vec<UnlockedUserKey<P>> =
            UserKeys::new(user.keys.into_iter().map(LockedKey::from)).unlock(provider, &secret).unlocked_keys;
        if user_keys.is_empty() {
            return Err(NeedsAuth("the stored key passphrase no longer unlocks the account keys".into()).into());
        }

        let mut addresses = HashMap::new();
        let mut emails = Vec::new();
        for address in self::addresses(api).await? {
            if !address.email.is_empty() {
                emails.push(address.email.to_ascii_lowercase());
            }
            let result = AddressKeys::new(address.keys.into_iter().map(LockedKey::from))
                .unlock(provider, &user_keys, Some(&secret));
            for failure in &result.failed {
                debug!("address key of {}: {failure}", address.email);
            }
            if result.unlocked_keys.is_empty() {
                warn!("no key of {} could be unlocked", address.email);
                continue;
            }
            addresses.insert(address.id, result.unlocked_keys);
        }
        Ok(Self { addresses, emails })
    }
}

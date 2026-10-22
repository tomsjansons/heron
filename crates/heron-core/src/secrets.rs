//! Credentials as JSON files under [`paths::data_dir`], readable only by the
//! user. Each provider picks a name such as `google-work` and owns its format.

use std::fs;
use std::io::Write;
use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt};
use std::path::PathBuf;

use anyhow::{Context, Result};
use serde::Serialize;
use serde::de::DeserializeOwned;

use crate::paths;

pub fn path(name: &str) -> PathBuf {
    paths::data_dir().join(format!("{name}.json"))
}

/// `Ok(None)` when nothing is stored under `name`.
pub fn load<T: DeserializeOwned>(name: &str) -> Result<Option<T>> {
    let path = path(name);
    match fs::read(&path) {
        Ok(bytes) => {
            let value = serde_json::from_slice(&bytes).with_context(|| format!("parsing {}", path.display()))?;
            Ok(Some(value))
        }
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(err) => Err(err).with_context(|| format!("reading {}", path.display())),
    }
}

/// Writes atomically with mode 0600, creating the directory with mode 0700.
pub fn store<T: Serialize>(name: &str, value: &T) -> Result<()> {
    let dir = paths::data_dir();
    fs::DirBuilder::new().recursive(true).mode(0o700).create(&dir)
        .with_context(|| format!("creating {}", dir.display()))?;

    let path = path(name);
    let tmp = path.with_extension("json.tmp");
    let mut file = fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(0o600)
        .open(&tmp)
        .with_context(|| format!("writing {}", tmp.display()))?;
    file.write_all(&serde_json::to_vec_pretty(value)?)?;
    file.sync_all()?;
    fs::rename(&tmp, &path).with_context(|| format!("writing {}", path.display()))?;
    Ok(())
}

pub fn remove(name: &str) -> Result<()> {
    match fs::remove_file(path(name)) {
        Err(err) if err.kind() != std::io::ErrorKind::NotFound => Err(err.into()),
        _ => Ok(()),
    }
}

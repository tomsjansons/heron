//! XDG locations. Config is hand-written; data holds credentials; state holds
//! what the daemon remembers between runs, such as acknowledgements.

use std::path::PathBuf;

fn xdg(var: &str, fallback: &str) -> PathBuf {
    match std::env::var_os(var) {
        Some(dir) if !dir.is_empty() => PathBuf::from(dir),
        _ => home().join(fallback),
    }
}

fn home() -> PathBuf {
    std::env::var_os("HOME").map(PathBuf::from).unwrap_or_else(|| PathBuf::from("/"))
}

/// `$XDG_CONFIG_HOME/heron/config.toml`
pub fn config_file() -> PathBuf {
    xdg("XDG_CONFIG_HOME", ".config").join("heron/config.toml")
}

/// `$XDG_DATA_HOME/heron/`, holding credentials. Created 0700 on first write.
pub fn data_dir() -> PathBuf {
    xdg("XDG_DATA_HOME", ".local/share").join("heron")
}

/// `$XDG_STATE_HOME/heron/`
pub fn state_dir() -> PathBuf {
    xdg("XDG_STATE_HOME", ".local/state").join("heron")
}

/// `$XDG_RUNTIME_DIR/heron.sock`, falling back to `/tmp/heron-$USER.sock`.
pub fn socket_path() -> PathBuf {
    match std::env::var_os("XDG_RUNTIME_DIR") {
        Some(dir) if !dir.is_empty() => PathBuf::from(dir).join("heron.sock"),
        _ => {
            let user = std::env::var("USER").unwrap_or_else(|_| "user".into());
            PathBuf::from(format!("/tmp/heron-{user}.sock"))
        }
    }
}

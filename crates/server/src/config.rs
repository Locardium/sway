//! Server configuration, in a TOML file edited over SSH.

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

#[derive(Debug, Serialize, Deserialize)]
pub struct Config {
    /// Where to listen. `0.0.0.0` to accept connections from outside the machine.
    #[serde(default = "default_listen")]
    pub listen: String,

    /// Name shown in the app's device list.
    #[serde(default = "default_name")]
    pub name: String,

    /// The database: identity of every track, playlists, and which device has what.
    #[serde(default = "default_data_dir")]
    pub data_dir: PathBuf,

    /// The audio files. This is what grows; it should live on the big disk,
    /// not the system one.
    #[serde(default = "default_music_dir")]
    pub music_dir: PathBuf,

    /// How many days a deleted file survives in the server's trash.
    ///
    /// A deletion travels: you delete it on one device and it disappears
    /// from all of them, server included. This is the only thing that makes
    /// the file recoverable afterward — without it, an accidental deletion
    /// takes down the backup copy too, which is exactly the one that had to
    /// survive.
    ///
    /// `0` destroys it on the spot: an exact mirror of your devices, with no
    /// safety net.
    #[serde(default = "default_retention_days")]
    pub retention_days: u64,

    /// What a device has to present in order to pair.
    ///
    /// Replaces the six-digit code: there's no screen here to compare it
    /// against. Generated only the first time. Changing it doesn't unpair
    /// anyone already paired — their keys are already stored — so it can be
    /// rotated without breaking anything.
    pub pair_token: String,
}

fn default_listen() -> String {
    "0.0.0.0:7420".into()
}
fn default_name() -> String {
    "Sway Server".into()
}
fn default_data_dir() -> PathBuf {
    "data".into()
}
fn default_music_dir() -> PathBuf {
    "music".into()
}
fn default_retention_days() -> u64 {
    90
}

impl Config {
    /// Environment variable that overrides the file's `pair_token`. In a
    /// deployment it's best for the secret not to live in a file that could
    /// end up in a repo.
    pub const TOKEN_ENV: &'static str = "SWAY_SERVER_TOKEN";

    /// Loads the file. If it doesn't exist, writes it with a new token and
    /// returns `None`: the first run doesn't start listening, it just prints
    /// what to do.
    pub fn load_or_create(path: &Path) -> Result<Option<Self>> {
        if path.exists() {
            let text = std::fs::read_to_string(path)
                .with_context(|| format!("could not read {}", path.display()))?;
            let mut cfg: Config = toml::from_str(&text)
                .with_context(|| format!("{} is not valid TOML", path.display()))?;
            if let Some(from_env) = std::env::var(Self::TOKEN_ENV)
                .ok()
                .map(|s| s.trim().to_string())
                .filter(|s| !s.is_empty())
            {
                log::info!("[server] token taken from {}", Self::TOKEN_ENV);
                cfg.pair_token = from_env;
            }
            if cfg.pair_token.trim().is_empty() {
                anyhow::bail!("`pair_token` is empty in {}", path.display());
            }
            return Ok(Some(cfg));
        }
        let token = uuid::Uuid::new_v4().simple().to_string();
        std::fs::write(path, template(&token))
            .with_context(|| format!("could not write {}", path.display()))?;
        Ok(None)
    }
}

/// Written by hand instead of with `toml::to_string` so the comments can
/// stay: this file will be read by a person who never saw this code.
fn template(token: &str) -> String {
    format!(
        r#"# Sway archive and sync server.
#
# Keeps whatever your devices send it and hands it back when they ask. It
# never imports music on its own: everything it has, someone sent it.

# Where to listen. 0.0.0.0 accepts connections from outside this machine.
listen = "0.0.0.0:7420"

# How it shows up in the device list in the app.
name = "Sway Server"

# The database: identity of every track, playlists, and per-device state.
#
# CAREFUL with Windows paths: a backslash is an escape in TOML. Write
# 'D:\Music' with SINGLE quotes, or "D:/Music" with forward slashes.
data_dir = "data"

# The audio files. This is what grows - point it at the big disk.
music_dir = "music"

# How many days a deleted file survives in the server's trash.
#
# Deletions travel: delete on one device and it is gone from all of them, the
# server included. This is the only thing that makes it recoverable
# afterwards. At 0 it is destroyed right away - an exact mirror of your
# devices, with no safety net.
retention_days = 90

# What you type in the app to pair a device with this server. It replaces the
# six-digit code, because there is no screen here to compare it on. Treat it
# like a password. Changing it does NOT unpair what is already paired.
pair_token = "{token}"
"#
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "sway-cfg-{tag}-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        std::fs::remove_dir_all(&dir).ok();
        std::fs::create_dir_all(&dir).unwrap();
        dir.join("config.toml")
    }

    #[test]
    fn the_first_run_writes_the_file_and_does_not_start() {
        let path = tmp("nuevo");
        assert!(
            Config::load_or_create(&path).unwrap().is_none(),
            "can't start listening before someone looks at the file"
        );
        assert!(path.exists());

        // And what it wrote has to be readable again.
        let cfg = Config::load_or_create(&path).unwrap().unwrap();
        assert_eq!(cfg.listen, "0.0.0.0:7420");
        assert_eq!(cfg.retention_days, 90);
        assert_eq!(cfg.pair_token.len(), 32, "token: uuid v4 in hex");
    }

    #[test]
    fn two_servers_are_not_born_with_the_same_token() {
        let a = tmp("tok-a");
        let b = tmp("tok-b");
        Config::load_or_create(&a).unwrap();
        Config::load_or_create(&b).unwrap();
        let ta = Config::load_or_create(&a).unwrap().unwrap().pair_token;
        let tb = Config::load_or_create(&b).unwrap().unwrap().pair_token;
        assert_ne!(ta, tb);
    }

    #[test]
    fn an_old_file_without_retention_reads_with_the_default() {
        let path = tmp("viejo");
        std::fs::write(
            &path,
            "listen = \"0.0.0.0:7420\"\npair_token = \"algo\"\n",
        )
        .unwrap();
        let cfg = Config::load_or_create(&path).unwrap().unwrap();
        assert_eq!(cfg.retention_days, 90);
        assert_eq!(cfg.music_dir, PathBuf::from("music"));
    }

    #[test]
    fn an_empty_token_does_not_start_the_server() {
        let path = tmp("vacio");
        std::fs::write(&path, "pair_token = \"\"\n").unwrap();
        // Without this, anyone reaching the port pairs: `secret_eq("",
        // "")` is true.
        assert!(Config::load_or_create(&path).is_err());
    }
}

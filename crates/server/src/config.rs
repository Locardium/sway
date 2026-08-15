//! Configuración del server, en un TOML que se edita por SSH.

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

#[derive(Debug, Serialize, Deserialize)]
pub struct Config {
    /// Dónde escuchar. `0.0.0.0` para aceptar desde afuera de la máquina.
    #[serde(default = "default_listen")]
    pub listen: String,

    /// Nombre visible en la lista de dispositivos de la app.
    #[serde(default = "default_name")]
    pub name: String,

    /// La base: quién es cada track, las playlists y qué dispositivo tiene qué.
    #[serde(default = "default_data_dir")]
    pub data_dir: PathBuf,

    /// Los archivos de audio. Es lo que crece; conviene que sea el disco
    /// grande y no el del sistema.
    #[serde(default = "default_music_dir")]
    pub music_dir: PathBuf,

    /// Cuántos días sobrevive en la papelera del server un archivo borrado.
    ///
    /// Un borrado viaja: lo borrás en un dispositivo y desaparece de todos,
    /// server incluido. Esto es lo único que hace que el archivo se pueda
    /// rescatar después — sin esto, un borrado por error se lleva puesta
    /// también la copia de respaldo, que es justo la que tenía que sobrevivir.
    ///
    /// `0` destruye en el acto: espejo exacto de tus dispositivos, sin red
    /// debajo.
    #[serde(default = "default_retention_days")]
    pub retention_days: u64,

    /// Lo que un dispositivo tiene que presentar para vincularse.
    ///
    /// Reemplaza al código de seis dígitos: acá no hay ninguna pantalla donde
    /// compararlo. Se genera solo la primera vez. Cambiarlo no desvincula a
    /// nadie que ya esté vinculado — las claves ya están guardadas — así que
    /// se puede rotar sin romper nada.
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
    /// Variable que pisa al `pair_token` del archivo. En un despliegue conviene
    /// que el secreto no viva en un archivo que puede terminar en un repo.
    pub const TOKEN_ENV: &'static str = "SWAY_SERVER_TOKEN";

    /// Carga el archivo. Si no existe lo escribe con un token nuevo y devuelve
    /// `None`: la primera corrida no arranca a escuchar, imprime qué hacer.
    pub fn load_or_create(path: &Path) -> Result<Option<Self>> {
        if path.exists() {
            let text = std::fs::read_to_string(path)
                .with_context(|| format!("no se pudo leer {}", path.display()))?;
            let mut cfg: Config = toml::from_str(&text)
                .with_context(|| format!("{} no es un TOML válido", path.display()))?;
            if let Some(from_env) = std::env::var(Self::TOKEN_ENV)
                .ok()
                .map(|s| s.trim().to_string())
                .filter(|s| !s.is_empty())
            {
                log::info!("[server] token taken from {}", Self::TOKEN_ENV);
                cfg.pair_token = from_env;
            }
            if cfg.pair_token.trim().is_empty() {
                anyhow::bail!("`pair_token` está vacío en {}", path.display());
            }
            return Ok(Some(cfg));
        }
        let token = uuid::Uuid::new_v4().simple().to_string();
        std::fs::write(path, template(&token))
            .with_context(|| format!("no se pudo escribir {}", path.display()))?;
        Ok(None)
    }
}

/// Se escribe a mano y no con `toml::to_string` para poder dejar los
/// comentarios: el archivo lo va a leer una persona que no vio este código.
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
        dir.join("sway-server.toml")
    }

    #[test]
    fn la_primera_corrida_escribe_el_archivo_y_no_arranca() {
        let path = tmp("nuevo");
        assert!(
            Config::load_or_create(&path).unwrap().is_none(),
            "no puede ponerse a escuchar antes de que alguien mire el archivo"
        );
        assert!(path.exists());

        // Y lo que escribió tiene que poder volver a leerse.
        let cfg = Config::load_or_create(&path).unwrap().unwrap();
        assert_eq!(cfg.listen, "0.0.0.0:7420");
        assert_eq!(cfg.retention_days, 90);
        assert_eq!(cfg.pair_token.len(), 32, "token: uuid v4 en hex");
    }

    #[test]
    fn dos_servers_no_nacen_con_el_mismo_token() {
        let a = tmp("tok-a");
        let b = tmp("tok-b");
        Config::load_or_create(&a).unwrap();
        Config::load_or_create(&b).unwrap();
        let ta = Config::load_or_create(&a).unwrap().unwrap().pair_token;
        let tb = Config::load_or_create(&b).unwrap().unwrap().pair_token;
        assert_ne!(ta, tb);
    }

    #[test]
    fn un_archivo_viejo_sin_retencion_se_lee_con_el_default() {
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
    fn un_token_vacio_no_arranca_el_server() {
        let path = tmp("vacio");
        std::fs::write(&path, "pair_token = \"\"\n").unwrap();
        // Sin esto, cualquiera que llegue al puerto se vincula: `secret_eq("",
        // "")` es true.
        assert!(Config::load_or_create(&path).is_err());
    }
}

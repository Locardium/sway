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

impl Config {
    /// Carga el archivo. Si no existe lo escribe con un token nuevo y devuelve
    /// `None`: la primera corrida no arranca a escuchar, imprime qué hacer.
    pub fn load_or_create(path: &Path) -> Result<Option<Self>> {
        if path.exists() {
            let text = std::fs::read_to_string(path)
                .with_context(|| format!("no se pudo leer {}", path.display()))?;
            let cfg: Config = toml::from_str(&text)
                .with_context(|| format!("{} no es un TOML válido", path.display()))?;
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
        r#"# Server de archivo y sync de Sway.
#
# Guarda todo lo que le mandan los dispositivos y se los devuelve cuando lo
# piden. No importa musica por su cuenta: lo que tiene, se lo mando alguien.

# Donde escuchar. 0.0.0.0 acepta desde afuera de esta maquina.
listen = "0.0.0.0:7420"

# Como se ve en la lista de dispositivos de la app.
name = "Sway Server"

# La base: identidad de cada track, playlists y estado de cada dispositivo.
data_dir = "data"

# Los archivos de audio. Esto es lo que crece — que sea el disco grande.
music_dir = "music"

# Lo que hay que poner en la app para vincular un dispositivo con este server.
# Reemplaza al codigo de seis digitos, porque aca no hay pantalla donde
# compararlo. Tratalo como una contrasena. Cambiarlo NO desvincula lo que ya
# esta vinculado.
pair_token = "{token}"
"#
    )
}

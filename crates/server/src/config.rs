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
                log::info!("[server] token tomado de {}", Self::TOKEN_ENV);
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
        r#"# Server de archivo y sync de Sway.
#
# Guarda todo lo que le mandan los dispositivos y se los devuelve cuando lo
# piden. No importa musica por su cuenta: lo que tiene, se lo mando alguien.

# Donde escuchar. 0.0.0.0 acepta desde afuera de esta maquina.
listen = "0.0.0.0:7420"

# Como se ve en la lista de dispositivos de la app.
name = "Sway Server"

# La base: identidad de cada track, playlists y estado de cada dispositivo.
#
# OJO con las rutas de Windows: la barra invertida es un escape. Se pone
# 'D:\Musica' con comillas SIMPLES, o "D:/Musica" con barras normales.
data_dir = "data"

# Los archivos de audio. Esto es lo que crece — que sea el disco grande.
music_dir = "music"

# Cuantos dias sobrevive en la papelera del server un archivo borrado.
#
# Un borrado viaja: lo borras en un dispositivo y desaparece de todos, server
# incluido. Esto es lo unico que hace que se pueda rescatar despues. En 0 se
# destruye en el acto — espejo exacto de tus dispositivos, sin red debajo.
retention_days = 90

# Lo que hay que poner en la app para vincular un dispositivo con este server.
# Reemplaza al codigo de seis digitos, porque aca no hay pantalla donde
# compararlo. Tratalo como una contrasena. Cambiarlo NO desvincula lo que ya
# esta vinculado.
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

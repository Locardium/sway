//! Server de archivo y sync de Sway.
//!
//! Guarda lo que le mandan los dispositivos y se lo devuelve cuando lo piden.
//! No importa música por su cuenta y no tiene interfaz: todo lo que tiene, se
//! lo mandó alguien.
//!
//! Para qué sirve, en dos casos concretos:
//!
//! - **Sincronizar fuera de casa.** El descubrimiento por mDNS sólo ve la red
//!   local; dos dispositivos en redes distintas no se encuentran ni se pueden
//!   llamar (los dos están detrás de un NAT). Contra un server con dirección
//!   pública, en cambio, los dos marcan hacia afuera — y como el server tiene
//!   todo, nadie necesita que el otro esté prendido.
//! - **Recuperar.** Si se pierde la biblioteca de todos los dispositivos, acá
//!   están los archivos y la organización.

use anyhow::{Context, Result};
use std::net::TcpListener;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use sway_core::{db, engine::Host, pairing};
use sway_server::config::Config;
use sway_server::host::ServerHost;
use sway_server::serve;

const DEFAULT_CONFIG: &str = "config.toml";
/// El nombre anterior. Un server que ya está andando lo tiene así, y
/// arrancar con el nombre nuevo le escribiría una configuración nueva —con
/// OTRO token— y se apagaría sin llegar a escuchar, que bajo systemd es una
/// caída silenciosa. Se sigue aceptando, avisando.
const LEGACY_CONFIG: &str = "sway-server.toml";

fn main() {
    let code = match run() {
        Ok(()) => 0,
        Err(e) => {
            eprintln!("Error: {e:#}");
            1
        }
    };
    // Lo último que pasa, salga bien o mal: si esta ventana la abrió un doble
    // click, cerrarse acá se lleva el mensaje puesto — que en la primera
    // corrida es justamente dónde quedó el token.
    hold_window_open();
    std::process::exit(code);
}

/// Deja la ventana abierta hasta que alguien apriete Enter, **sólo** si la
/// consola es de este proceso.
///
/// La distinción importa: en una terminal o bajo systemd nadie va a apretar
/// nada, y un server que arranca esperando una tecla no arranca nunca. Windows
/// lo dice contando cuántos procesos comparten la consola — si es uno solo,
/// la creó este programa al abrirse, o sea que fue un doble click.
#[cfg(windows)]
fn hold_window_open() {
    use windows_sys::Win32::System::Console::GetConsoleProcessList;
    let mut pids = [0u32; 2];
    let attached = unsafe { GetConsoleProcessList(pids.as_mut_ptr(), pids.len() as u32) };
    if attached != 1 {
        return;
    }
    println!("\nPress Enter to close.");
    let mut _line = String::new();
    let _ = std::io::stdin().read_line(&mut _line);
}

#[cfg(not(windows))]
fn hold_window_open() {}

fn run() -> Result<()> {
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info")).init();

    let path = config_path();
    let Some(cfg) = Config::load_or_create(&path)? else {
        println!("Created {} with a new token.", path.display());
        println!();
        println!("Open it, copy the `pair_token`, and start the server again.");
        println!("It will then start listening, and you can add it from the app.");
        return Ok(());
    };

    std::fs::create_dir_all(&cfg.data_dir)
        .with_context(|| format!("no se pudo crear {}", cfg.data_dir.display()))?;
    std::fs::create_dir_all(&cfg.music_dir)
        .with_context(|| format!("no se pudo crear {}", cfg.music_dir.display()))?;

    let db_file = cfg.data_dir.join("sway.sqlite");
    let conn = db::open(&db_file).with_context(|| format!("no se pudo abrir {}", db_file.display()))?;
    // El WAL se consolida desde su propia conexión, sin frenar a quien escribe.
    db::spawn_checkpointer(&db_file);
    // El nombre sale de la config: es lo que se va a ver en la app.
    db::set_device_name(&conn, &cfg.name)?;

    let hostage = Arc::new(ServerHost::new(conn, cfg.music_dir.clone()));
    // Genera el par de claves en la primera corrida.
    let (uid, pubkey) = hostage.with_db(|conn| {
        let (_, public) = pairing::keypair(conn)?;
        Ok((db::this_device_uid(conn)?, public))
    })?;

    // El server quiere todo, en las dos direcciones, y lo declara en la fila
    // que se replica. No alcanza con que sea el default: los dispositivos leen
    // esa fila para decidir qué le mandan, y una fila explícita es también lo
    // que la app muestra (en gris) cuando abrís el server en la lista.
    hostage.with_db(|conn| {
        let me = db::this_device_uid(conn)?;
        sway_core::scope::set_mode(conn, &me, sway_core::scope::Mode::All)?;
        sway_core::scope::set_direction(conn, &me, "both")?;
        Ok(())
    })?;

    // La papelera: lo que la retención ya dejó vencer se borra de verdad.
    // Corre al arrancar y una vez por día — un server queda prendido meses, y
    // si sólo se limpiara al arrancar no se limpiaría nunca.
    spawn_trash_purge(cfg.music_dir.clone(), cfg.retention_days);

    let listener = TcpListener::bind(&cfg.listen)
        .with_context(|| format!("no se pudo escuchar en {}", cfg.listen))?;

    log::info!("[server] {} ({uid})", cfg.name);
    log::info!("[server] public key {}", fingerprint(&pubkey));
    log::info!("[server] files in {}", cfg.music_dir.display());

    serve::run(
        Arc::new(serve::Server {
            host: hostage,
            token: cfg.pair_token,
        }),
        listener,
    )
}

fn spawn_trash_purge(music_dir: PathBuf, retention_days: u64) {
    std::thread::spawn(move || loop {
        let n = sway_core::trash::purge_old(&music_dir, retention_days);
        if n > 0 {
            log::info!("[server] trash: {n} file(s) older than {retention_days} days removed");
        }
        std::thread::sleep(std::time::Duration::from_secs(24 * 3600));
    });
}

fn config_path() -> PathBuf {
    // Una sola opción, y por eso sin biblioteca de argumentos:
    //   sway-server [ruta-del-config]
    resolve_config(std::env::args().nth(1), Path::new("."))
}

/// Qué archivo de configuración usar. La ruta escrita a mano gana siempre;
/// sin ella manda el nombre nuevo, y el viejo sólo entra si es el único que
/// existe.
fn resolve_config(arg: Option<String>, dir: &Path) -> PathBuf {
    if let Some(arg) = arg {
        return PathBuf::from(arg);
    }
    let current = dir.join(DEFAULT_CONFIG);
    if !current.exists() {
        let legacy = dir.join(LEGACY_CONFIG);
        if legacy.exists() {
            log::warn!("[server] using {LEGACY_CONFIG}: rename it to {DEFAULT_CONFIG}");
            return legacy;
        }
    }
    current
}

/// Los primeros bytes de la clave, para poder compararla de un vistazo con la
/// que muestre la app si algún día no coincide.
fn fingerprint(pubkey: &[u8]) -> String {
    pubkey
        .iter()
        .take(8)
        .map(|b| format!("{b:02x}"))
        .collect::<Vec<_>>()
        .join(":")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "sway-cfgpath-{tag}-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        std::fs::remove_dir_all(&dir).ok();
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn sin_nada_escrito_se_usa_el_nombre_nuevo() {
        let dir = tmp("nuevo");
        assert_eq!(resolve_config(None, &dir), dir.join(DEFAULT_CONFIG));
    }

    /// Un server que ya venía andando tiene el nombre viejo. Si el renombre lo
    /// ignorara, la primera corrida escribiría una configuración nueva con OTRO
    /// token y se apagaría sin escuchar — y bajo systemd eso es un server caído
    /// sin que nadie se entere.
    #[test]
    fn el_nombre_viejo_sigue_sirviendo_si_es_el_unico() {
        let dir = tmp("viejo");
        std::fs::write(dir.join(LEGACY_CONFIG), "").unwrap();
        assert_eq!(resolve_config(None, &dir), dir.join(LEGACY_CONFIG));
    }

    /// Con los dos archivos, manda el nuevo: es el que el usuario acaba de
    /// escribir, y el viejo puede ser el que quedó de antes.
    #[test]
    fn con_los_dos_gana_el_nuevo() {
        let dir = tmp("ambos");
        std::fs::write(dir.join(LEGACY_CONFIG), "").unwrap();
        std::fs::write(dir.join(DEFAULT_CONFIG), "").unwrap();
        assert_eq!(resolve_config(None, &dir), dir.join(DEFAULT_CONFIG));
    }

    /// La ruta escrita a mano gana siempre, exista o no: es lo que permite
    /// tener la configuración fuera del directorio de trabajo (ver el systemd
    /// del README).
    #[test]
    fn la_ruta_a_mano_le_gana_a_todo() {
        let dir = tmp("amano");
        std::fs::write(dir.join(DEFAULT_CONFIG), "").unwrap();
        let elegida = dir.join("otra-cosa.toml");
        assert_eq!(
            resolve_config(Some(elegida.display().to_string()), &dir),
            elegida
        );
    }
}

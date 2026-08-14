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
use std::path::PathBuf;
use std::sync::Arc;
use sway_core::{db, engine::Host, pairing};
use sway_server::config::Config;
use sway_server::host::ServerHost;
use sway_server::serve;

const DEFAULT_CONFIG: &str = "sway-server.toml";

fn main() -> Result<()> {
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info")).init();

    let path = config_path();
    let Some(cfg) = Config::load_or_create(&path)? else {
        println!("Se creó {} con un token nuevo.", path.display());
        println!("Revisá el archivo y volvé a arrancar el server.");
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

    let listener = TcpListener::bind(&cfg.listen)
        .with_context(|| format!("no se pudo escuchar en {}", cfg.listen))?;

    log::info!("[server] {} ({uid})", cfg.name);
    log::info!("[server] clave pública {}", fingerprint(&pubkey));
    log::info!("[server] archivos en {}", cfg.music_dir.display());

    serve::run(
        Arc::new(serve::Server {
            host: hostage,
            token: cfg.pair_token,
        }),
        listener,
    )
}

fn config_path() -> PathBuf {
    // Una sola opción, y por eso sin biblioteca de argumentos:
    //   sway-server [ruta-del-config]
    std::env::args()
        .nth(1)
        .map(PathBuf::from)
        .unwrap_or_else(|| DEFAULT_CONFIG.into())
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

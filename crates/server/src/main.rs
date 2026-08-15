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
    println!("\nEnter para cerrar.");
    let mut _line = String::new();
    let _ = std::io::stdin().read_line(&mut _line);
}

#[cfg(not(windows))]
fn hold_window_open() {}

fn run() -> Result<()> {
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info")).init();

    let path = config_path();
    let Some(cfg) = Config::load_or_create(&path)? else {
        println!("Se creó {} con un token nuevo.", path.display());
        println!();
        println!("Abrilo, copiá el `pair_token`, y volvé a arrancar el server.");
        println!("Ahí queda escuchando y ya lo podés agregar desde la app.");
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

fn spawn_trash_purge(music_dir: PathBuf, retention_days: u64) {
    std::thread::spawn(move || loop {
        let n = sway_core::trash::purge_old(&music_dir, retention_days);
        if n > 0 {
            log::info!("[server] papelera: {n} archivo(s) pasaron los {retention_days} días");
        }
        std::thread::sleep(std::time::Duration::from_secs(24 * 3600));
    });
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

//! El motor de sync corriendo sin nadie adelante.
//!
//! La app implementa `Host` sobre su `AppHandle` (base en el estado de Tauri,
//! avisos como eventos de ventana) y la suite de integridad lo implementa
//! sobre un directorio temporal. Esta es la tercera implementación, y la más
//! chata de las tres: una base, una carpeta, y el resto al log.

use anyhow::{anyhow, Result};
use std::path::PathBuf;
use std::sync::Mutex;
use sway_core::engine::{Host, Progress};
use sway_core::rusqlite::Connection;

pub struct ServerHost {
    db: Mutex<Connection>,
    music_dir: PathBuf,
}

impl ServerHost {
    pub fn new(conn: Connection, music_dir: PathBuf) -> Self {
        Self {
            db: Mutex::new(conn),
            music_dir,
        }
    }
}

impl Host for ServerHost {
    fn with_db<T>(&self, f: impl FnOnce(&Connection) -> Result<T>) -> Result<T> {
        let conn = self.db.lock().map_err(|_| anyhow!("poisoned db lock"))?;
        f(&conn)
    }

    // `with_db_read` queda como el default (la misma conexión). En la app hay
    // una segunda conexión de sólo lectura porque la pantalla no puede quedar
    // esperando a que el sync suelte el lock; acá no hay pantalla.

    fn music_dir(&self) -> PathBuf {
        self.music_dir.clone()
    }

    // `expect_path` no hace nada: no hay watcher de carpeta, así que nadie va
    // a intentar auto-importar lo que deja el sync.

    fn progress(&self, p: &Progress) {
        // Sólo los extremos de cada archivo. Un server que corre semanas no
        // puede escribir una línea por chunk.
        if p.done == 0 {
            log::info!(
                "[sync] {} {}/{}: {}",
                if p.sending { "sending" } else { "receiving" },
                p.index + 1,
                p.total_files,
                p.filename
            );
        } else if p.done >= p.total && p.total > 0 {
            log::info!("[sync] done {} ({} bytes)", p.filename, p.total);
        }
    }

    // `library_changed` tampoco: no hay UI que recargar.
}

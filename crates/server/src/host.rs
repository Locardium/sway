//! El motor de sync corriendo sin nadie adelante.
//!
//! La app implementa `Host` sobre su `AppHandle` (base en el estado de Tauri,
//! avisos como eventos de ventana) y la suite de integridad lo implementa
//! sobre un directorio temporal. Esta es la tercera implementación, y la más
//! chata de las tres: una base, una carpeta, y el resto al log.

use anyhow::{anyhow, Result};
use std::collections::VecDeque;
use std::path::PathBuf;
use std::sync::{Condvar, Mutex};
use std::time::Duration;
use sway_core::engine::{Host, Progress, Seen};
use sway_core::wire::Mark;
use sway_core::rusqlite::Connection;

pub struct ServerHost {
    db: Mutex<Connection>,
    music_dir: PathBuf,
    /// Qué se movió acá y quién lo movió, para poder avisarle a los demás.
    changes: Mutex<Changes>,
    changed: Condvar,
}

/// Cuántos cambios se recuerdan con su autor.
///
/// Sesenta y cuatro es holgado: entre que un dispositivo empieza a esperar y
/// que se le contesta pasan segundos, y harían falta 64 cambios de OTROS
/// dispositivos en esa ventana para que se olvide alguno. Y olvidarse no
/// pierde nada: se contesta que sí, que es un sync de más y no uno de menos.
const REMEMBER: usize = 64;

#[derive(Default)]
struct Changes {
    /// Identifica ESTA corrida del server. Se sortea al arrancar.
    ///
    /// Sin esto, la revisión 57 de hoy y la 57 de después de un reinicio son
    /// el mismo número, y un dispositivo con la marca guardada concluiría que
    /// está al día justo cuando se perdió todo lo del medio.
    epoch: u64,
    /// Cuántas veces se movió la biblioteca desde que arrancó el proceso. No
    /// es persistente ni tiene por qué serlo: un reinicio corta las conexiones
    /// que estaban esperando, así que nadie se queda con un número viejo.
    rev: u64,
    /// Los últimos, con el uid de quien los hizo.
    recent: VecDeque<(u64, String)>,
}

impl Changes {
    fn mark(&self) -> Mark {
        Mark { epoch: self.epoch, rev: self.rev }
    }

    /// ¿Pasó algo después de `since` que `ignoring` no sepa ya?
    ///
    /// El autor importa: sin esto, el dispositivo que empuja un cambio se
    /// despierta a sí mismo y sale a sincronizar de nuevo lo que él acaba de
    /// mandar — un manifiesto entero por internet, en cada cambio local.
    fn news_for(&self, since: u64, ignoring: &str) -> bool {
        if self.rev <= since {
            return false;
        }
        // Se olvidó parte de lo que pasó desde entonces: no hay forma de saber
        // de quién era, y quedarse callado sería peor que un sync de más.
        if self.recent.front().map(|(r, _)| *r > since + 1).unwrap_or(true) {
            return true;
        }
        self.recent.iter().any(|(r, who)| *r > since && who != ignoring)
    }
}

impl ServerHost {
    pub fn new(conn: Connection, music_dir: PathBuf) -> Self {
        Self {
            db: Mutex::new(conn),
            music_dir,
            changes: Mutex::new(Changes {
                // Al azar y no la hora: dos arranques dentro del mismo
                // milisegundo darían la misma, y "improbable" no es "no pasa".
                epoch: uuid::Uuid::new_v4().as_u128() as u64,
                ..Changes::default()
            }),
            changed: Condvar::new(),
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

    // `library_changed` sigue sin hacer nada: no hay UI que recargar. Avisarle
    // a los demás es otra cosa, y entra por `note_change_by`, que sí sabe
    // quién causó el cambio.

    fn note_change_by(&self, peer_uid: &str) {
        let Ok(mut c) = self.changes.lock() else { return };
        c.rev += 1;
        let rev = c.rev;
        c.recent.push_back((rev, peer_uid.to_string()));
        while c.recent.len() > REMEMBER {
            c.recent.pop_front();
        }
        // A todos los que esperan: puede haber varios dispositivos a la vez, y
        // cada uno decide por su cuenta si el cambio le interesa.
        self.changed.notify_all();
    }

    fn revision(&self) -> Option<Mark> {
        self.changes.lock().ok().map(|c| c.mark())
    }

    fn wait_revision(&self, since: Mark, ignoring: &str, max: Duration) -> Seen {
        let Ok(changes) = self.changes.lock() else {
            return Seen { news: false, mark: since };
        };
        match self
            .changed
            .wait_timeout_while(changes, max, |c| !c.news_for(since.rev, ignoring))
        {
            // Las dos respuestas salen del mismo guard: si la revisión se
            // leyera después de soltarlo, un cambio entrado en el medio se
            // colaría dentro de la marca que se manda como "estás al día".
            Ok((c, _)) => Seen {
                news: c.news_for(since.rev, ignoring),
                mark: c.mark(),
            },
            Err(_) => Seen { news: false, mark: since },
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const PC: &str = "uid-pc";
    const CELU: &str = "uid-celu";

    fn host() -> ServerHost {
        ServerHost::new(Connection::open_in_memory().unwrap(), PathBuf::from("."))
    }

    /// Sin cambios, la espera se cumple sola. Es lo que después se traduce en
    /// un latido: la conexión sigue viva y no se avisó nada.
    #[test]
    fn sin_cambios_la_espera_vence() {
        let h = host();
        let rev = h.revision().unwrap();
        assert!(!h.wait_revision(rev, PC, Duration::from_millis(80)).news);
    }

    /// El cambio de otro corta la espera enseguida, sin agotar el plazo.
    #[test]
    fn el_cambio_de_otro_corta_la_espera() {
        let h = std::sync::Arc::new(host());
        let rev = h.revision().unwrap();
        let bg = std::sync::Arc::clone(&h);
        std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(50));
            bg.note_change_by(CELU);
        });
        let start = std::time::Instant::now();
        assert!(h.wait_revision(rev, PC, Duration::from_secs(10)).news);
        assert!(start.elapsed() < Duration::from_secs(5), "esperó el plazo entero");
    }

    /// Lo que empujó uno mismo no es novedad para uno mismo. Sin esto, cada
    /// cambio local termina en un sync de vuelta contra el archivo que no trae
    /// nada.
    #[test]
    fn lo_propio_no_despierta_a_nadie() {
        let h = host();
        let rev = h.revision().unwrap();
        h.note_change_by(PC);
        assert!(!h.wait_revision(rev, PC, Duration::from_millis(80)).news);
        // Al otro sí le interesa.
        assert!(h.wait_revision(rev, CELU, Duration::from_millis(80)).news);
    }

    /// Un cambio ocurrido ANTES de empezar a esperar también cuenta: la
    /// referencia la pone quien espera. Sin esto, un cambio caído entre dos
    /// latidos no despertaría a nadie.
    #[test]
    fn un_cambio_anterior_a_la_espera_tambien_cuenta() {
        let h = host();
        let rev = h.revision().unwrap();
        h.note_change_by(CELU);
        assert!(h.wait_revision(rev, PC, Duration::from_millis(80)).news);
    }

    /// Lo que se contesta como "estás al día" y la revisión que se manda salen
    /// de la misma mirada. Si se leyeran por separado, el latido podría decir
    /// "al día en la revisión N" con una novedad ya adentro de N, y quien
    /// espera avanzaría su referencia por encima de algo que nunca le llegó.
    #[test]
    fn la_revision_que_se_informa_es_la_que_se_miro() {
        let h = host();
        let rev = h.revision().unwrap();
        h.note_change_by(PC);
        h.note_change_by(PC);
        let seen = h.wait_revision(rev, PC, Duration::from_millis(80));
        assert!(!seen.news, "eran propios");
        assert_eq!(
            seen.mark.rev,
            rev.rev + 2,
            "la revisión informada tiene que ser la actual"
        );
        assert_eq!(seen.mark.epoch, rev.epoch, "la corrida no cambia sola");
    }

    /// Con más cambios de los que se recuerdan ya no se sabe de quién era cada
    /// uno, así que se avisa igual.
    #[test]
    fn cuando_se_olvida_lo_viejo_se_avisa_igual() {
        let h = host();
        let rev = h.revision().unwrap();
        for _ in 0..REMEMBER + 1 {
            h.note_change_by(PC);
        }
        assert!(h.wait_revision(rev, PC, Duration::from_millis(80)).news);
    }
}

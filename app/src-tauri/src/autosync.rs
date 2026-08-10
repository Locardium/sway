//! Sincronización automática.
//!
//! Un sync que hay que pedir a mano no sirve para el caso real: importás una
//! canción en la PC y querés encontrarla en el celular sin acordarte de nada.
//!
//! Se dispara por tres motivos, cada uno cubriendo un agujero del anterior:
//!
//! - **Cambió algo acá** (import, playlist editada, track movido). Con un
//!   respiro de unos segundos: importar una carpeta son cientos de cambios
//!   seguidos y sincronizar en cada uno sería absurdo.
//! - **Apareció un dispositivo**. El celular estuvo sin wifi toda la tarde;
//!   cuando vuelve hay que ponerse al día sin esperar a que cambie algo más.
//! - **Cada tanto**, como red de contención por si se perdió alguno de los
//!   dos anteriores.
//!
//! El bucle infinito se corta solo: aplicar cambios del otro lado dispara su
//! propio "cambió algo", pero ese sync ya no encuentra nada que hacer y
//! termina sin volver a disparar nada.

use crate::AppState;
use std::sync::atomic::{AtomicI64, Ordering};
use std::time::Duration;
use tauri::{AppHandle, Manager};

/// Cuánto se espera desde el último cambio antes de sincronizar.
///
/// Medio segundo alcanza: las operaciones grandes ya entran como UN comando
/// (importar una carpeta, arrastrar veinte tracks a una playlist), así que
/// esto sólo junta comandos separados disparados casi juntos. Y el costo de
/// un sync de más es barato — un manifest que vuelve sin nada que hacer —
/// mientras que la espera se nota en la cara.
const QUIET_PERIOD_MS: i64 = 500;
/// Red de contención.
const PERIODIC: Duration = Duration::from_secs(10 * 60);
/// Sumado al respiro, es el techo de lo que tarda en arrancar un sync.
const TICK: Duration = Duration::from_millis(250);
/// Cada cuánto se reintenta cuando hay un cambio para propagar pero no hay
/// ningún dispositivo disponible.
///
/// Sin esto, lo pendiente **no se limpia** (a propósito: un cambio hecho con
/// el celular apagado tiene que viajar cuando vuelva), así que el bucle se
/// daba por despierto en cada tick: cuatro veces por segundo tomaba el lock de
/// la DB para listar dispositivos, escribía una línea de log, y volvía a
/// empezar — para nada. Contra el mismo lock que necesita la UI. Y si el peer
/// estaba marcado offline, el cambio recién salía en la red de contención de
/// 10 minutos.
const RETRY_WHEN_ALONE: Duration = Duration::from_secs(15);

#[derive(Default)]
pub struct AutoSync {
    /// Momento del último cambio local sin sincronizar. 0 = nada pendiente.
    pending_since: AtomicI64,
}

impl AutoSync {
    /// Lo llama cualquier cosa que modifique la biblioteca.
    pub fn note_change(&self) {
        self.pending_since
            .store(crate::db::now_ms(), Ordering::Relaxed);
    }

    #[cfg(test)]
    fn pending(&self) -> i64 {
        self.pending_since.load(Ordering::Relaxed)
    }

    /// Hay un cambio pendiente y ya pasó el respiro. No lo consume: recién
    /// se limpia cuando se pudo mandar a alguien, así un cambio hecho con el
    /// celular apagado no se pierde.
    fn is_settled(&self) -> bool {
        let since = self.pending_since.load(Ordering::Relaxed);
        since != 0 && crate::db::now_ms() - since >= QUIET_PERIOD_MS
    }

    fn clear(&self) {
        self.pending_since.store(0, Ordering::Relaxed);
    }
}

pub fn enabled(conn: &rusqlite::Connection) -> bool {
    crate::db::get_setting(conn, "auto_sync_p2p")
        .ok()
        .flatten()
        .map(|v| v == "1")
        .unwrap_or(true)
}

pub fn set_enabled(conn: &rusqlite::Connection, on: bool) -> rusqlite::Result<()> {
    crate::db::set_setting(conn, "auto_sync_p2p", if on { "1" } else { "0" })
}

/// Aviso de que algo cambió en esta biblioteca.
pub fn note_change(handle: &AppHandle) {
    log::info!("[autosync] cambio local anotado");
    handle.state::<AppState>().autosync.note_change();
}

/// Un dispositivo volvió a estar disponible: ponerse al día.
///
/// Se llama desde dos lados, porque ninguno cubre al otro: el sondeo TCP ve
/// al que estaba offline y volvió, y el descubrimiento ve al que apareció en
/// la red por primera vez (ese nace `online`, así que no hay transición que
/// sondear). Filtra los no vinculados acá para que ninguno de los dos tenga
/// que acordarse.
pub fn peer_came_online(handle: &AppHandle, uid: &str) {
    let state = handle.state::<AppState>();
    {
        let guard = state.db.lock();
        let Ok(conn) = guard else { return };
        if !enabled(&conn) {
            return;
        }
        let paired = conn
            .query_row("SELECT 1 FROM devices WHERE uid = ?1", [uid], |_| Ok(()))
            .is_ok();
        if !paired {
            return;
        }
    }
    log::info!("[autosync] {uid} está disponible: poniéndose al día");
    crate::pairing::sync_files_auto(handle.clone(), uid.to_string());
}

pub fn spawn(handle: AppHandle) {
    std::thread::spawn(move || {
        let mut since_periodic = std::time::Instant::now();
        // Antes de este momento no se vuelve a intentar propagar. Sólo se
        // mueve cuando un intento no encontró a nadie.
        let mut next_try = std::time::Instant::now();
        loop {
            std::thread::sleep(TICK);
            let state = handle.state::<AppState>();

            let due_periodic = since_periodic.elapsed() >= PERIODIC;
            let due_change = state.autosync.is_settled() && std::time::Instant::now() >= next_try;
            if !due_periodic && !due_change {
                continue;
            }
            if due_periodic {
                since_periodic = std::time::Instant::now();
            }

            let peers: Vec<String> = {
                let guard = state.db.lock();
                let Ok(conn) = guard else {
                    log::warn!("[autosync] no se pudo tomar el lock de la DB");
                    continue;
                };
                if !enabled(&conn) {
                    log::debug!("[autosync] apagado por configuracion");
                    continue;
                }
                state
                    .peers
                    .merged_list(&conn)
                    .into_iter()
                    .filter(|p| p.paired && p.online)
                    .map(|p| p.uid)
                    .collect()
            };
            if peers.is_empty() {
                // No se limpia lo pendiente: un cambio hecho con el otro
                // dispositivo apagado tiene que viajar cuando vuelva, no
                // perderse acá. Pero se espera antes de volver a mirar, o esto
                // es un bucle ocupado contra el lock de la DB.
                next_try = std::time::Instant::now() + RETRY_WHEN_ALONE;
                // Distinguir los dos casos: acá se llega tanto con algo
                // pendiente como en la pasada periódica sin nada que hacer, y
                // decir siempre "hay cambios para propagar" hace perder tiempo
                // leyendo el log.
                if due_change {
                    log::info!(
                        "[autosync] hay cambios para propagar pero ningun dispositivo vinculado esta disponible"
                    );
                } else {
                    log::debug!("[autosync] ningun dispositivo vinculado disponible");
                }
                // Puede estar marcado offline por un sondeo viejo: preguntar de
                // nuevo ahora en vez de esperar la red de contención.
                let h = handle.clone();
                std::thread::spawn(move || crate::discovery::probe_once(&h));
                continue;
            }
            next_try = std::time::Instant::now();
            if due_change {
                state.autosync.clear();
                log::info!("[autosync] cambios locales -> {} dispositivo(s)", peers.len());
            }
            for uid in peers {
                crate::pairing::sync_files_auto(handle.clone(), uid);
            }
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_change_waits_for_the_quiet_period() {
        let a = AutoSync::default();
        assert!(!a.is_settled(), "sin cambios no hay nada que hacer");

        a.note_change();
        assert!(!a.is_settled(), "recién cambió: hay que esperar");

        // Simula que pasó el respiro.
        a.pending_since
            .store(crate::db::now_ms() - QUIET_PERIOD_MS - 1, Ordering::Relaxed);
        assert!(a.is_settled());
    }

    /// Importar una carpeta son cientos de cambios seguidos: cada uno reinicia
    /// la espera para que salga un solo sync al final.
    #[test]
    fn a_new_change_restarts_the_wait() {
        let a = AutoSync::default();
        a.pending_since
            .store(crate::db::now_ms() - QUIET_PERIOD_MS - 1, Ordering::Relaxed);
        assert!(a.is_settled());
        a.note_change();
        assert!(!a.is_settled());
    }

    /// Lo pendiente no se consume al mirarlo: si no hay a quién mandárselo,
    /// tiene que seguir pendiente hasta que aparezca un dispositivo.
    #[test]
    fn checking_does_not_consume_the_pending_change() {
        let a = AutoSync::default();
        a.pending_since
            .store(crate::db::now_ms() - QUIET_PERIOD_MS - 1, Ordering::Relaxed);
        assert!(a.is_settled());
        assert!(a.is_settled(), "mirarlo dos veces no lo borra");
        assert_ne!(a.pending(), 0);
        a.clear();
        assert!(!a.is_settled());
    }
}

//! Lo que sabe de vinculación un dispositivo sin pantalla (Fase 6.1).
//!
//! La ceremonia de pairing tiene dos mitades bien distintas. Una es
//! criptografía y filas en la base: qué clave tiene cada dispositivo, si la que
//! llega es la que ya teníamos, y qué se guarda cuando el vínculo se concreta.
//! La otra es una persona mirando dos pantallas y comparando seis dígitos.
//!
//! Acá vive la primera. La segunda sigue en `app/src-tauri/src/pairing.rs`,
//! porque necesita una ventana donde emitir eventos y alguien que los mire.
//!
//! La separación no es prolijidad: el server de archivo (Fase 6.2) corre en un
//! Ubuntu sin nadie adelante y necesita exactamente esta mitad — verificar
//! claves, rechazar la que no coincide, dar de alta el dispositivo — mientras
//! reemplaza la otra por un token de configuración.
//!
//! **Regla que no cambia de un lado ni del otro:** una clave distinta para un
//! uid ya conocido se rechaza y se registra. Nunca se vuelve a confiar en
//! silencio.

use crate::db;
use anyhow::Result;
use base64::Engine as _;
use rusqlite::Connection;
use std::time::Duration;

/// Cuánto se espera para abrir una conexión con otro dispositivo.
pub const CONNECT_TIMEOUT: Duration = Duration::from_secs(5);
/// Generoso a propósito: del otro lado puede haber alguien todavía decidiendo.
pub const IO_TIMEOUT: Duration = Duration::from_secs(180);

const SETTING_PRIVKEY: &str = "noise_private";
const SETTING_PUBKEY: &str = "noise_public";

// ---------------------------------------------------------------------------
// Identidad criptográfica de este dispositivo
// ---------------------------------------------------------------------------

/// Par de claves estático, generado una sola vez. Vive en `app_settings`, o
/// sea en la base (en Android, almacenamiento privado del paquete).
pub fn keypair(conn: &Connection) -> Result<(Vec<u8>, Vec<u8>)> {
    let b64 = base64::engine::general_purpose::STANDARD;
    if let (Some(priv_b64), Some(pub_b64)) = (
        db::get_setting(conn, SETTING_PRIVKEY)?,
        db::get_setting(conn, SETTING_PUBKEY)?,
    ) {
        if let (Ok(pv), Ok(pb)) = (b64.decode(&priv_b64), b64.decode(&pub_b64)) {
            return Ok((pv, pb));
        }
    }
    let (private, public) = crate::wire::generate_keypair()?;
    db::set_setting(conn, SETTING_PRIVKEY, &b64.encode(&private))?;
    db::set_setting(conn, SETTING_PUBKEY, &b64.encode(&public))?;
    log::info!("[pair] new key pair generated");
    Ok((private, public))
}

// ---------------------------------------------------------------------------
// Estado de `devices`
// ---------------------------------------------------------------------------

pub enum Known {
    /// Ya pareado y la clave coincide.
    Trusted,
    /// Nunca se pareó con este uid.
    Unknown,
    /// Conocido pero con OTRA clave pública. Alarma, no rutina.
    KeyMismatch,
}

pub fn known_state(conn: &Connection, uid: &str, pubkey: &[u8]) -> Known {
    let stored: Option<Option<Vec<u8>>> = conn
        .query_row("SELECT pubkey FROM devices WHERE uid = ?1", [uid], |r| {
            r.get(0)
        })
        .ok();
    match stored {
        Some(Some(k)) if k == pubkey => Known::Trusted,
        Some(Some(_)) => Known::KeyMismatch,
        _ => Known::Unknown,
    }
}

pub fn store_device(
    conn: &Connection,
    uid: &str,
    name: &str,
    platform: &str,
    pubkey: &[u8],
) -> Result<()> {
    let now = db::now_ms();
    conn.execute(
        "INSERT INTO devices (uid, name, platform, pubkey, paired_at, last_seen)
         VALUES (?1, ?2, ?3, ?4, ?5, ?5)
         ON CONFLICT(uid) DO UPDATE SET
            name = excluded.name, platform = excluded.platform,
            pubkey = excluded.pubkey, paired_at = excluded.paired_at,
            last_seen = excluded.last_seen",
        rusqlite::params![uid, name, platform, pubkey, now],
    )?;
    conn.execute(
        "INSERT INTO sync_log (ts, peer, kind, detail) VALUES (?1, ?2, 'paired', ?3)",
        rusqlite::params![now, uid, name],
    )?;
    Ok(())
}

/// Dirección fija de un dispositivo que no se descubre solo (el server).
///
/// Va aparte del alta porque el alta la comparten los dos caminos y sólo uno
/// de los dos tiene una dirección que valga guardar: la de un peer de la LAN
/// cambia con el DHCP, y la que vale es la que anuncia por mDNS.
pub fn set_device_address(conn: &Connection, uid: &str, address: &str) -> Result<()> {
    conn.execute(
        "UPDATE devices SET address = ?1 WHERE uid = ?2",
        rusqlite::params![address, uid],
    )?;
    Ok(())
}

/// Hasta dónde sabemos de la biblioteca de ese dispositivo.
///
/// Guardarla es lo que evita comparar las dos bibliotecas enteras cada vez que
/// arranca la app: con la marca en la mano se pregunta "¿pasó algo desde
/// acá?", y si no pasó nada no viaja nada. En un celular, donde el sistema
/// mata la app cuando se le antoja, eso es la diferencia entre unas cuantas
/// comparaciones completas por día y ninguna.
pub fn watch_mark(conn: &Connection, uid: &str) -> Option<crate::wire::Mark> {
    conn.query_row(
        "SELECT watch_epoch, watch_rev FROM devices WHERE uid = ?1",
        [uid],
        |r| {
            Ok(match (r.get::<_, Option<i64>>(0)?, r.get::<_, Option<i64>>(1)?) {
                (Some(epoch), Some(rev)) => Some(crate::wire::Mark {
                    epoch: epoch as u64,
                    rev: rev as u64,
                }),
                _ => None,
            })
        },
    )
    .ok()
    .flatten()
}

/// Guarda (o borra, con `None`) la marca.
pub fn set_watch_mark(
    conn: &Connection,
    uid: &str,
    mark: Option<crate::wire::Mark>,
) -> Result<()> {
    let (epoch, rev) = match mark {
        Some(m) => (Some(m.epoch as i64), Some(m.rev as i64)),
        None => (None, None),
    };
    conn.execute(
        "UPDATE devices SET watch_epoch = ?1, watch_rev = ?2 WHERE uid = ?3",
        rusqlite::params![epoch, rev, uid],
    )?;
    Ok(())
}

/// Los que tienen dirección fija: `(uid, name, platform, address)`.
pub fn devices_with_address(conn: &Connection) -> Vec<(String, String, String, String)> {
    let mut stmt = match conn.prepare(
        "SELECT uid, name, platform, address FROM devices
         WHERE address IS NOT NULL AND address <> ''",
    ) {
        Ok(s) => s,
        Err(_) => return Vec::new(),
    };
    let rows = stmt.query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)));
    match rows {
        Ok(rows) => rows.filter_map(|r| r.ok()).collect(),
        Err(_) => Vec::new(),
    }
}

pub fn forget_device(conn: &Connection, uid: &str) -> Result<()> {
    conn.execute("DELETE FROM devices WHERE uid = ?1", [uid])?;
    Ok(())
}

/// "Lo vi recién, y se llama así". Lo escribe cada `Hello`.
pub fn touch_device(conn: &Connection, uid: &str, name: &str) {
    let _ = conn.execute(
        "UPDATE devices SET last_seen = ?1, name = ?2 WHERE uid = ?3",
        rusqlite::params![db::now_ms(), name, uid],
    );
}

/// Una clave distinta para un uid conocido puede ser una reinstalación del otro
/// lado — o alguien haciéndose pasar por él. No se resuelve solo: queda
/// registrado y hay que desvincular a mano para volver a parear.
pub fn log_key_mismatch(conn: &Connection, uid: &str, name: &str) {
    log::warn!("[pair] different key for {name} ({uid}) - connection rejected");
    let _ = conn.execute(
        "INSERT INTO sync_log (ts, peer, kind, detail) VALUES (?1, ?2, 'key-mismatch', ?3)",
        rusqlite::params![db::now_ms(), uid, name],
    );
}

// ---------------------------------------------------------------------------
// Presentación
// ---------------------------------------------------------------------------

pub fn library_counts(conn: &Connection) -> (i64, i64) {
    let tracks = conn
        .query_row("SELECT COUNT(*) FROM tracks", [], |r| r.get(0))
        .unwrap_or(0);
    let playlists = conn
        .query_row(
            "SELECT COUNT(*) FROM playlists WHERE kind = 'playlist'",
            [],
            |r| r.get(0),
        )
        .unwrap_or(0);
    (tracks, playlists)
}

/// Quién soy: uid estable y nombre visible.
pub fn me(conn: &Connection) -> Result<(String, String)> {
    let uid = db::this_device_uid(conn)?;
    let name = db::device_name(conn)?;
    Ok((uid, name))
}

/// En qué corro. `server` no sale de acá: lo declara el binario headless, que
/// no es ninguna de estas plataformas a los efectos de la UI del otro lado.
pub fn platform() -> String {
    if cfg!(target_os = "android") {
        "android".into()
    } else if cfg!(target_os = "windows") {
        "windows".into()
    } else if cfg!(target_os = "macos") {
        "macos".into()
    } else {
        "linux".into()
    }
}

/// Plataforma que declara el server de archivo. La UI la usa para mostrarlo
/// distinto de un dispositivo con pantalla y para no esperar que aparezca en
/// mDNS.
pub const PLATFORM_SERVER: &str = "server";

// ---------------------------------------------------------------------------
// Token de vinculación (dispositivos sin pantalla)
// ---------------------------------------------------------------------------

/// Compara dos secretos sin cortar en la primera diferencia.
///
/// El código de 6 dígitos lo compara una persona; un token lo compara el
/// server, y ahí sí importa cuánto tarda en decir que no: con una comparación
/// que corta temprano, el tiempo de respuesta filtra cuántos caracteres del
/// principio son correctos, y el token se adivina de a uno.
pub fn secret_eq(a: &str, b: &str) -> bool {
    let (a, b) = (a.as_bytes(), b.as_bytes());
    if a.len() != b.len() {
        return false;
    }
    let mut diff = 0u8;
    for i in 0..a.len() {
        diff |= a[i] ^ b[i];
    }
    diff == 0
}

#[cfg(test)]
mod tests {
    use super::*;

    fn db() -> Connection {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch("PRAGMA foreign_keys=ON;").unwrap();
        db::init_schema(&conn).unwrap();
        conn
    }

    #[test]
    fn el_par_de_claves_se_genera_una_sola_vez() {
        let conn = db();
        let (priv1, pub1) = keypair(&conn).unwrap();
        let (priv2, pub2) = keypair(&conn).unwrap();
        assert_eq!(priv1, priv2, "regenerar la clave privada rompe todo vínculo");
        assert_eq!(pub1, pub2);
        assert!(!priv1.is_empty());
    }

    #[test]
    fn una_clave_distinta_para_un_uid_conocido_no_pasa_por_confiada() {
        let conn = db();
        store_device(&conn, "peer-1", "Celu", "android", b"clave-original").unwrap();

        assert!(matches!(
            known_state(&conn, "peer-1", b"clave-original"),
            Known::Trusted
        ));
        assert!(matches!(
            known_state(&conn, "peer-1", b"otra-clave"),
            Known::KeyMismatch
        ));
        assert!(matches!(
            known_state(&conn, "peer-2", b"clave-original"),
            Known::Unknown
        ));
    }

    #[test]
    fn desvincular_saca_al_dispositivo_de_los_confiados() {
        let conn = db();
        store_device(&conn, "peer-1", "Celu", "android", b"k").unwrap();
        forget_device(&conn, "peer-1").unwrap();
        assert!(matches!(known_state(&conn, "peer-1", b"k"), Known::Unknown));
    }

    #[test]
    fn el_pairing_queda_registrado() {
        let conn = db();
        store_device(&conn, "peer-1", "Celu", "android", b"k").unwrap();
        let logged: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM sync_log WHERE peer = 'peer-1' AND kind = 'paired'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(logged, 1);
    }

    #[test]
    fn la_direccion_fija_se_guarda_y_se_recupera() {
        let conn = db();
        store_device(&conn, "srv-1", "Server", PLATFORM_SERVER, b"k").unwrap();
        store_device(&conn, "celu", "Celu", "android", b"k2").unwrap();
        assert!(devices_with_address(&conn).is_empty());

        set_device_address(&conn, "srv-1", "casa.ejemplo:7420").unwrap();
        let listed = devices_with_address(&conn);
        assert_eq!(listed.len(), 1, "el peer de la LAN no tiene que tener dirección");
        assert_eq!(listed[0].0, "srv-1");
        assert_eq!(listed[0].3, "casa.ejemplo:7420");
    }

    /// La marca sobrevive al cierre de la app: es lo que evita comparar las
    /// dos bibliotecas enteras en cada arranque.
    #[test]
    fn la_marca_se_guarda_y_se_recupera() {
        let conn = db();
        store_device(&conn, "srv-1", "Server", PLATFORM_SERVER, b"k").unwrap();
        assert!(watch_mark(&conn, "srv-1").is_none(), "todavía no sabemos nada");

        let m = crate::wire::Mark { epoch: 7, rev: 42 };
        set_watch_mark(&conn, "srv-1", Some(m)).unwrap();
        assert_eq!(watch_mark(&conn, "srv-1"), Some(m));

        set_watch_mark(&conn, "srv-1", None).unwrap();
        assert!(watch_mark(&conn, "srv-1").is_none(), "se tiene que poder borrar");
    }

    /// Y se va con el dispositivo: dejarla ahí haría que volver a vincular el
    /// mismo server arranque creyendo que sabe algo de una biblioteca que ya
    /// no tiene nada que ver.
    #[test]
    fn desvincular_se_lleva_la_marca() {
        let conn = db();
        store_device(&conn, "srv-1", "Server", PLATFORM_SERVER, b"k").unwrap();
        set_watch_mark(&conn, "srv-1", Some(crate::wire::Mark { epoch: 1, rev: 2 })).unwrap();
        forget_device(&conn, "srv-1").unwrap();
        store_device(&conn, "srv-1", "Server", PLATFORM_SERVER, b"k").unwrap();
        assert!(watch_mark(&conn, "srv-1").is_none());
    }

    #[test]
    fn desvincular_el_server_se_lleva_su_direccion() {
        let conn = db();
        store_device(&conn, "srv-1", "Server", PLATFORM_SERVER, b"k").unwrap();
        set_device_address(&conn, "srv-1", "casa.ejemplo:7420").unwrap();
        forget_device(&conn, "srv-1").unwrap();
        assert!(devices_with_address(&conn).is_empty());
    }

    #[test]
    fn el_token_se_compara_entero() {
        assert!(secret_eq("abc123", "abc123"));
        assert!(!secret_eq("abc123", "abc124"));
        assert!(!secret_eq("abc123", "abc12"));
        assert!(!secret_eq("", "x"));
        assert!(secret_eq("", ""));
    }
}

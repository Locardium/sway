//! What a screenless device knows about pairing (Phase 6.1).
//!
//! The pairing ceremony has two quite distinct halves. One is cryptography
//! and database rows: which key each device has, whether the incoming one
//! matches what we already had, and what gets saved when the link is
//! confirmed. The other is a person looking at two screens and comparing six
//! digits.
//!
//! This is where the first half lives. The second one still lives in
//! `app/src-tauri/src/pairing.rs`, because it needs a window to emit events
//! to and someone watching them.
//!
//! The separation isn't tidiness for its own sake: the file server (Phase
//! 6.2) runs on an Ubuntu box with nobody in front of it and needs exactly
//! this half — verifying keys, rejecting the one that doesn't match, enrolling
//! the device — while it replaces the other half with a configuration token.
//!
//! **Rule that doesn't change on either side:** a different key for an
//! already-known uid is rejected and logged. Trust is never silently
//! restored.

use crate::db;
use anyhow::Result;
use base64::Engine as _;
use rusqlite::Connection;
use std::time::Duration;

/// How long to wait when opening a connection to another device.
pub const CONNECT_TIMEOUT: Duration = Duration::from_secs(5);
/// Deliberately generous: someone on the other end may still be deciding.
pub const IO_TIMEOUT: Duration = Duration::from_secs(180);

const SETTING_PRIVKEY: &str = "noise_private";
const SETTING_PUBKEY: &str = "noise_public";

// ---------------------------------------------------------------------------
// This device's cryptographic identity
// ---------------------------------------------------------------------------

/// Static key pair, generated once. Lives in `app_settings`, i.e. in the
/// database (on Android, the package's private storage).
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
// `devices` state
// ---------------------------------------------------------------------------

pub enum Known {
    /// Already paired and the key matches.
    Trusted,
    /// Never paired with this uid.
    Unknown,
    /// Known but with a DIFFERENT public key. An alarm, not routine.
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

/// Fixed address of a device that can't discover itself (the server).
///
/// Kept separate from enrollment because enrollment is shared by both paths,
/// and only one of them has an address worth saving: a LAN peer's address
/// changes with DHCP, and the one that matters is the one it announces over
/// mDNS.
pub fn set_device_address(conn: &Connection, uid: &str, address: &str) -> Result<()> {
    conn.execute(
        "UPDATE devices SET address = ?1 WHERE uid = ?2",
        rusqlite::params![address, uid],
    )?;
    Ok(())
}

/// How far we know that device's library to be.
///
/// Saving it is what avoids comparing the two full libraries every time the
/// app starts: with the mark in hand, it asks "did anything happen since
/// here?", and if nothing did, nothing travels. On a phone, where the system
/// kills the app whenever it feels like it, that's the difference between a
/// handful of full comparisons a day and none at all.
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

/// Saves (or clears, with `None`) the mark.
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

/// The ones with a fixed address: `(uid, name, platform, address)`.
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

/// "Just saw it, and it's called that." Written on every `Hello`.
pub fn touch_device(conn: &Connection, uid: &str, name: &str) {
    let _ = conn.execute(
        "UPDATE devices SET last_seen = ?1, name = ?2 WHERE uid = ?3",
        rusqlite::params![db::now_ms(), name, uid],
    );
}

/// A different key for a known uid could be a reinstall on the other end —
/// or someone impersonating it. It doesn't resolve itself: it gets logged,
/// and unpairing by hand is required to pair again.
pub fn log_key_mismatch(conn: &Connection, uid: &str, name: &str) {
    log::warn!("[pair] different key for {name} ({uid}) - connection rejected");
    let _ = conn.execute(
        "INSERT INTO sync_log (ts, peer, kind, detail) VALUES (?1, ?2, 'key-mismatch', ?3)",
        rusqlite::params![db::now_ms(), uid, name],
    );
}

// ---------------------------------------------------------------------------
// Presentation
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

/// Who am I: stable uid and visible name.
pub fn me(conn: &Connection) -> Result<(String, String)> {
    let uid = db::this_device_uid(conn)?;
    let name = db::device_name(conn)?;
    Ok((uid, name))
}

/// What platform I'm running on. `server` doesn't come out of here: it's
/// declared by the headless binary, which isn't any of these platforms as
/// far as the UI on the other end is concerned.
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

/// Platform declared by the file server. The UI uses it to show it
/// differently from a device with a screen, and to not expect it to show up
/// on mDNS.
pub const PLATFORM_SERVER: &str = "server";

// ---------------------------------------------------------------------------
// Pairing token (screenless devices)
// ---------------------------------------------------------------------------

/// Compares two secrets without short-circuiting on the first difference.
///
/// The 6-digit code is compared by a person; a token is compared by the
/// server, and there it does matter how long it takes to say no: with a
/// comparison that short-circuits early, the response time leaks how many
/// characters from the start are correct, and the token gets guessed one
/// character at a time.
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
    fn the_key_pair_is_generated_only_once() {
        let conn = db();
        let (priv1, pub1) = keypair(&conn).unwrap();
        let (priv2, pub2) = keypair(&conn).unwrap();
        assert_eq!(priv1, priv2, "regenerating the private key breaks every link");
        assert_eq!(pub1, pub2);
        assert!(!priv1.is_empty());
    }

    #[test]
    fn a_different_key_for_a_known_uid_is_not_trusted() {
        let conn = db();
        store_device(&conn, "peer-1", "Phone", "android", b"original-key").unwrap();

        assert!(matches!(
            known_state(&conn, "peer-1", b"original-key"),
            Known::Trusted
        ));
        assert!(matches!(
            known_state(&conn, "peer-1", b"other-key"),
            Known::KeyMismatch
        ));
        assert!(matches!(
            known_state(&conn, "peer-2", b"original-key"),
            Known::Unknown
        ));
    }

    #[test]
    fn unpairing_removes_the_device_from_trusted() {
        let conn = db();
        store_device(&conn, "peer-1", "Phone", "android", b"k").unwrap();
        forget_device(&conn, "peer-1").unwrap();
        assert!(matches!(known_state(&conn, "peer-1", b"k"), Known::Unknown));
    }

    #[test]
    fn pairing_gets_logged() {
        let conn = db();
        store_device(&conn, "peer-1", "Phone", "android", b"k").unwrap();
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
    fn the_fixed_address_is_saved_and_retrieved() {
        let conn = db();
        store_device(&conn, "srv-1", "Server", PLATFORM_SERVER, b"k").unwrap();
        store_device(&conn, "phone", "Phone", "android", b"k2").unwrap();
        assert!(devices_with_address(&conn).is_empty());

        set_device_address(&conn, "srv-1", "home.example:7420").unwrap();
        let listed = devices_with_address(&conn);
        assert_eq!(listed.len(), 1, "the LAN peer shouldn't have an address");
        assert_eq!(listed[0].0, "srv-1");
        assert_eq!(listed[0].3, "home.example:7420");
    }

    /// The mark survives the app closing: it's what avoids comparing the two
    /// full libraries on every startup.
    #[test]
    fn the_mark_is_saved_and_retrieved() {
        let conn = db();
        store_device(&conn, "srv-1", "Server", PLATFORM_SERVER, b"k").unwrap();
        assert!(watch_mark(&conn, "srv-1").is_none(), "we don't know anything yet");

        let m = crate::wire::Mark { epoch: 7, rev: 42 };
        set_watch_mark(&conn, "srv-1", Some(m)).unwrap();
        assert_eq!(watch_mark(&conn, "srv-1"), Some(m));

        set_watch_mark(&conn, "srv-1", None).unwrap();
        assert!(watch_mark(&conn, "srv-1").is_none(), "it must be possible to clear it");
    }

    /// And it leaves with the device: leaving it there would make repairing
    /// the same server start out believing it knows something about a
    /// library that has nothing to do with it anymore.
    #[test]
    fn unpairing_takes_the_mark_with_it() {
        let conn = db();
        store_device(&conn, "srv-1", "Server", PLATFORM_SERVER, b"k").unwrap();
        set_watch_mark(&conn, "srv-1", Some(crate::wire::Mark { epoch: 1, rev: 2 })).unwrap();
        forget_device(&conn, "srv-1").unwrap();
        store_device(&conn, "srv-1", "Server", PLATFORM_SERVER, b"k").unwrap();
        assert!(watch_mark(&conn, "srv-1").is_none());
    }

    #[test]
    fn unpairing_the_server_takes_its_address_with_it() {
        let conn = db();
        store_device(&conn, "srv-1", "Server", PLATFORM_SERVER, b"k").unwrap();
        set_device_address(&conn, "srv-1", "home.example:7420").unwrap();
        forget_device(&conn, "srv-1").unwrap();
        assert!(devices_with_address(&conn).is_empty());
    }

    #[test]
    fn the_token_is_compared_in_full() {
        assert!(secret_eq("abc123", "abc123"));
        assert!(!secret_eq("abc123", "abc124"));
        assert!(!secret_eq("abc123", "abc12"));
        assert!(!secret_eq("", "x"));
        assert!(secret_eq("", ""));
    }
}

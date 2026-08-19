//! Sway's archive and sync server.
//!
//! Stores whatever devices send it and hands it back when they ask for it.
//! It doesn't import music on its own and has no interface: everything it
//! has, someone sent it.
//!
//! What it's for, in two concrete cases:
//!
//! - **Syncing away from home.** mDNS discovery only sees the local network;
//!   two devices on different networks can't find each other or call each
//!   other (both are behind a NAT). Against a server with a public address,
//!   on the other hand, both dial out — and since the server has everything,
//!   neither needs the other to be online.
//! - **Recovering.** If every device's library is lost, the files and the
//!   organization are still here.

use anyhow::{Context, Result};
use std::net::TcpListener;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use sway_core::{db, engine::Host, pairing};
use sway_server::config::Config;
use sway_server::host::ServerHost;
use sway_server::serve;

const DEFAULT_CONFIG: &str = "config.toml";
/// The previous name. A server that's already running still has this one,
/// and starting with the new name would write a fresh config —with ANOTHER
/// token— and shut down without ever listening, which under systemd is a
/// silent outage. It's still accepted, with a warning.
const LEGACY_CONFIG: &str = "sway-server.toml";

fn main() {
    let code = match run() {
        Ok(()) => 0,
        Err(e) => {
            eprintln!("Error: {e:#}");
            1
        }
    };
    // The last thing that happens, whether it went well or not: if this
    // window was opened by a double click, closing here would take the
    // printed message with it — which on the first run is exactly where the
    // token ended up.
    hold_window_open();
    std::process::exit(code);
}

/// Keeps the window open until someone presses Enter, **only** if the
/// console belongs to this process.
///
/// The distinction matters: in a terminal or under systemd nobody is going
/// to press anything, and a server that starts up waiting for a keypress
/// never starts. Windows tells us by counting how many processes share the
/// console — if it's just one, this program created it by opening, meaning
/// it was a double click.
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
        .with_context(|| format!("could not create {}", cfg.data_dir.display()))?;
    std::fs::create_dir_all(&cfg.music_dir)
        .with_context(|| format!("could not create {}", cfg.music_dir.display()))?;

    let db_file = cfg.data_dir.join("sway.sqlite");
    let conn = db::open(&db_file).with_context(|| format!("could not open {}", db_file.display()))?;
    // WAL checkpointing runs from its own connection, without slowing down writers.
    db::spawn_checkpointer(&db_file);
    // The name comes from the config: it's what shows up in the app.
    db::set_device_name(&conn, &cfg.name)?;

    let hostage = Arc::new(ServerHost::new(conn, cfg.music_dir.clone()));
    // Generates the keypair on the first run.
    let (uid, pubkey) = hostage.with_db(|conn| {
        let (_, public) = pairing::keypair(conn)?;
        Ok((db::this_device_uid(conn)?, public))
    })?;

    // The server wants everything, in both directions, and declares it in
    // the row that gets replicated. It's not enough for it to be the
    // default: devices read that row to decide what to send it, and an
    // explicit row is also what the app shows (in gray) when you open the
    // server in the list.
    hostage.with_db(|conn| {
        let me = db::this_device_uid(conn)?;
        sway_core::scope::set_mode(conn, &me, sway_core::scope::Mode::All)?;
        sway_core::scope::set_direction(conn, &me, "both")?;
        Ok(())
    })?;

    // The trash: whatever retention has already let expire gets truly
    // deleted. Runs on startup and once a day — a server stays up for
    // months, and if it only cleaned up at startup it would never clean up
    // at all.
    spawn_trash_purge(cfg.music_dir.clone(), cfg.retention_days);

    let listener = TcpListener::bind(&cfg.listen)
        .with_context(|| format!("could not listen on {}", cfg.listen))?;

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
    // A single option, and that's why there's no argument-parsing library:
    //   sway-server [config-path]
    resolve_config(std::env::args().nth(1), Path::new("."))
}

/// Which config file to use. A hand-written path always wins; without one,
/// the new name takes over, and the old one only counts if it's the only one
/// that exists.
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

/// The first few bytes of the key, so it can be compared at a glance with
/// what the app shows if the two ever stop matching.
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
    fn with_nothing_written_it_uses_the_new_name() {
        let dir = tmp("nuevo");
        assert_eq!(resolve_config(None, &dir), dir.join(DEFAULT_CONFIG));
    }

    /// A server that was already running has the old name. If the rename
    /// were ignored, the first run would write a fresh config with ANOTHER
    /// token and shut down without listening — and under systemd that's a
    /// downed server nobody notices.
    #[test]
    fn the_old_name_still_works_if_it_is_the_only_one() {
        let dir = tmp("viejo");
        std::fs::write(dir.join(LEGACY_CONFIG), "").unwrap();
        assert_eq!(resolve_config(None, &dir), dir.join(LEGACY_CONFIG));
    }

    /// With both files present, the new one wins: it's the one the user just
    /// wrote, and the old one might just be left over from before.
    #[test]
    fn with_both_present_the_new_one_wins() {
        let dir = tmp("ambos");
        std::fs::write(dir.join(LEGACY_CONFIG), "").unwrap();
        std::fs::write(dir.join(DEFAULT_CONFIG), "").unwrap();
        assert_eq!(resolve_config(None, &dir), dir.join(DEFAULT_CONFIG));
    }

    /// A hand-written path always wins, whether it exists or not: it's what
    /// lets the config live outside the working directory (see the systemd
    /// section of the README).
    #[test]
    fn the_hand_written_path_beats_everything() {
        let dir = tmp("amano");
        std::fs::write(dir.join(DEFAULT_CONFIG), "").unwrap();
        let chosen = dir.join("something-else.toml");
        assert_eq!(
            resolve_config(Some(chosen.display().to_string()), &dir),
            chosen
        );
    }
}

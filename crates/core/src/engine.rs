//! The sync engine, with no Tauri inside (Phase 5.8).
//!
//! Until now synchronization lived glued to `AppHandle`: to read the database
//! it had to ask the app for its state, and to announce a received file it
//! had to emit a window event. That was enough to make it work, but it left
//! the hard requirement of all of Phase 5 —**never lose music**— resting on
//! manual testing with two real devices.
//!
//! Everything the engine needs from the device it runs on now comes in
//! through `Host`: the database, the managed folder, and the notices to the
//! UI. The app implements it over `AppHandle` (see `lib.rs`); the integrity
//! suite implements it over a temporary directory, and so it can spin up
//! **two engines in the same process** and make them actually synchronize
//! over loopback — files, network cuts, conflicts and deletions included.
//!
//! What does NOT go in here: pairing, discovery, and anything that needs a
//! person watching a screen. That stays in `pairing.rs`.

use crate::wire::{Mark, Msg, Session};
use anyhow::{anyhow, Result};
use rusqlite::Connection;
use std::path::{Path, PathBuf};

// ---------------------------------------------------------------------------
// The device the engine runs on
// ---------------------------------------------------------------------------

/// Progress on a file. The app turns it into a window event; the integrity
/// suite uses it to cut the network right in the middle of a transfer, which
/// is the only honest way to test resumption.
pub struct Progress<'a> {
    pub peer_uid: &'a str,
    /// Index of the current file and total files in this run.
    pub index: usize,
    pub total_files: usize,
    pub filename: &'a str,
    /// Bytes of the current file.
    pub done: u64,
    pub total: u64,
    pub sending: bool,
}

/// What the engine needs from the device it runs on.
///
/// The two ways of touching the database are closures rather than a returned
/// guard on purpose: this way the implementer decides how the lock is taken
/// (and when it's released) without the engine being able to hold onto it
/// across a long operation — **never hold the SQLite lock while hashing or
/// doing I/O**, a rule that has already bitten this project twice.
pub trait Host {
    fn with_db<T>(&self, f: impl FnOnce(&Connection) -> Result<T>) -> Result<T>;

    /// Read-only connection, for what almost always returns zero and isn't
    /// worth queuing behind an in-progress sync. Defaults to the same one.
    fn with_db_read<T>(&self, f: impl FnOnce(&Connection) -> Result<T>) -> Result<T> {
        self.with_db(f)
    }

    /// Managed folder: this is where the files, the trash, and the partials live.
    fn music_dir(&self) -> PathBuf;

    /// "I wrote this file myself": the folder watcher skips it instead of
    /// auto-importing it with a new uid. With no watcher, nothing is needed.
    fn expect_path(&self, _dest: &Path) {}

    fn progress(&self, _p: &Progress) {}

    /// The library changed. `force` distinguishes the end of a run (which
    /// always notifies) from the file-by-file trickle (which is throttled).
    fn library_changed(&self, _force: bool) {}

    /// `peer_uid` just moved this library.
    ///
    /// Separate from `library_changed` because it answers a different
    /// question: that one says "needs a redraw", this one says "the others
    /// need to be told", and for the latter it's necessary to know who NOT to
    /// tell. Without the author, the device that pushes a change wakes itself
    /// up and goes out to sync something it just sent itself.
    fn note_change_by(&self, _peer_uid: &str) {}

    /// How far this library has gotten: the current run and how many times it
    /// moved since it started.
    ///
    /// The count isn't persistent and doesn't need to be — it's only used to
    /// compare against itself within a single wait (see `Msg::Watch`) — and
    /// that's why it travels alongside the run: a restart resets it to zero,
    /// and without knowing it was a different run, that zero is
    /// indistinguishable from "nothing happened".
    ///
    /// `None` = this device doesn't know how to report changes. It's the
    /// default because only the file server makes sense to wait: between two
    /// devices with a screen, whoever changes something calls the other one
    /// and pushes it, so nobody needs to be notified.
    fn revision(&self) -> Option<Mark> {
        None
    }

    /// Waits up to `max` for a change to appear after `since` that wasn't made
    /// by `ignoring`.
    ///
    /// The reference point is set by the caller and is deliberately not
    /// re-taken here: the wait is cut short every so often to send a
    /// heartbeat, and if each round re-read the revision, a change that
    /// landed exactly between two rounds would end up on the old side of the
    /// comparison and wouldn't wake anyone up.
    fn wait_revision(&self, _since: Mark, _ignoring: &str, _max: std::time::Duration) -> Seen {
        Seen {
            news: false,
            mark: Mark { epoch: 0, rev: 0 },
        }
    }
}

/// What a wait saw: whether there's news, and what revision the library was
/// at when it looked.
///
/// Both things come from the **same** look on purpose. Reading the revision
/// separately, afterward, leaves a window where a change comes in without
/// anyone counting it: the heartbeat would say "you're up to date at revision
/// N" when N already includes something that wasn't counted, and the waiter
/// would advance its `since` past a piece of news that never reached it.
pub struct Seen {
    pub news: bool,
    pub mark: Mark,
}

/// How often a heartbeat is sent while nothing happens (`Msg::Watch`).
///
/// An open, silent connection doesn't survive on its own, and the one that
/// cuts it first isn't the proxy —Nginx's stream holds for 10 minutes— but
/// the carrier's NAT: on mobile data an idle TCP connection drops somewhere
/// between 30 and 60 seconds. It was measured: at two minutes, the phone's
/// connection lived for 17 to 47 seconds at a time.
///
/// Forty-five seconds falls under that limit and on top of that halves how
/// long it takes the server to realize nobody's on the other end anymore —
/// it only finds out when the heartbeat fails, and until then it has a thread
/// stuck talking to a dead connection.
///
/// It's still cheap: a frame of a few dozen bytes, against the TCP polling
/// every 10 seconds that this replaces.
pub const WATCH_HEARTBEAT: std::time::Duration = std::time::Duration::from_secs(45);

// ---------------------------------------------------------------------------
// Result
// ---------------------------------------------------------------------------

#[derive(Debug, Default)]
pub struct SyncResult {
    pub received: usize,
    pub sent: usize,
    pub failed: usize,
    pub bytes: u64,
    pub organized: usize,
}

/// The other side closed without saying anything: a health check, an app
/// that went away, a phone that fell asleep or switched networks.
///
/// `BrokenPipe` belongs here: it's what you get writing to a socket the other
/// side already closed, i.e. the normal end of a session being cut. Without
/// this it showed up as an error in the user's face ("broken pipe"), which
/// means nothing to whoever reads it and on top of that sounds like something
/// broke.
pub fn is_disconnect(e: &anyhow::Error) -> bool {
    use std::io::ErrorKind::*;
    e.downcast_ref::<std::io::Error>()
        .map(|io| {
            matches!(
                io.kind(),
                UnexpectedEof | ConnectionReset | ConnectionAborted | BrokenPipe | NotConnected
            )
        })
        .unwrap_or(false)
}

// ---------------------------------------------------------------------------
// Serving side
// ---------------------------------------------------------------------------

/// What happened while serving someone.
///
/// The server doesn't decide anything —it responds to requests— so without
/// this its log is a string of loose lines with no totals, and there's no way
/// to tell at a glance whether a run moved anything. On the server side,
/// which has no screen, this is the only summary that exists.
#[derive(Debug, Default)]
pub struct ServeStats {
    /// Files pushed to us.
    pub received: usize,
    /// Files requested from us and sent.
    pub sent: usize,
    pub applied: crate::merge::Applied,
}

impl ServeStats {
    pub fn moved_something(&self) -> bool {
        self.received > 0 || self.sent > 0 || self.applied.total() > 0
    }
}

/// After the handshake, the session stays open serving requests until the
/// other side hangs up.
pub fn serve_requests<H: Host>(
    host: &H,
    sess: &mut Session,
    peer_uid: &str,
) -> Result<ServeStats> {
    let mut stats = ServeStats::default();
    loop {
        let msg = match sess.recv() {
            Ok(m) => m,
            // Hung up: normal end of the session, not an error.
            Err(e) if is_disconnect(&e) => return Ok(stats),
            Err(e) => return Err(e),
        };
        match msg {
            Msg::ManifestReq { gzip } => {
                let manifest = host.with_db(|conn| Ok(crate::manifest::build(conn)?))?;
                send_manifest(sess, manifest, gzip)?;
            }
            // Someone is requesting one of our files.
            Msg::BlobReq { hash, offset } => {
                let path = host.with_db(|conn| Ok(crate::transfer::path_for_hash(conn, &hash)))?;
                match path {
                    Some(p) => {
                        crate::transfer::send_file(sess, &p, offset, &hash)?;
                        stats.sent += 1;
                    }
                    None => sess.send(&Msg::BlobError {
                        reason: format!("I do not have file {hash}"),
                    })?,
                }
            }
            // A file is being pushed to us.
            Msg::BlobPush {
                track_uid,
                hash,
                filename,
                title,
                artist,
                album,
                genre,
                duration_ms,
                bpm,
                updated_at,
                ..
            } => {
                let music_dir = host.music_dir();
                // Push: the other side sends from scratch, not resuming anything of ours.
                let got = crate::transfer::receive_file(
                    sess,
                    &music_dir,
                    &hash,
                    &filename,
                    false,
                    &mut |_, _| {},
                    &mut |dest| host.expect_path(dest),
                )?;
                host.with_db(|conn| {
                    // It just sent it to us: it obviously has it. This is what
                    // later allows freeing this file without risking anything.
                    let _ = crate::scope::note_replicas(conn, peer_uid, &[hash.clone()]);
                    // A failure registering THIS file can't cut the session:
                    // the `?` used to make a single problematic track flip
                    // the whole sync, which retried on its own and flipped
                    // again. It's logged and moves on to the next one.
                    if let Err(e) = crate::transfer::insert_received(
                        conn,
                        &got.path,
                        &track_uid,
                        &hash,
                        got.bytes,
                        &title,
                        &artist,
                        &album,
                        &genre,
                        duration_ms,
                        bpm,
                        updated_at,
                    ) {
                        log::warn!("[sync] could not register {filename}: {e}");
                    }
                    Ok(())
                })?;
                log::info!("[sync] received {filename} ({} bytes)", got.bytes);
                stats.received += 1;
                host.note_change_by(peer_uid);
                // Not forced: in a batch of files, a full library reload
                // per file leaves the UI unusable.
                host.library_changed(false);
            }
            // Organization changes sent to us by the other side.
            Msg::MetaPush { changes } => {
                let music_dir = host.music_dir();
                let applied =
                    host.with_db(|conn| Ok(crate::merge::apply(conn, &changes, &music_dir)?))?;
                if applied.total() > 0 {
                    // All five figures, not three. `total()` also counts scope
                    // and deletions, so changing a direction or checking a
                    // playlist landed here and printed three zeros: the log
                    // said "nothing happened" exactly when something had.
                    log::info!(
                        "[sync] applied {} tracks, {} playlists, {} memberships, {} deletions, {} scope rows",
                        applied.tracks,
                        applied.playlists,
                        applied.memberships,
                        applied.deleted,
                        applied.scope
                    );
                    host.library_changed(true);
                    host.note_change_by(peer_uid);
                }
                // This device's scope may have been changed by the other side:
                // if it re-checked a playlist, what was freed is recovered
                // from the trash before anyone requests it over the network.
                if applied.scope > 0 {
                    restore(host);
                }
                stats.applied.tracks += applied.tracks;
                stats.applied.playlists += applied.playlists;
                stats.applied.memberships += applied.memberships;
                stats.applied.deleted += applied.deleted;
                stats.applied.scope += applied.scope;
                sess.send(&Msg::MetaAck { applied })?;
            }
            // "Tell me when something changes." This doesn't return until the
            // library moves or the connection is cut: the caller left this
            // session open exactly for that and won't send anything else.
            Msg::Watch { since } => {
                let Some(current) = host.revision() else {
                    // Let it know so it stops trying, instead of reconnecting
                    // to a device that's never going to answer.
                    sess.send(&Msg::Reject {
                        reason: "this device does not report changes".into(),
                    })?;
                    return Ok(stats);
                };
                // With no mark, it waits from now. With one from another run —or
                // from a revision that doesn't exist yet— there's no way to know
                // what happened in between, so the answer is "yes" and to come
                // compare. It's the safe answer: an extra sync breaks nothing,
                // a missing one leaves an incomplete library.
                let since = match since {
                    None => current,
                    Some(s) if s.epoch != current.epoch => {
                        sess.send(&Msg::Changed { mark: current })?;
                        log::info!("[sync] {peer_uid} knows another run: telling it to sync");
                        return Ok(stats);
                    }
                    Some(s) if s.rev > current.rev => {
                        sess.send(&Msg::Changed { mark: current })?;
                        log::info!("[sync] {peer_uid} knows a newer revision: telling it to sync");
                        return Ok(stats);
                    }
                    Some(s) => s,
                };
                log::info!("[sync] {peer_uid} is waiting for changes");
                loop {
                    let seen = host.wait_revision(since, peer_uid, WATCH_HEARTBEAT);
                    if seen.news {
                        sess.send(&Msg::Changed { mark: seen.mark })?;
                        log::info!("[sync] told {peer_uid} that there are changes");
                        return Ok(stats);
                    }
                    sess.send(&Msg::Ping { mark: seen.mark })?;
                }
            }
            Msg::Bye => return Ok(stats),
            other => return Err(anyhow!("unexpected request: {other:?}")),
        }
    }
}

// ---------------------------------------------------------------------------
// Syncing side
// ---------------------------------------------------------------------------

/// Requests the other side's inventory and computes the plan. Also returns
/// the remote manifest: the transfer needs each track's metadata to register
/// what it receives with the other device's uid.
/// Also returns the effective direction: what can be pulled and what can be
/// pushed, resolved from what EACH device says about itself (`takes`,
/// `gives`). It isn't negotiated over the network — both rows travel
/// replicated in the manifest, so both sides arrive at the same conclusion on
/// their own.
pub fn fetch_plan<H: Host>(
    host: &H,
    sess: &mut Session,
) -> Result<(crate::manifest::Plan, crate::manifest::Manifest, (bool, bool))> {
    sess.send(&Msg::ManifestReq { gzip: true })?;
    let remote = match sess.recv()? {
        Msg::ManifestData { manifest } => *manifest,
        Msg::ManifestGz { data } => serde_json::from_slice(&crate::manifest::expand(&data)?)?,
        other => return Err(anyhow!("expected the manifest, got {other:?}")),
    };
    let local = host.with_db(|conn| {
        // Who has which file. This is the only thing that later allows
        // freeing space without risking the last copy (see `scope::evictable`).
        let hashes: Vec<String> = remote
            .tracks
            .iter()
            .filter(|t| t.present)
            .filter_map(|t| t.hash.clone())
            .collect();
        crate::scope::note_replicas(conn, &remote.device_uid, &hashes)?;
        Ok(crate::manifest::build(conn)?)
    })?;
    let dir = crate::scope::link_from(
        &local.device_uid,
        &remote.device_uid,
        &local.device_sync,
        &remote.device_sync,
    );
    Ok((crate::manifest::plan(&local, &remote), remote, dir))
}

/// Sends the inventory, compressed if the other side knows how to read it.
///
/// It's the only big thing that travels in a comparison, and it travels
/// **whole** even if nothing changed: with 5000 tracks that's 4 MB of JSON,
/// which compressed comes down to a few hundred kilobytes. On the phone
/// side, that comes out of the data plan.
fn send_manifest(
    sess: &mut Session,
    manifest: crate::manifest::Manifest,
    gzip: bool,
) -> Result<()> {
    if !gzip {
        return sess.send(&Msg::ManifestData {
            manifest: Box::new(manifest),
        });
    }
    let json = serde_json::to_vec(&manifest)?;
    let data = crate::manifest::squeeze(&json)?;
    log::debug!(
        "[sync] manifest: {} KB -> {} KB",
        json.len() / 1024,
        data.len() / 1024
    );
    sess.send(&Msg::ManifestGz { data })
}

/// Trims the plan down to what the two devices' direction allows. The raw
/// plan says what's missing; this says what of that is actually going to
/// happen, which is what needs to be shown before hitting Sync.
///
/// Files and organization go together: if a device doesn't send, it doesn't
/// send anything — moving playlists without the files, or the other way
/// around, leaves the two libraries describing different things.
pub fn restrict(plan: &mut crate::manifest::Plan, (takes, gives): (bool, bool)) {
    if !takes {
        plan.pull_files.clear();
        plan.pull_meta = 0;
        plan.pull_playlists = 0;
        plan.pull_memberships = 0;
        plan.deletes_in = 0;
    }
    if !gives {
        plan.push_files.clear();
        plan.push_meta = 0;
        plan.push_playlists = 0;
        plan.push_memberships = 0;
        plan.deletes_out = 0;
    }
}

/// A full run over an already-opened and handshaken session.
pub fn sync<H: Host>(host: &H, sess: &mut Session, peer_uid: &str) -> Result<SyncResult> {
    // A slow sync is three different things —inventory, files,
    // organization— and without measuring them separately there's no way to
    // know which one it is.
    let started = std::time::Instant::now();
    // Before planning: what came back into scope and is still in the trash
    // gets recovered here, not requested over the network. Otherwise,
    // re-checking a just-freed playlist would download gigabytes again that
    // were a rename away.
    restore(host);
    let (mut plan, remote, dir) = fetch_plan(host, sess)?;
    // The direction is resolved here, not in `plan()`: the plan describes
    // what's missing between the two libraries, the direction describes what
    // to do with that.
    restrict(&mut plan, dir);

    // The scope change may have been made by the OTHER device, and it only
    // shows up now, in its manifest. Applying it before transferring —and
    // rescuing from the trash what comes back in— is what avoids downloading
    // gigabytes again over the network that are a `rename` away. Otherwise,
    // it would be applied after the file loop, i.e. too late.
    if !plan.pull_files.is_empty() {
        apply_remote_scope(host, &remote)?;
        // Only if something actually came back: rebuilding the local
        // manifest means walking the whole library, and doing that on every
        // sync "just in case" is wastefully expensive — especially on phones.
        if restore(host) > 0 {
            let local = host.with_db(|conn| Ok(crate::manifest::build(conn)?))?;
            plan = crate::manifest::plan(&local, &remote);
            restrict(&mut plan, dir);
        }
    }
    let music_dir = host.music_dir();
    let after_plan = started.elapsed();

    let total_files = plan.pull_files.len() + plan.push_files.len();
    let (mut received, mut sent, mut failed, mut bytes) = (0usize, 0usize, 0usize, 0u64);
    let mut index = 0usize;

    // --- Pull what's missing here -----------------------------------------
    for f in &plan.pull_files {
        index += 1;
        let entry = remote.tracks.iter().find(|t| t.uid == f.track_uid);
        let Some(entry) = entry else { continue };
        let at = index;
        let mut progress = |done: u64, total: u64| {
            host.progress(&Progress {
                peer_uid,
                index: at,
                total_files,
                filename: &f.filename,
                done,
                total,
                sending: false,
            });
        };
        let got = crate::transfer::pull_file(
            sess,
            &music_dir,
            &f.hash,
            &f.filename,
            &mut progress,
            &mut |dest| host.expect_path(dest),
        );
        match got {
            Ok(got) => {
                host.with_db(|conn| {
                    crate::transfer::insert_received(
                        conn,
                        &got.path,
                        &entry.uid,
                        &f.hash,
                        got.bytes,
                        &entry.title,
                        &entry.artist,
                        &entry.album,
                        &entry.genre,
                        entry.duration_ms,
                        entry.bpm,
                        entry.updated_at,
                    )?;
                    Ok(())
                })?;
                bytes += got.bytes;
                received += 1;
            }
            Err(e) => {
                // A network cut isn't "this file failed": the session is no
                // longer good for anything, and continuing the loop against
                // a dead socket only piles up errors. It's cut and retried
                // later — the `.part` on disk is what makes that cost nothing.
                if is_disconnect(&e) {
                    return Err(e);
                }
                log::warn!("[sync] could not fetch {}: {e}", f.filename);
                failed += 1;
            }
        }
    }

    // --- Push what's missing there ------------------------------------
    for f in &plan.push_files {
        index += 1;
        let local = host.with_db(|conn| Ok(local_track(conn, &f.track_uid)))?;
        let Some((path, entry)) = local else {
            failed += 1;
            continue;
        };
        host.progress(&Progress {
            peer_uid,
            index,
            total_files,
            filename: &f.filename,
            done: 0,
            total: f.size as u64,
            sending: true,
        });
        let push = sess.send(&Msg::BlobPush {
            track_uid: entry.uid.clone(),
            hash: f.hash.clone(),
            filename: f.filename.clone(),
            size: f.size as u64,
            title: entry.title.clone(),
            artist: entry.artist.clone(),
            album: entry.album.clone(),
            genre: entry.genre.clone(),
            duration_ms: entry.duration_ms,
            bpm: entry.bpm,
            updated_at: entry.updated_at,
        });
        // A file that can't be read doesn't cut the whole run short.
        match push.and_then(|_| crate::transfer::send_file(sess, &path, 0, &f.hash)) {
            Ok(()) => {
                bytes += f.size as u64;
                sent += 1;
                // Now the file also lives there: it counts as a backup that
                // allows freeing space here.
                let _ = host.with_db(|conn| {
                    let _ = crate::scope::note_replicas(conn, peer_uid, &[f.hash.clone()]);
                    Ok(())
                });
            }
            Err(e) => {
                if is_disconnect(&e) {
                    return Err(e);
                }
                log::warn!("[sync] could not send {}: {e}", f.filename);
                failed += 1;
            }
        }
    }

    // --- Organization (Phase 5.5) ------------------------------------------
    //
    // Deliberately after the files: a membership for a track that hasn't
    // arrived yet is ignored, so it's better for it to exist first.
    //
    // The local manifest is rebuilt here rather than reusing the one from
    // `fetch_plan`: the rows that just came in via transfer have to travel in
    // this same sync, not the next one.
    let local = host.with_db(|conn| Ok(crate::manifest::build(conn)?))?;
    // With the metadata direction cut off the exchange still happens, empty:
    // the MetaPush/MetaAck round trip is the shape of the protocol, and
    // skipping it would leave the session waiting for a message that never
    // arrives.
    let after_files = started.elapsed();
    let (takes, gives) = dir;
    let mine = if gives {
        crate::merge::changes_for_peer(&local, &remote)
    } else {
        crate::merge::Changes::default()
    };
    let theirs = if takes {
        crate::merge::changes_for_peer(&remote, &local)
    } else {
        crate::merge::Changes::default()
    };

    let applied_here =
        host.with_db(|conn| Ok(crate::merge::apply(conn, &theirs, &music_dir)?))?;
    if applied_here.scope > 0 {
        restore(host);
    }
    sess.send(&Msg::MetaPush {
        changes: Box::new(mine),
    })?;
    let applied_there = match sess.recv()? {
        Msg::MetaAck { applied } => applied,
        other => return Err(anyhow!("expected MetaAck, got {other:?}")),
    };
    // Broken down and not just the total: if a sync repeats the same numbers
    // run after run, it isn't converging, and the total alone doesn't say
    // what's being re-applied.
    log::info!(
        "[sync] here: {} meta, {} playlists, {} memberships, {} deletions, {} scope | there: {} meta, {} playlists, {} memberships, {} deletions, {} scope",
        applied_here.tracks,
        applied_here.playlists,
        applied_here.memberships,
        applied_here.deleted,
        applied_here.scope,
        applied_there.tracks,
        applied_there.playlists,
        applied_there.memberships,
        applied_there.deleted,
        applied_there.scope
    );
    if applied_here.total() > 0 {
        log::debug!(
            "[sync] incoming: {} tracks, {} playlists, {} memberships, {} tombstones",
            theirs.tracks.len(),
            theirs.playlists.len(),
            theirs.memberships.len(),
            theirs.tombstones.len()
        );
    }

    let _ = sess.send(&Msg::Bye);
    let total = started.elapsed();
    let timings = format!(
        "[sync] timings: inventory {} ms, {total_files} file(s) {} ms, organization {} ms, total {} ms",
        after_plan.as_millis(),
        (after_files - after_plan).as_millis(),
        (total - after_files).as_millis(),
        total.as_millis()
    );
    log::info!("{timings}");
    // TEMPORARY — to the file too: on Android with logcat off this line is
    // the only way to see how much a sync is blocking.
    crate::perf_line(&timings);

    // Human-readable history per device (shown by the Sync screen). Only runs
    // that did something: a line for every empty automatic sync every few
    // minutes isn't history, it's noise.
    if received + sent + failed + applied_here.total() + applied_there.total() > 0 {
        let _ = host.with_db(|conn| {
            let _ = conn.execute(
                "INSERT INTO sync_log (ts, peer, kind, detail) VALUES (?1, ?2, 'sync', ?3)",
                rusqlite::params![
                    crate::db::now_ms(),
                    peer_uid,
                    format!(
                        "{received} in, {sent} out, {} organized{}",
                        applied_here.total() + applied_there.total(),
                        if failed > 0 { format!(", {failed} failed") } else { String::new() }
                    )
                ],
            );
            Ok(())
        });
    }

    Ok(SyncResult {
        received,
        sent,
        failed,
        bytes,
        organized: applied_here.total() + applied_there.total(),
    })
}

// ---------------------------------------------------------------------------
// Common pieces
// ---------------------------------------------------------------------------

/// Data for a local track by uid: the real path and its manifest entry.
fn local_track(
    conn: &Connection,
    uid: &str,
) -> Option<(std::path::PathBuf, crate::manifest::TrackEntry)> {
    conn.query_row(
        "SELECT path, uid, content_hash, COALESCE(size_bytes,0), COALESCE(rel_path,''),
                title, artist, album, genre, duration_ms, bpm, updated_at
         FROM tracks WHERE uid = ?1",
        [uid],
        |r| {
            let path: String = r.get(0)?;
            Ok((
                std::path::PathBuf::from(path),
                crate::manifest::TrackEntry {
                    uid: r.get(1)?,
                    hash: r.get(2)?,
                    size: r.get(3)?,
                    filename: r.get(4)?,
                    title: r.get(5)?,
                    artist: r.get(6)?,
                    album: r.get(7)?,
                    genre: r.get(8)?,
                    duration_ms: r.get(9)?,
                    bpm: r.get(10)?,
                    updated_at: r.get(11)?,
                    present: true,
                },
            ))
        },
    )
    .ok()
}

/// Applies the scope rows from the other side's manifest (LWW). It's the
/// same merge that `merge::apply` does later; it goes here first because the
/// scope decides what gets transferred in this same run.
fn apply_remote_scope<H: Host>(host: &H, remote: &crate::manifest::Manifest) -> Result<()> {
    host.with_db(|conn| {
        for e in &remote.scopes {
            crate::scope::apply_entry(conn, e)?;
        }
        for m in &remote.device_sync {
            crate::scope::apply_device_sync(conn, m)?;
        }
        Ok(())
    })
}

/// Recovers from the trash whatever has come back into this device's scope.
/// Best-effort: if it fails, the file gets downloaded again over the network
/// and nothing is lost.
/// The hash is computed **with the lock released**: hashing the trash can
/// take seconds, and holding the mutex in the meantime freezes the whole UI —
/// every `list_tracks` call from the frontend is left waiting. Only the two
/// endpoints (finding candidates, applying) touch the DB, and both are cheap.
pub fn restore<H: Host>(host: &H) -> usize {
    let music_dir = host.music_dir();

    // TEMPORARY — diagnostics. The outer `timed` measures everything together,
    // including the wait for the lock, so it doesn't distinguish "it's slow"
    // from "it waited on someone else".
    let t0 = std::time::Instant::now();
    // Finding out whether there's anything to recover is a read, and almost
    // always returns zero. Through the write connection, that "there's
    // nothing" used to queue up behind the sync: measured on Android, up to
    // 1255 ms of waiting to do nothing. The write lock is taken further
    // below, and only if there's something to move.
    let candidates = host.with_db_read(|conn| {
        let lock_ms = t0.elapsed().as_millis();
        let t1 = std::time::Instant::now();
        let r = crate::scope::restorable(conn);
        crate::perf_line(&format!(
            "  restore_local: lock {} ms, restorable {} ms, {} candidate(s)",
            lock_ms,
            t1.elapsed().as_millis(),
            r.as_ref().map(|c| c.len()).unwrap_or(0)
        ));
        Ok(r?)
    });
    let candidates = match candidates {
        Ok(c) => c,
        Err(e) => {
            log::warn!("[scope] could not list restorable files: {e}");
            return 0;
        }
    };
    if candidates.is_empty() {
        return 0;
    }

    let t2 = std::time::Instant::now();
    let found = crate::scope::find_in_trash(&music_dir, &candidates);
    crate::perf_line(&format!(
        "  restore_local: find_in_trash {} ms, {} found",
        t2.elapsed().as_millis(),
        found.len()
    ));
    if found.is_empty() {
        return 0;
    }

    let n = host.with_db(|conn| {
        Ok(crate::scope::finish_restore(
            conn,
            &music_dir,
            &found,
            &|p| host.expect_path(p),
        )?)
    });
    let n = match n {
        Ok(n) => n,
        Err(e) => {
            log::warn!("[scope] restoring from trash failed: {e}");
            return 0;
        }
    };
    if n > 0 {
        host.library_changed(true);
    }
    n
}

// ---------------------------------------------------------------------------
// Integrity suite (Phase 5.8)
// ---------------------------------------------------------------------------
//
// Two full devices —each with its own database, managed folder, and trash—
// actually synchronizing over a loopback socket, with the same code that
// runs in the app. What these tests chase isn't that the happy path works
// (that's already visible from using it): it's that **music is never lost**
// on the paths that can't be rehearsed by hand without two phones and a lot
// of patience — a cut in the middle of a transfer, both sides editing the
// same thing without seeing each other, a deletion traveling.
#[cfg(test)]
mod tests {
    use super::*;
    use crate::wire::{generate_keypair, Session};
    use std::net::{Shutdown, TcpListener, TcpStream};
    use std::sync::Mutex;
    use std::time::Duration;

    /// No test moves more than a few megabytes: if something is left waiting
    /// it's a bug, and an error is worth more than a hung suite.
    const TEST_IO_TIMEOUT: Duration = Duration::from_secs(20);

    /// A fake device, with everything the engine needs from a real one. It's
    /// the other implementation of `Host` (the first is `AppHandle`).
    struct Device {
        dir: PathBuf,
        db: Mutex<Connection>,
        /// Copy of this side's socket, to be able to cut the network by hand.
        sock: Mutex<Option<TcpStream>>,
        /// Cuts the connection once a transfer passes this many bytes.
        cut_after: Mutex<Option<u64>>,
        /// Progress seen, to be able to assert that a resumption started
        /// where it left off and not from scratch.
        seen: Mutex<Vec<(u64, u64)>>,
    }

    impl Host for Device {
        fn with_db<T>(&self, f: impl FnOnce(&Connection) -> Result<T>) -> Result<T> {
            let conn = self.db.lock().map_err(|_| anyhow!("db lock"))?;
            f(&conn)
        }
        fn music_dir(&self) -> PathBuf {
            self.dir.clone()
        }
        fn progress(&self, p: &Progress) {
            self.seen.lock().unwrap().push((p.done, p.total));
            let cut = *self.cut_after.lock().unwrap();
            if let Some(n) = cut {
                if p.done >= n {
                    // The wifi drops right here.
                    if let Some(s) = self.sock.lock().unwrap().as_ref() {
                        let _ = s.shutdown(Shutdown::Both);
                    }
                    *self.cut_after.lock().unwrap() = None;
                }
            }
        }
    }

    impl Device {
        fn new(tag: &str) -> Self {
            let dir = std::env::temp_dir().join(format!(
                "sway-eng-{tag}-{}-{:?}",
                std::process::id(),
                std::thread::current().id()
            ));
            std::fs::remove_dir_all(&dir).ok();
            std::fs::create_dir_all(&dir).unwrap();
            let conn = Connection::open_in_memory().unwrap();
            crate::db::init_schema(&conn).unwrap();
            // Forces the identity: the manifest needs it and this way it's fixed.
            crate::db::this_device_uid(&conn).unwrap();
            Self {
                dir,
                db: Mutex::new(conn),
                sock: Mutex::new(None),
                cut_after: Mutex::new(None),
                seen: Mutex::new(Vec::new()),
            }
        }

        fn uid(&self) -> String {
            self.with_db(|c| Ok(crate::db::this_device_uid(c)?)).unwrap()
        }

        /// A track with a real file in the managed folder.
        fn add_track(&self, filename: &str, bytes: &[u8], title: &str) -> String {
            let path = self.dir.join(filename);
            std::fs::write(&path, bytes).unwrap();
            let hash = crate::hashing::hash_file(&path).unwrap();
            let uid = crate::db::new_uid();
            let conn = self.db.lock().unwrap();
            crate::transfer::insert_received(
                &conn,
                &path,
                &uid,
                &hash,
                bytes.len() as u64,
                title,
                "Artist",
                "",
                "",
                0,
                None,
                crate::db::now_ms(),
            )
            .unwrap();
            uid
        }

        fn playlist(&self, name: &str, parent: Option<i64>) -> i64 {
            let conn = self.db.lock().unwrap();
            crate::db::create_playlist(&conn, name, "playlist", parent).unwrap()
        }

        fn folder(&self, name: &str) -> i64 {
            let conn = self.db.lock().unwrap();
            crate::db::create_playlist(&conn, name, "folder", None).unwrap()
        }

        fn track_id(&self, uid: &str) -> i64 {
            let conn = self.db.lock().unwrap();
            conn.query_row("SELECT id FROM tracks WHERE uid = ?1", [uid], |r| r.get(0))
                .unwrap()
        }

        fn add_to_playlist(&self, playlist: i64, track_uids: &[&str]) {
            let ids: Vec<i64> = track_uids.iter().map(|u| self.track_id(u)).collect();
            let mut conn = self.db.lock().unwrap();
            crate::db::add_tracks_to_playlist(&mut conn, playlist, &ids).unwrap();
        }

        /// Local deletion, just like `db::delete_tracks` leaves it: row out,
        /// tombstone in, file out of the library. What the app does with the
        /// file (OS trash) isn't the network's business, and in a test it
        /// would send temporary files to the real trash.
        fn delete_track(&self, uid: &str) {
            let conn = self.db.lock().unwrap();
            let path: String = conn
                .query_row("SELECT path FROM tracks WHERE uid = ?1", [uid], |r| r.get(0))
                .unwrap();
            conn.execute("DELETE FROM tracks WHERE uid = ?1", [uid]).unwrap();
            crate::db::record_tombstone(&conn, "track", uid).unwrap();
            std::fs::remove_file(path).ok();
        }

        fn track_uids(&self) -> Vec<String> {
            let conn = self.db.lock().unwrap();
            let mut stmt = conn
                .prepare("SELECT uid FROM tracks ORDER BY uid")
                .unwrap();
            let v: Vec<String> = stmt
                .query_map([], |r| r.get(0))
                .unwrap()
                .map(|r| r.unwrap())
                .collect();
            v
        }

        /// Titles of a playlist, in the order defined by rank.
        fn order_in(&self, playlist: &str) -> Vec<String> {
            let conn = self.db.lock().unwrap();
            let mut stmt = conn
                .prepare(
                    "SELECT t.title FROM playlist_tracks pt
                     JOIN playlists p ON p.id = pt.playlist_id
                     JOIN tracks t ON t.id = pt.track_id
                     WHERE p.name = ?1 ORDER BY pt.rank",
                )
                .unwrap();
            let v: Vec<String> = stmt
                .query_map([playlist], |r| r.get(0))
                .unwrap()
                .map(|r| r.unwrap())
                .collect();
            v
        }

        fn playlist_names(&self) -> Vec<String> {
            let conn = self.db.lock().unwrap();
            let mut stmt = conn
                .prepare("SELECT name FROM playlists ORDER BY name")
                .unwrap();
            let v: Vec<String> = stmt
                .query_map([], |r| r.get(0))
                .unwrap()
                .map(|r| r.unwrap())
                .collect();
            v
        }

        /// Audio files in the managed folder (excluding trash and partials,
        /// which live in `.sway-*` subdirectories).
        fn audio_files(&self) -> Vec<PathBuf> {
            let mut v: Vec<PathBuf> = std::fs::read_dir(&self.dir)
                .unwrap()
                .flatten()
                .filter(|e| e.path().is_file())
                .map(|e| e.path())
                .collect();
            v.sort();
            v
        }

        /// Everything left in the library's trash.
        fn trashed(&self) -> Vec<Vec<u8>> {
            let dir = crate::trash::trash_dir(&self.dir);
            std::fs::read_dir(dir)
                .map(|rd| {
                    rd.flatten()
                        .filter(|e| e.path().is_file())
                        .map(|e| std::fs::read(e.path()).unwrap())
                        .collect()
                })
                .unwrap_or_default()
        }

        fn partials(&self) -> Vec<PathBuf> {
            std::fs::read_dir(crate::transfer::incoming_dir(&self.dir))
                .map(|rd| rd.flatten().map(|e| e.path()).collect())
                .unwrap_or_default()
        }

        fn cut_after(&self, bytes: u64) {
            *self.cut_after.lock().unwrap() = Some(bytes);
        }

        fn first_progress(&self) -> u64 {
            self.seen.lock().unwrap().first().map(|(d, _)| *d).unwrap_or(0)
        }

        fn forget_progress(&self) {
            self.seen.lock().unwrap().clear();
        }
    }

    impl Drop for Device {
        fn drop(&mut self) {
            std::fs::remove_dir_all(&self.dir).ok();
        }
    }

    /// Both ends of an encrypted channel over loopback, with the socket
    /// copies noted on each device so the network can be cut.
    fn link(a: &Device, b: &Device) -> (Session, Session) {
        let (ka, _) = generate_keypair().unwrap();
        let (kb, _) = generate_keypair().unwrap();
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let server = std::thread::spawn(move || {
            let (s, _) = listener.accept().unwrap();
            s.set_read_timeout(Some(TEST_IO_TIMEOUT)).unwrap();
            let clone = s.try_clone().unwrap();
            (Session::accept(s, &kb).unwrap(), clone)
        });
        let cs = TcpStream::connect(addr).unwrap();
        cs.set_read_timeout(Some(TEST_IO_TIMEOUT)).unwrap();
        let mine = cs.try_clone().unwrap();
        let client = Session::connect(cs, &ka).unwrap();
        let (srv, theirs) = server.join().unwrap();
        *a.sock.lock().unwrap() = Some(mine);
        *b.sock.lock().unwrap() = Some(theirs);
        (client, srv)
    }

    /// A full run: `a` syncs, `b` serves. It's exactly what happens between
    /// the PC and the phone, with the server thread on this side.
    fn sync_once(a: &Device, b: &Device) -> Result<SyncResult> {
        let (mut ca, mut sb) = link(a, b);
        let a_uid = a.uid();
        let b_uid = b.uid();
        std::thread::scope(|s| {
            let server = s.spawn(move || serve_requests(b, &mut sb, &a_uid));
            let out = sync(a, &mut ca, &b_uid);
            let _ = server.join().unwrap();
            out
        })
    }

    fn audio(n: usize, seed: u8) -> Vec<u8> {
        (0..n).map(|i| ((i + seed as usize) % 251) as u8).collect()
    }

    // -----------------------------------------------------------------------

    /// The base case, and the property that broke the most on-device:
    /// converging once is easy, staying put afterward is the hard part. A
    /// sync that repeats work on every run is the symptom of all the
    /// ping-pong bugs from Phase 5.
    #[test]
    fn two_libraries_converge_and_the_second_run_moves_nothing() {
        let a = Device::new("conv-a");
        let b = Device::new("conv-b");
        let t1 = a.add_track("uno.flac", &audio(2048, 1), "One");
        let t2 = a.add_track("dos.flac", &audio(3000, 2), "Two");
        a.add_track("tres.flac", &audio(1500, 3), "Three");
        let gigs = a.folder("Gigs");
        let set = a.playlist("Set", Some(gigs));
        // Reverse of the import order: the manual order is data, not a side
        // effect of how the files came in.
        a.add_to_playlist(set, &[&t2, &t1]);

        let r = sync_once(&a, &b).expect("the first sync has to work");
        assert_eq!(r.sent, 3, "all three files travel");
        assert_eq!(r.received, 0);

        assert_eq!(a.track_uids(), b.track_uids(), "same identity on both sides");
        assert_eq!(b.audio_files().len(), 3);
        assert_eq!(b.playlist_names(), vec!["Gigs".to_string(), "Set".to_string()]);
        assert_eq!(b.order_in("Set"), vec!["Two".to_string(), "One".to_string()]);
        // The hierarchy travels by uid, not local id.
        let parent: String = {
            let conn = b.db.lock().unwrap();
            conn.query_row(
                "SELECT parent.name FROM playlists p JOIN playlists parent ON parent.id = p.parent_id
                 WHERE p.name = 'Set'",
                [],
                |r| r.get(0),
            )
            .unwrap()
        };
        assert_eq!(parent, "Gigs");

        let r2 = sync_once(&a, &b).expect("the second run has to work");
        assert_eq!(
            (r2.sent, r2.received, r2.organized),
            (0, 0, 0),
            "everything's already there: it can't move anything again"
        );
        // And not the other way either: if the other side thought it was
        // missing something, the pair would end up sending each other the
        // same thing forever.
        let r3 = sync_once(&b, &a).expect("the reverse direction too");
        assert_eq!((r3.sent, r3.received, r3.organized), (0, 0, 0));
    }

    /// The case that can't be tested by hand without fighting the wifi: the
    /// network cuts out in the middle of a file. Nothing is lost, and the
    /// next run **resumes** instead of downloading everything again.
    #[test]
    fn a_cut_midway_loses_nothing_and_resumption_picks_up_where_it_left_off() {
        let a = Device::new("cut-a");
        let b = Device::new("cut-b");
        let bytes = audio(2_500_000, 7);
        a.add_track("largo.flac", &bytes, "Long");

        // B requests it, so B is the one that resumes: the offset is carried by the request.
        b.cut_after(1_000_000);
        let err = sync_once(&b, &a).expect_err("the network cut out, the sync can't say it worked");
        assert!(is_disconnect(&err), "a cut is a cut, not some weird error: {err}");

        assert!(b.track_uids().is_empty(), "nothing half-downloaded enters the library");
        assert!(b.audio_files().is_empty(), "and no broken file is left in the folder");
        let partials = b.partials();
        assert_eq!(partials.len(), 1, "what was downloaded survives as a partial");
        let partial_len = std::fs::metadata(&partials[0]).unwrap().len();
        assert!(
            partial_len >= 1_000_000 && partial_len < bytes.len() as u64,
            "the partial has what arrived ({partial_len} bytes)"
        );
        assert_eq!(a.audio_files().len(), 1, "the sender never loses anything");

        b.forget_progress();
        let r = sync_once(&b, &a).expect("and now it works");
        assert_eq!(r.received, 1);
        assert!(
            b.first_progress() >= 1_000_000,
            "started from zero: the resumption was useless"
        );
        assert_eq!(b.audio_files().len(), 1, "one file, not two");
        assert_eq!(std::fs::read(&b.audio_files()[0]).unwrap(), bytes, "and it's the same one");
        assert!(b.partials().is_empty(), "the partial is consumed once it's done");
    }

    /// A deletion travels, but the file **isn't destroyed**: it stays in the
    /// library's trash for 30 days. And it doesn't come back on its own in
    /// the next run, which is what a naive union merge would do.
    #[test]
    fn a_deletion_travels_leaves_the_file_in_the_trash_and_does_not_come_back() {
        let a = Device::new("del-a");
        let b = Device::new("del-b");
        let bytes = audio(4096, 11);
        let uid = a.add_track("chau.flac", &bytes, "Bye");
        a.add_track("queda.flac", &audio(2048, 12), "Stays");
        sync_once(&a, &b).unwrap();
        assert_eq!(b.audio_files().len(), 2);

        a.delete_track(&uid);
        sync_once(&a, &b).unwrap();

        assert_eq!(b.track_uids().len(), 1, "the deletion was applied on the other side");
        assert_eq!(b.audio_files().len(), 1);
        assert!(
            b.trashed().contains(&bytes),
            "but the file still exists, in the trash"
        );

        let r = sync_once(&a, &b).unwrap();
        assert_eq!((r.sent, r.received), (0, 0), "what was deleted can't come back");
        assert_eq!(a.track_uids().len(), 1);
        assert_eq!(b.track_uids().len(), 1);
    }

    /// Both sides edit without seeing each other. The rule is the usual one:
    /// the newest wins field by field, and when in doubt it's kept —
    /// memberships are merged, not overwritten.
    #[test]
    fn editing_both_sides_without_seeing_each_other_loses_neither_change() {
        let a = Device::new("split-a");
        let b = Device::new("split-b");
        let t1 = a.add_track("uno.flac", &audio(2048, 31), "One");
        let t2 = a.add_track("dos.flac", &audio(2048, 32), "Two");
        let set = a.playlist("Set", None);
        a.add_to_playlist(set, &[&t1]);
        sync_once(&a, &b).unwrap();

        // Without seeing each other: each one renames the same playlist and
        // adds something different to it.
        {
            let conn = b.db.lock().unwrap();
            let id: i64 = conn
                .query_row("SELECT id FROM playlists WHERE name = 'Set'", [], |r| r.get(0))
                .unwrap();
            // B's is the OLD change: it was made before they met.
            conn.execute(
                "UPDATE playlists SET name = 'Saturday', updated_at = ?1 WHERE id = ?2",
                rusqlite::params![crate::db::now_ms() - 10_000, id],
            )
            .unwrap();
        }
        let warmup = b.playlist("Warmup", None);
        let b_t2 = b.track_uids().into_iter().find(|u| *u == t2).unwrap();
        b.add_to_playlist(warmup, &[&b_t2]);

        {
            let conn = a.db.lock().unwrap();
            crate::db::rename_playlist(&conn, set, "Friday").unwrap();
        }
        a.add_to_playlist(set, &[&t2]);

        sync_once(&a, &b).unwrap();

        for (who, d) in [("A", &a), ("B", &b)] {
            let mut names = d.playlist_names();
            names.sort();
            assert_eq!(
                names,
                vec!["Friday".to_string(), "Warmup".to_string()],
                "{who}: the newer rename wins and the other side's new playlist isn't lost"
            );
            let mut order = d.order_in("Friday");
            order.sort();
            assert_eq!(
                order,
                vec!["Two".to_string(), "One".to_string()],
                "{who}: memberships are merged"
            );
            assert_eq!(d.order_in("Warmup"), vec!["Two".to_string()], "{who}: and so is the new playlist's");
        }

        let r = sync_once(&a, &b).unwrap();
        assert_eq!(
            (r.sent, r.received, r.organized),
            (0, 0, 0),
            "after resolving the conflict they have to stay put"
        );
    }

    /// The full cycle of selective sync, which is where music would most
    /// easily get lost: unchecking a playlist, freeing the space, and
    /// checking it again. The three things that have to hold are that
    /// freeing **doesn't destroy** (it goes to the trash), that what was
    /// freed doesn't come back on its own while it's out of scope, and that
    /// re-checking it rescues it from disk instead of downloading it again
    /// over the network — which with a 20 GB library isn't an efficiency
    /// detail but the difference between usable and unusable.
    #[test]
    fn freeing_space_does_not_destroy_and_re_checking_rescues_without_network() {
        let a = Device::new("scope-a");
        let b = Device::new("scope-b");
        let party_bytes = audio(4096, 41);
        let t1 = a.add_track("set.flac", &audio(2048, 42), "FromSet");
        let t2 = a.add_track("fiesta.flac", &party_bytes, "FromParty");
        let set = a.playlist("Set", None);
        let fiesta = a.playlist("Party", None);
        a.add_to_playlist(set, &[&t1]);
        a.add_to_playlist(fiesta, &[&t2]);

        // Started by B: this way it gets noted that A has a copy of both
        // files, which is what later enables freeing space with no risk.
        let r = sync_once(&b, &a).unwrap();
        assert_eq!(r.received, 2);

        // B switches to selective and keeps only "Set".
        let b_uid = b.uid();
        let fiesta_uid: String = {
            let conn = b.db.lock().unwrap();
            crate::scope::set_mode(&conn, &b_uid, crate::scope::Mode::Selected).unwrap();
            let s: String = conn
                .query_row("SELECT uid FROM playlists WHERE name = 'Set'", [], |r| r.get(0))
                .unwrap();
            let f: String = conn
                .query_row("SELECT uid FROM playlists WHERE name = 'Party'", [], |r| r.get(0))
                .unwrap();
            crate::scope::set_playlist(&conn, &b_uid, &s, true).unwrap();
            f
        };

        // Free space: only what's confirmed to exist on another device.
        let (n, _) = {
            let conn = b.db.lock().unwrap();
            let items = crate::scope::evictable(&conn, &b.dir).unwrap();
            assert_eq!(items.len(), 1, "only the one that fell out of scope");
            crate::scope::evict(&conn, &b.dir, &items).unwrap()
        };
        assert_eq!(n, 1);
        assert_eq!(b.audio_files().len(), 1, "the file left the library");
        assert!(b.trashed().contains(&party_bytes), "but it's in the trash, not destroyed");
        assert_eq!(b.track_uids().len(), 2, "and the row stays: the track is still visible");

        // Out of scope doesn't come back on its own, no matter that the other side has it.
        let r = sync_once(&b, &a).unwrap();
        assert_eq!(r.received, 0, "unchecked is unchecked");
        assert_eq!(b.audio_files().len(), 1);

        // It's checked again: the file has to come out of the trash.
        {
            let conn = b.db.lock().unwrap();
            crate::scope::set_playlist(&conn, &b_uid, &fiesta_uid, true).unwrap();
        }
        let r = sync_once(&b, &a).unwrap();
        assert_eq!(
            r.received, 0,
            "it was a rename away: it can't have traveled over the network"
        );
        assert_eq!(b.audio_files().len(), 2, "and yet it came back");
        let came_back = b
            .audio_files()
            .into_iter()
            .map(|p| std::fs::read(p).unwrap())
            .any(|c| c == party_bytes);
        assert!(came_back, "with the same bytes, verified by hash");
    }
}

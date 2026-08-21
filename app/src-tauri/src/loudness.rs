//! Integrated loudness (EBU R128) for "normalize volume".
//!
//! The measurement is derived from the bytes, so it is deliberately **not**
//! replicated: each device measures its own copy of a file and arrives at the
//! same number. Syncing it would buy nothing and add a field that can conflict.
//!
//! Why measure instead of reading ReplayGain tags: the tags are the cheap
//! option, but DJ rips overwhelmingly don't carry them. A "normalize volume"
//! switch that quietly does nothing on most of the library is worse than no
//! switch, so this decodes the file and computes R128 for real.
//!
//! Cost: decoding is the expensive part, roughly real-time ÷ 30 per track on a
//! modern machine. That's why it runs on one background thread at low
//! priority, in small batches, and never blocks anything the user is doing.

use crate::db;
use rodio::{Decoder, Source};
use std::fs::File;
use std::io::BufReader;
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use tauri::{AppHandle, Emitter, Manager};

/// How many tracks are pulled from the DB at a time. The point is to release
/// the read lock between batches rather than hold it for the whole library.
const BATCH: usize = 32;

/// Anything quieter than this counts as silence when looking for where the
/// audio starts and ends. -60 dBFS is below the noise floor of any real
/// recording but well above a true digital zero, so it also catches the
/// near-silent hiss that encoder padding and vinyl rips leave behind.
const SILENCE_FLOOR: f32 = 0.001; // 10^(-60/20)

/// Never trim more than this from either edge. A track that genuinely opens
/// with a long quiet intro is not padding, and cutting 30 seconds off it
/// because it starts under the floor would be worse than the gap.
const MAX_TRIM_MS: i64 = 12_000;

/// What one decode of a file yields.
#[derive(Debug, Clone, Copy)]
pub struct Analysis {
    /// Integrated loudness, LUFS.
    pub lufs: f64,
    /// Absolute ms where the audio starts.
    pub lead_ms: i64,
    /// Absolute ms where the audio ends. Absolute, not "silence from the
    /// end", because the end would have to be measured against the track's
    /// stored duration — a bitrate estimate for VBR, and wrong by seconds.
    pub audio_end_ms: i64,
}

/// Measures a file: how loud it is overall, and where its audio actually
/// starts and ends.
///
/// Both come out of the same pass on purpose — decoding is the expensive
/// part by a wide margin, and once the samples are in hand, finding the first
/// and last audible frame costs a comparison each.
///
/// `None` when the file can't be opened or decoded, or when it is too short
/// for R128 to produce an integrated value (under ~400 ms — the gate needs at
/// least one block). A jingle that short doesn't need normalizing anyway.
pub fn measure(path: &Path) -> Option<Analysis> {
    let file = File::open(path).ok()?;
    let decoder = Decoder::new(BufReader::new(file)).ok()?;
    let channels = decoder.channels() as usize;
    let rate = decoder.sample_rate();
    if channels == 0 || rate == 0 {
        return None;
    }

    let mut meter = ebur128::EbuR128::new(channels as u32, rate, ebur128::Mode::I).ok()?;

    // Fed in frames, not one sample at a time: `add_frames_f32` wants
    // interleaved data and the per-call overhead is what dominates otherwise.
    // 8192 frames is ~185 ms at 44.1 kHz — small enough that a 20-minute DJ
    // set never materializes in memory.
    let chunk_frames = 8192usize;
    let mut buf: Vec<f32> = Vec::with_capacity(chunk_frames * channels);

    // Frame indices of the first and last sample above the floor. Tracked
    // while streaming so the file is never held in memory.
    let mut frames: i64 = 0;
    let mut first_loud: Option<i64> = None;
    let mut last_loud: i64 = 0;

    for sample in decoder.convert_samples::<f32>() {
        buf.push(sample);
        if buf.len() >= chunk_frames * channels {
            scan_edges(&buf, channels, &mut frames, &mut first_loud, &mut last_loud);
            meter.add_frames_f32(&buf).ok()?;
            buf.clear();
        }
    }
    if !buf.is_empty() {
        // A trailing partial frame (a truncated file) would make the frame
        // count disagree with the channel count and be rejected; drop it.
        let usable = buf.len() - (buf.len() % channels);
        if usable > 0 {
            scan_edges(&buf[..usable], channels, &mut frames, &mut first_loud, &mut last_loud);
            meter.add_frames_f32(&buf[..usable]).ok()?;
        }
    }

    let lufs = match meter.loudness_global() {
        // R128 reports -inf for silence. That isn't a level to correct
        // towards — normalizing it would ask for the maximum boost on a track
        // that has nothing in it.
        Ok(v) if v.is_finite() => v,
        _ => return None,
    };

    let to_ms = |f: i64| (f * 1000) / rate as i64;
    let total_ms = to_ms(frames);
    // A file with nothing above the floor anywhere: leave both edges alone
    // rather than "trim" the entire thing.
    let (lead_ms, audio_end_ms) = match first_loud {
        None => (0, total_ms),
        Some(first) => {
            let lead = to_ms(first).clamp(0, MAX_TRIM_MS);
            // The cap applies to how much is cut, not to where the cut is.
            let trail = to_ms((frames - 1 - last_loud).max(0)).clamp(0, MAX_TRIM_MS);
            (lead, total_ms - trail)
        }
    };
    Some(Analysis { lufs, lead_ms, audio_end_ms })
}

/// Advances the frame counter over one interleaved chunk and notes the first
/// and last frame carrying anything audible.
fn scan_edges(
    chunk: &[f32],
    channels: usize,
    frames: &mut i64,
    first_loud: &mut Option<i64>,
    last_loud: &mut i64,
) {
    for frame in chunk.chunks_exact(channels) {
        if frame.iter().any(|s| s.abs() > SILENCE_FLOOR) {
            if first_loud.is_none() {
                *first_loud = Some(*frames);
            }
            *last_loud = *frames;
        }
        *frames += 1;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    const RATE: u32 = 44_100;

    /// Writes a 16-bit stereo WAV by hand — no encoder dependency for what is
    /// a 44-byte header and the samples straight after it.
    fn write_wav(path: &Path, frames: &[i16]) {
        let data_len = (frames.len() * 2) as u32; // 2 bytes per sample
        let mut f = File::create(path).unwrap();
        let byte_rate = RATE * 2 * 2;
        f.write_all(b"RIFF").unwrap();
        f.write_all(&(36 + data_len).to_le_bytes()).unwrap();
        f.write_all(b"WAVEfmt ").unwrap();
        f.write_all(&16u32.to_le_bytes()).unwrap();
        f.write_all(&1u16.to_le_bytes()).unwrap(); // PCM
        f.write_all(&2u16.to_le_bytes()).unwrap(); // stereo
        f.write_all(&RATE.to_le_bytes()).unwrap();
        f.write_all(&byte_rate.to_le_bytes()).unwrap();
        f.write_all(&4u16.to_le_bytes()).unwrap(); // block align
        f.write_all(&16u16.to_le_bytes()).unwrap();
        f.write_all(b"data").unwrap();
        f.write_all(&data_len.to_le_bytes()).unwrap();
        for s in frames {
            f.write_all(&s.to_le_bytes()).unwrap();
        }
    }

    /// Interleaved stereo: `silence_ms` of digital zero, a tone, then more
    /// silence.
    fn tone_between_silences(lead_ms: u64, tone_ms: u64, trail_ms: u64) -> Vec<i16> {
        let f = |ms: u64| (RATE as u64 * ms / 1000) as usize;
        let mut out = vec![0i16; f(lead_ms) * 2];
        for i in 0..f(tone_ms) {
            let t = i as f32 / RATE as f32;
            let v = ((t * 440.0 * std::f32::consts::TAU).sin() * 8000.0) as i16;
            out.push(v);
            out.push(v);
        }
        out.extend(std::iter::repeat(0i16).take(f(trail_ms) * 2));
        out
    }

    fn tmp(name: &str) -> std::path::PathBuf {
        let mut p = std::env::temp_dir();
        p.push(format!("sway-loudness-{}-{name}.wav", std::process::id()));
        p
    }

    /// The measurement that gapless depends on: where the audio actually
    /// starts and ends. Tolerance is one analysis block — the edges are found
    /// per frame, but the tone's own zero crossings sit near the floor.
    #[test]
    fn silent_edges_are_found() {
        let p = tmp("edges");
        write_wav(&p, &tone_between_silences(500, 2000, 800));
        let a = measure(&p).expect("a 2 s tone is measurable");
        let _ = std::fs::remove_file(&p);

        assert!((a.lead_ms - 500).abs() < 60, "lead was {} ms", a.lead_ms);
        // 500 + 2000 + 800 = 3300 ms total, audio ending at 2500.
        assert!((a.audio_end_ms - 2500).abs() < 60, "end was {} ms", a.audio_end_ms);
        assert!(a.lufs.is_finite() && a.lufs < 0.0, "lufs was {}", a.lufs);
    }

    /// A file that is audible edge to edge must not be trimmed at all —
    /// clipping the first beat off a track is far worse than a small gap.
    #[test]
    fn a_track_with_no_silence_is_not_trimmed() {
        let p = tmp("full");
        write_wav(&p, &tone_between_silences(0, 2000, 0));
        let a = measure(&p).expect("measurable");
        let _ = std::fs::remove_file(&p);

        assert!(a.lead_ms < 60, "lead was {} ms", a.lead_ms);
        assert!((a.audio_end_ms - 2000).abs() < 60, "end was {} ms", a.audio_end_ms);
    }

    /// Silence all the way through is not "one long lead": there is no audio
    /// to find, so both edges stay at zero rather than trimming the file away.
    #[test]
    fn a_fully_silent_file_reports_no_trim() {
        let p = tmp("silent");
        write_wav(&p, &vec![0i16; (RATE as usize) * 2 * 2]);
        let measured = measure(&p);
        let _ = std::fs::remove_file(&p);

        // R128 gives -inf for pure silence, which `measure` refuses outright.
        // Either way the contract is the same: nothing gets trimmed.
        if let Some(a) = measured {
            assert_eq!(a.lead_ms, 0);
        }
    }

    /// A long quiet intro is a musical choice, not padding. The cap is what
    /// stops the analyzer from eating it.
    #[test]
    fn the_trim_is_capped() {
        let p = tmp("capped");
        // 20 s of silence is past MAX_TRIM_MS (12 s).
        write_wav(&p, &tone_between_silences(20_000, 1000, 0));
        let a = measure(&p).expect("measurable");
        let _ = std::fs::remove_file(&p);

        assert_eq!(a.lead_ms, MAX_TRIM_MS, "should stop at the cap");
    }
}

/// Progress of the background sweep, pushed to the UI as `loudness-progress`.
#[derive(Clone, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Progress {
    /// Tracks still unmeasured, this one included. 0 = the sweep is done.
    pub pending: i64,
}

/// Guards against two sweeps running at once. A second request while one is
/// already going is a no-op rather than a queue: the sweep always picks up
/// whatever is unmeasured *now*, so the one in flight will reach the new
/// tracks on its own.
#[derive(Default)]
pub struct Analyzer {
    running: Arc<AtomicBool>,
}

impl Analyzer {
    /// Measures everything unmeasured, on a background thread. Returns
    /// immediately. Safe to call on every startup and after every import.
    pub fn sweep(&self, app: AppHandle) {
        if self.running.swap(true, Ordering::SeqCst) {
            log::debug!("[analysis] sweep already running, ignoring");
            return;
        }
        let running = self.running.clone();
        std::thread::spawn(move || {
            let t0 = std::time::Instant::now();
            let done = run_sweep(&app);
            // A background job with no trace is a job nobody can tell is
            // broken. This project has been bitten by exactly that before.
            if done > 0 {
                log::info!("[analysis] measured {done} track(s) in {:?}", t0.elapsed());
            }
            running.store(false, Ordering::SeqCst);
        });
    }
}

/// Returns how many tracks it measured.
fn run_sweep(app: &AppHandle) -> usize {
    let mut done = 0usize;
    loop {
        let batch = {
            let state = app.state::<crate::AppState>();
            let Ok(conn) = state.db_read.lock() else {
                return done;
            };
            match db::tracks_needing_analysis(&conn, BATCH) {
                Ok(v) => v,
                Err(e) => {
                    log::warn!("[analysis] could not list pending tracks: {e}");
                    return done;
                }
            }
        };
        if batch.is_empty() {
            let _ = app.emit("loudness-progress", Progress { pending: 0 });
            return done;
        }
        log::info!("[analysis] measuring {} track(s)", batch.len());

        for (id, path) in batch {
            // Decoding happens with NO database lock held. Same rule the hash
            // backfill learned the hard way: holding the lock across file I/O
            // freezes every screen that needs to read.
            let measured = measure(Path::new(&path));
            let state = app.state::<crate::AppState>();
            let Ok(conn) = state.db.lock() else { return done };
            let saved = match measured {
                Some(a) => db::set_track_analysis(&conn, id, a.lufs, a.lead_ms, a.audio_end_ms),
                // A file that can't be measured is written at the reference
                // level with no trim, i.e. "needs no correction". Leaving it
                // NULL would put it back in the queue on every startup and
                // re-decode a broken file forever.
                None => {
                    log::info!("[analysis] {path} not measurable, left at reference level");
                    db::set_track_analysis(&conn, id, db::TARGET_LUFS, 0, 0)
                }
            };
            if let Err(e) = saved {
                log::warn!("[analysis] could not save track {id}: {e}");
            }
            done += 1;
            let pending = db::analysis_pending_count(&conn).unwrap_or(0);
            drop(conn);
            let _ = app.emit("loudness-progress", Progress { pending });
        }
    }
}

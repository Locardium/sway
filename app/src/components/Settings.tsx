import { useEffect, useState } from 'react';
import { open } from '@tauri-apps/plugin-dialog';
import { ChevronRight } from 'lucide-react';
import { Modal } from './Modal';
import { Switch } from './Switch';
import {
  exportLibraryXmlNow,
  getAutoSyncXml,
  importFromUri,
  listOutputDevices,
  loudnessPending,
  rescanAnalysis,
  setAutoSyncXml,
  supportsLoudnessAnalysis,
  type PlaybackPrefs,
} from '../api';
import { isAndroid } from '../platform';

/// System default is an entry in the list, not a device name: it means
/// *follow whatever the system does*, which is not the same as pinning the
/// device that happens to be the default right now. Empty value = `null`
/// over the wire.
const SYSTEM_DEFAULT = '';

interface Props {
  trackCount: number;
  /// Playback preferences and their setter, owned by App (the player reads
  /// them too, so there is one copy and this screen edits it).
  prefs: PlaybackPrefs;
  onPrefs: (next: PlaybackPrefs) => void | Promise<void>;
  onClose: () => void;
  onStatus: (msg: string) => void;
  onImported: () => void | Promise<void>;
  /// Sync has its own screen (Phase 5.7): devices, directions, deletions,
  /// selection and space don't fit in a single Settings row.
  onOpenSync: () => void;
}

const AUDIO_MIME = [
  'audio/flac',
  'audio/mpeg',
  'audio/wav',
  'audio/x-wav',
  'audio/mp4',
  'audio/aac',
  'audio/ogg',
  'audio/opus',
  'audio/*',
];

// Android's picker gives a content:// URI, not a path with a readable file
// name. Best-effort: the last segment usually carries the original name
// encoded (works with Android's standard document picker; other providers
// may give something generic — lofty still detects the real format by
// content, not by this extension).
function guessFileName(uri: string): string {
  try {
    const decoded = decodeURIComponent(uri);
    const last = decoded.split(/[/:]/).pop()?.trim();
    if (last && last.includes('.')) return last;
  } catch {
    // ignore, falls back
  }
  return `track-${Date.now()}.mp3`;
}

export default function Settings({
  trackCount,
  prefs,
  onPrefs,
  onClose,
  onStatus,
  onImported,
  onOpenSync,
}: Props) {
  const [autoSyncXml, setAutoSyncXmlState] = useState(true);
  const [syncing, setSyncing] = useState(false);
  const [importing, setImporting] = useState(false);
  const [devices, setDevices] = useState<string[]>([]);
  /// Tracks still waiting on the analyzer. `null` = not asked yet.
  const [pending, setPending] = useState<number | null>(null);
  const [rescanning, setRescanning] = useState(false);

  const android = isAndroid();
  const showExport = !android;

  /// Edits one preference and saves the lot — they travel together.
  function patch(next: Partial<PlaybackPrefs>) {
    onPrefs({ ...prefs, ...next });
  }


  // How much of the library the analyzer still has to get through. Polled
  // whenever this screen is open, not only while normalization is on: tracks
  // are measured as they arrive, so the count is the honest answer to "is it
  // still working" regardless of what the measurement is being used for.
  useEffect(() => {
    if (!supportsLoudnessAnalysis) return;
    let alive = true;
    const read = () =>
      loudnessPending()
        .then((n) => {
          if (alive) setPending(n);
        })
        .catch(() => {});
    read();
    // The sweep runs in the background; this is just the screen catching up
    // with it while it's open.
    const t = setInterval(read, 1500);
    return () => {
      alive = false;
      clearInterval(t);
    };
  }, []);

  /// Rescan throws away every measurement and lets the sweep redo the
  /// library. The button only starts it — the row's progress line is what
  /// reports how it's going.
  async function doRescan() {
    setRescanning(true);
    try {
      const n = await rescanAnalysis();
      setPending(n);
      onStatus(`Analyzing ${n} track${n === 1 ? '' : 's'}…`);
    } catch (e) {
      onStatus('Could not start the rescan: ' + e);
    } finally {
      setRescanning(false);
    }
  }

  useEffect(() => {
    if (!showExport) return;
    getAutoSyncXml()
      .then(setAutoSyncXmlState)
      .catch(() => {});
  }, [showExport]);

  // Read when the screen opens rather than kept live: headphones plugged in
  // while this modal is on screen is not worth a listener, and reopening it
  // re-reads.
  useEffect(() => {
    listOutputDevices()
      .then(setDevices)
      .catch(() => setDevices([]));
  }, []);

  async function toggleAutoSync(v: boolean) {
    setAutoSyncXmlState(v);
    try {
      await setAutoSyncXml(v);
    } catch (e) {
      onStatus(String(e));
    }
  }

  async function pickAndImport() {
    let picked: string | string[] | null;
    try {
      picked = await open({
        multiple: true,
        filters: [{ name: 'Audio', extensions: AUDIO_MIME }],
      });
    } catch (e) {
      onStatus('Picker error: ' + e);
      return;
    }
    if (!picked) return;
    const uris = Array.isArray(picked) ? picked : [picked];
    setImporting(true);
    let ok = 0;
    let failed = 0;
    for (const uri of uris) {
      try {
        await importFromUri(uri, guessFileName(uri));
        ok++;
      } catch (e) {
        failed++;
        console.error('import_from_uri failed for', uri, e);
      }
    }
    setImporting(false);
    await onImported();
    onStatus(failed === 0 ? `Imported ${ok} track(s).` : `Imported ${ok}, ${failed} failed.`);
  }

  async function syncNow() {
    setSyncing(true);
    try {
      await exportLibraryXmlNow();
      onStatus('iTunes library synced.');
    } catch (e) {
      onStatus('Sync error: ' + e);
    } finally {
      setSyncing(false);
    }
  }

  return (
    <Modal title="Settings" onClose={onClose} wide>
      <div className="settings">
        <section>
          <h4>Library</h4>
          <div className="set-row">
            <div className="set-label">
              <span>Managed folder</span>
              <small>Imported files are copied here</small>
            </div>
            <div className="set-control">
              <code className="set-path">&lt;Music&gt;/Sway</code>
              <button disabled>Change…</button>
            </div>
          </div>
          {android && (
            <div className="set-row">
              <div className="set-label">
                <span>Import tracks</span>
                <small>Pick audio files from this device</small>
              </div>
              <button onClick={pickAndImport} disabled={importing}>
                {importing ? 'Importing…' : 'Import…'}
              </button>
            </div>
          )}
          {/* The analyzer decodes every file, which is rodio/symphonia and so
              desktop-only. On Android the row is the count and nothing else,
              rather than a Rescan button with nothing behind it. */}
          <div className="set-row">
            <div className="set-label">
              <span>Tracks in library</span>
              {supportsLoudnessAnalysis && (
                <small>
                  {pending == null || pending === 0
                    ? 'Rescan re-measures loudness and silent edges for every track'
                    : `Analyzing — ${pending} track${pending === 1 ? '' : 's'} to go`}
                </small>
              )}
            </div>
            <div className="set-control">
              <span className="set-value">{trackCount}</span>
              {supportsLoudnessAnalysis && (
                <button onClick={doRescan} disabled={rescanning}>
                  {rescanning ? 'Starting…' : 'Rescan'}
                </button>
              )}
            </div>
          </div>
        </section>

        <section>
          <h4>Sync</h4>
          <div className="set-row">
            <div className="set-label">
              <span>Devices, selection and space</span>
              <small>Pair devices, choose what lives where, review deletions</small>
            </div>
            <button onClick={onOpenSync}>
              Open <ChevronRight size={13} />
            </button>
          </div>
        </section>

        <section>
          <h4>Playback</h4>
          <div className="set-row">
            <div className="set-label">
              <span>Crossfade</span>
              <small>
                {prefs.crossfadeSecs === 0
                  ? 'Off — one track ends before the next begins'
                  : `${prefs.crossfadeSecs}s of overlap between tracks`}
              </small>
            </div>
            <input
              type="range"
              min={0}
              max={12}
              value={prefs.crossfadeSecs}
              onChange={(e) => patch({ crossfadeSecs: Number(e.target.value) })}
              className="range set-slider"
              style={{ ['--fill' as string]: `${(prefs.crossfadeSecs / 12) * 100}%` }}
              aria-label="Crossfade in seconds"
            />
          </div>
          <div className="set-row">
            <div className="set-label">
              <span>Gapless playback</span>
              <small>
                {prefs.crossfadeSecs > 0
                  ? 'Not used while crossfade is on — an overlap has no gap'
                  : 'Trims the silence at the start and end of each file so tracks run straight into each other'}
              </small>
            </div>
            <Switch
              checked={prefs.gapless}
              onChange={(v) => patch({ gapless: v })}
              disabled={prefs.crossfadeSecs > 0}
            />
          </div>
          <div className="set-row">
            <div className="set-label">
              <span>Autoplay next track</span>
              <small>
                {prefs.autoplay
                  ? 'Keeps going through the queue on its own'
                  : 'Stops when the track ends and stays there'}
              </small>
            </div>
            <Switch checked={prefs.autoplay} onChange={(v) => patch({ autoplay: v })} />
          </div>
          {/* Needs a measured LUFS per track, and the analyzer that produces
              it is desktop-only. The per-track gain knob works on both. */}
          {supportsLoudnessAnalysis && (
            <div className="set-row">
              <div className="set-label">
                <span>Normalize volume</span>
                <small>
                  Brings every track to the same perceived loudness,
                  turning down what was mastered loud and turning up what wasn&rsquo;t. Your
                  gain knob still applies on top.
                </small>
              </div>
              <Switch checked={prefs.normalize} onChange={(v) => patch({ normalize: v })} />
            </div>
          )}

          <div className="set-row">
            <div className="set-label">
              <span>Output device</span>
              <small>
                {prefs.outputDevice
                  ? 'Pinned — system output changes are ignored'
                  : 'Follows the system, including changes mid-track'}
              </small>
            </div>
            <select
              className="set-select"
              value={prefs.outputDevice ?? SYSTEM_DEFAULT}
              onChange={(e) =>
                patch({ outputDevice: e.target.value === SYSTEM_DEFAULT ? null : e.target.value })
              }
            >
              <option value={SYSTEM_DEFAULT}>System default</option>
              {devices.map((d) => (
                <option key={d} value={d}>
                  {d}
                </option>
              ))}
              {/* The pinned device isn't in the list any more (headphones
                  unplugged since last time). Shown rather than dropped, or the
                  select renders blank and looks broken — playback already fell
                  back to the system default on its own. */}
              {prefs.outputDevice != null && !devices.includes(prefs.outputDevice) && (
                <option value={prefs.outputDevice}>{prefs.outputDevice} — not connected</option>
              )}
            </select>
          </div>
        </section>

        {showExport && (
          <section>
            <h4>Export</h4>
            <div className="set-row">
              <div className="set-label">
                <span>Auto-sync to iTunes library</span>
                <small>
                  Keeps <code className="set-path">Music\iTunes\iTunes Music Library.xml</code> updated
                  automatically (Rekordbox/Serato read it from there)
                </small>
              </div>
              <Switch checked={autoSyncXml} onChange={toggleAutoSync} />
            </div>
            <div className="set-row">
              <div className="set-label">
                <span>iTunes XML</span>
                <small>Writes the library now, regardless of auto-sync</small>
              </div>
              <button onClick={syncNow} disabled={syncing}>
                {syncing ? 'Syncing…' : 'Sync now'}
              </button>
            </div>
          </section>
        )}

        <section>
          <h4>About</h4>
          <div className="set-row">
            <div className="set-label"><span>Version</span></div>
            <span className="set-value">1.0.0</span>
          </div>
          {/* <p className="set-note">
            Sway — a DJ library manager. Current output volume {Math.round(volume * 100)}%.
          </p> */}
        </section>
      </div>
    </Modal>
  );
}

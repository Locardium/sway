import { useEffect, useState } from 'react';
import { open } from '@tauri-apps/plugin-dialog';
import { ChevronRight } from 'lucide-react';
import { Modal } from './Modal';
import { Switch } from './Switch';
import {
  exportLibraryXmlNow,
  getAutoSyncXml,
  importFromUri,
  setAutoSyncXml,
} from '../api';
import { isAndroid } from '../platform';

interface Props {
  trackCount: number;
  volume: number;
  onClose: () => void;
  onStatus: (msg: string) => void;
  onImported: () => void | Promise<void>;
  /// Sync tiene pantalla propia (Fase 5.7): dispositivos, direcciones,
  /// borrados, selección y espacio no entran en una fila de Settings.
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

// El picker de Android da un content:// URI, no un path con nombre de
// archivo legible. Best-effort: el ultimo segmento suele traer el nombre
// original codeado (funciona con el document picker estandar de Android;
// otros proveedores pueden dar algo generico — lofty igual detecta el
// formato real por contenido, no por esta extension).
function guessFileName(uri: string): string {
  try {
    const decoded = decodeURIComponent(uri);
    const last = decoded.split(/[/:]/).pop()?.trim();
    if (last && last.includes('.')) return last;
  } catch {
    // ignore, cae al fallback
  }
  return `track-${Date.now()}.mp3`;
}

// Ajustes en su mayoria prototipo (no funcionales todavia): la UI existe para
// definir el modelo, la logica llega en fases siguientes.
export default function Settings({
  trackCount,
  volume,
  onClose,
  onStatus,
  onImported,
  onOpenSync,
}: Props) {
  const [gapless, setGapless] = useState(true);
  const [autoplay, setAutoplay] = useState(true);
  const [crossfade, setCrossfade] = useState(0);
  const [compact, setCompact] = useState(false);
  const [normalize, setNormalize] = useState(false);
  const [theme, setTheme] = useState('dark');
  const [accent, setAccent] = useState('sky');
  const [autoSyncXml, setAutoSyncXmlState] = useState(true);
  const [syncing, setSyncing] = useState(false);
  const [importing, setImporting] = useState(false);

  const android = isAndroid();
  const showExport = !android;

  useEffect(() => {
    if (!showExport) return;
    getAutoSyncXml()
      .then(setAutoSyncXmlState)
      .catch(() => {});
  }, [showExport]);

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

  const accents: { id: string; color: string }[] = [
    { id: 'sky', color: 'oklch(80% 0.11 220)' },
    { id: 'green', color: 'oklch(80% 0.15 155)' },
    { id: 'violet', color: 'oklch(72% 0.15 300)' },
    { id: 'amber', color: 'oklch(80% 0.13 75)' },
    { id: 'rose', color: 'oklch(72% 0.16 15)' },
  ];

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
          <div className="set-row">
            <div className="set-label">
              <span>Tracks in library</span>
            </div>
            <div className="set-control">
              <span className="set-value">{trackCount}</span>
              <button disabled>Rescan</button>
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
              <small>{crossfade === 0 ? 'Off' : `${crossfade}s`}</small>
            </div>
            <input
              type="range"
              min={0}
              max={12}
              value={crossfade}
              onChange={(e) => setCrossfade(Number(e.target.value))}
              className="range set-slider"
              style={{ ['--fill' as string]: `${(crossfade / 12) * 100}%` }}
            />
          </div>
          <div className="set-row">
            <div className="set-label"><span>Gapless playback</span></div>
            <Switch checked={gapless} onChange={setGapless} />
          </div>
          <div className="set-row">
            <div className="set-label"><span>Autoplay next track</span></div>
            <Switch checked={autoplay} onChange={setAutoplay} />
          </div>
          <div className="set-row">
            <div className="set-label">
              <span>Normalize volume</span>
              <small>ReplayGain</small>
            </div>
            <Switch checked={normalize} onChange={setNormalize} />
          </div>
          <div className="set-row">
            <div className="set-label"><span>Output device</span></div>
            <select disabled className="set-select">
              <option>System default</option>
            </select>
          </div>
        </section>

        <section>
          <h4>Appearance</h4>
          <div className="set-row">
            <div className="set-label"><span>Theme</span></div>
            <select value={theme} onChange={(e) => setTheme(e.target.value)} className="set-select">
              <option value="dark">Dark</option>
              <option value="light">Light</option>
              <option value="system">System</option>
            </select>
          </div>
          <div className="set-row">
            <div className="set-label"><span>Accent color</span></div>
            <div className="swatches">
              {accents.map((a) => (
                <button
                  key={a.id}
                  className={'swatch' + (accent === a.id ? ' on' : '')}
                  style={{ background: a.color }}
                  onClick={() => setAccent(a.id)}
                  aria-label={a.id}
                />
              ))}
            </div>
          </div>
          <div className="set-row">
            <div className="set-label"><span>Compact rows</span></div>
            <Switch checked={compact} onChange={setCompact} />
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
                <span>Rekordbox / iTunes XML</span>
                <small>Writes the library now, regardless of auto-sync</small>
              </div>
              <button onClick={syncNow} disabled={syncing}>
                {syncing ? 'Syncing…' : 'Sync now'}
              </button>
            </div>
            <div className="set-row">
              <div className="set-label">
                <span>Serato crates</span>
                <small>Coming in a later phase</small>
              </div>
              <button disabled>Export…</button>
            </div>
          </section>
        )}

        <section>
          <h4>About</h4>
          <div className="set-row">
            <div className="set-label"><span>Version</span></div>
            <span className="set-value">Sway 0.1.0</span>
          </div>
          <p className="set-note">
            Sway — a DJ library manager. Current output volume {Math.round(volume * 100)}%.
          </p>
        </section>
      </div>
    </Modal>
  );
}

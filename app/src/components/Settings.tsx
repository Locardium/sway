import { useCallback, useEffect, useState } from 'react';
import { open } from '@tauri-apps/plugin-dialog';
import { listen } from '@tauri-apps/api/event';
import { Modal } from './Modal';
import {
  confirmPairing,
  connectPeer,
  deviceIdentity,
  exportLibraryXmlNow,
  getAutoSyncXml,
  importFromUri,
  listPeers,
  refreshPeers,
  setAutoSyncXml,
  setDeviceName,
  SYNC_PROTO,
  unpairDevice,
  type PairingDone,
  type PairingRequest,
  type Peer,
  type PeerHello,
} from '../api';
import { isAndroid } from '../platform';

interface Props {
  trackCount: number;
  volume: number;
  onClose: () => void;
  onStatus: (msg: string) => void;
  onImported: () => void | Promise<void>;
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

function Switch({ checked, onChange }: { checked: boolean; onChange: (v: boolean) => void }) {
  return (
    <button
      role="switch"
      aria-checked={checked}
      className={'switch' + (checked ? ' on' : '')}
      onClick={() => onChange(!checked)}
    >
      <span className="switch-knob" />
    </button>
  );
}

// Ajustes en su mayoria prototipo (no funcionales todavia): la UI existe para
// definir el modelo, la logica llega en fases siguientes.
export default function Settings({ trackCount, volume, onClose, onStatus, onImported }: Props) {
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
  const [deviceName, setDeviceNameState] = useState('');
  const [savedName, setSavedName] = useState('');
  const [peers, setPeers] = useState<Peer[]>([]);
  const [pairing, setPairing] = useState<PairingRequest | null>(null);
  const [busyPeer, setBusyPeer] = useState<string | null>(null);
  // Conteos que mandó cada peer en su Hello — la prueba visible de que el
  // canal cifrado quedó vivo.
  const [hellos, setHellos] = useState<Record<string, PeerHello>>({});

  const android = isAndroid();
  const showExport = !android;

  useEffect(() => {
    if (!showExport) return;
    getAutoSyncXml()
      .then(setAutoSyncXmlState)
      .catch(() => {});
  }, [showExport]);

  const reloadPeers = useCallback(() => {
    listPeers().then(setPeers).catch(() => {});
  }, []);

  /// El botón: pregunta de nuevo a la red y vuelve a leer. Las respuestas
  /// llegan por `peers-changed`, así que el spinner es solo para que se note
  /// que algo pasó.
  const [scanning, setScanning] = useState(false);
  async function rescan() {
    setScanning(true);
    try {
      await refreshPeers();
    } catch (e) {
      onStatus(String(e));
    }
    reloadPeers();
    setTimeout(() => setScanning(false), 1200);
  }

  useEffect(() => {
    if (!('__TAURI_INTERNALS__' in window)) return;
    deviceIdentity()
      .then(([, name]) => {
        setDeviceNameState(name);
        setSavedName(name);
      })
      .catch(() => {});
    reloadPeers();
    // El backend avisa por `peers-changed` cuando alguien aparece, se va o
    // deja de responder al sondeo. El intervalo es solo una red de contencion
    // por si se pierde un evento.
    const uns = [
      listen('peers-changed', reloadPeers),
      listen<PairingRequest>('pairing-request', (e) => setPairing(e.payload)),
      listen<PairingDone>('pairing-done', (e) => {
        setPairing(null);
        setBusyPeer(null);
        const { name, ok, error } = e.payload;
        onStatus(ok ? `Paired with ${name}` : `${name}: ${error ?? 'pairing failed'}`);
        reloadPeers();
      }),
      listen<PeerHello>('peer-hello', (e) => {
        setBusyPeer(null);
        setHellos((h) => ({ ...h, [e.payload.uid]: e.payload }));
      }),
    ];
    const timer = setInterval(reloadPeers, 15000);
    return () => {
      uns.forEach((un) => un.then((f) => f()));
      clearInterval(timer);
    };
  }, [reloadPeers, onStatus]);

  async function decide(accept: boolean) {
    if (!pairing) return;
    const uid = pairing.uid;
    setPairing(null);
    if (!accept) setBusyPeer(null);
    try {
      await confirmPairing(uid, accept);
    } catch (e) {
      onStatus(String(e));
    }
  }

  async function unpair(peer: Peer) {
    try {
      await unpairDevice(peer.uid);
      setHellos((h) => {
        const next = { ...h };
        delete next[peer.uid];
        return next;
      });
      onStatus(`Unpaired ${peer.name}`);
    } catch (e) {
      onStatus(String(e));
    }
  }

  async function saveDeviceName() {
    const name = deviceName.trim();
    if (!name || name === savedName) return;
    try {
      await setDeviceName(name);
      setSavedName(name);
      onStatus('Device renamed');
    } catch (e) {
      onStatus(String(e));
    }
  }

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

  // El código va sobre Settings: es una decisión de seguridad y no puede
  // quedar escondida detrás de un scroll.
  if (pairing) {
    return (
      <Modal title={pairing.incoming ? `${pairing.name} wants to pair` : `Pair with ${pairing.name}`} onClose={() => decide(false)}>
        <div className="pair-dialog">
          <p className="set-note">
            Check that this code is showing on <strong>{pairing.name}</strong> too. If the two codes
            are different, someone else is on the line — reject it.
          </p>
          <div className="pair-code">{pairing.code}</div>
          <div className="pair-actions">
            <button onClick={() => decide(false)}>Reject</button>
            <button className="primary" onClick={() => decide(true)}>
              Codes match
            </button>
          </div>
        </div>
      </Modal>
    );
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
          <h4>Sync</h4>
          <div className="set-row">
            <div className="set-label">
              <span>This device</span>
              <small>The name other devices see on your network</small>
            </div>
            <input
              className="set-input"
              value={deviceName}
              maxLength={48}
              onChange={(e) => setDeviceNameState(e.target.value)}
              onBlur={saveDeviceName}
              onKeyDown={(e) => {
                if (e.key === 'Enter') (e.target as HTMLInputElement).blur();
              }}
            />
          </div>
          <div className="set-row">
            <div className="set-label">
              <span>Devices on this network</span>
              <small>
                {peers.length === 0
                  ? 'Looking for other devices running Sway…'
                  : `${peers.length} found`}
              </small>
            </div>
            <button onClick={rescan} disabled={scanning}>{scanning ? 'Scanning…' : 'Refresh'}</button>
          </div>
          {peers.length > 0 && (
            <ul className="peer-list">
              {peers.map((p) => {
                const hello = hellos[p.uid];
                const incompatible = p.online && p.proto !== SYNC_PROTO;
                let detail: string;
                if (!p.online) detail = 'Not on this network';
                else if (hello) detail = `${hello.tracks} tracks · ${hello.playlists} playlists`;
                else detail = `${p.platform} · ${p.addrs[0] ?? 'no address'}:${p.port}`;
                return (
                  <li key={p.uid} className={'peer' + (p.online ? '' : ' offline')}>
                    <div className="set-label">
                      <span>{p.name}</span>
                      <small>{detail}</small>
                    </div>
                    <div className="set-control">
                      <span className={'peer-badge' + (p.paired && p.online ? ' ok' : '')}>
                        {!p.online
                          ? 'Offline'
                          : incompatible
                            ? 'Other version'
                            : p.paired
                              ? 'Paired'
                              : 'Not paired'}
                      </span>
                      <button
                        disabled={!p.online || incompatible || busyPeer === p.uid}
                        onClick={() => {
                          setBusyPeer(p.uid);
                          connectPeer(p.uid).catch((e) => {
                            setBusyPeer(null);
                            onStatus(String(e));
                          });
                        }}
                      >
                        {busyPeer === p.uid ? 'Connecting…' : p.paired ? 'Ping' : 'Pair'}
                      </button>
                      {p.paired && <button onClick={() => unpair(p)}>Unpair</button>}
                    </div>
                  </li>
                );
              })}
            </ul>
          )}
          <p className="set-note">
            Pairing shows a 6-digit code on both devices — they must match. Nothing is transferred
            yet; “Ping” just asks the other device how much library it has.
          </p>
        </section>

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

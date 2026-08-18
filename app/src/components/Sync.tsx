import { useCallback, useEffect, useMemo, useRef, useState } from 'react';
import { ChevronLeft, ChevronRight } from 'lucide-react';
import { listen } from '@tauri-apps/api/event';
import { Modal } from './Modal';
import { Switch } from './Switch';
import { formatBytes, formatWhen } from '../format';
import {
  confirmPairing,
  connectPeer,
  deviceIdentity,
  freeSpace,
  getAutoSyncP2p,
  getScope,
  listPeers,
  pairWithServer,
  PLATFORM_SERVER,
  previewSync,
  setSyncLimits,
  syncConditions,
  type Conditions,
  type SyncLimits,
  refreshPeers,
  setAutoSyncP2p,
  setDeviceName,
  setScopeDirection,
  setScopeMode,
  setScopePlaylists,
  storageStatus,
  syncFiles,
  syncHistory,
  SYNC_PROTO,
  unpairDevice,
  type LogEntry,
  type PairingDone,
  type PairingRequest,
  type Peer,
  type PlaylistNode,
  type Scope,
  type Storage,
  type SyncDone,
  type SyncPlanEvent,
  type SyncProgress,
} from '../api';

interface Props {
  /// El árbol de playlists ya cargado por App: el editor de scope necesita los
  /// uids, que es lo único que significa algo del otro lado.
  nodes: PlaylistNode[];
  onClose: () => void;
  onStatus: (msg: string) => void;
  onLibraryChanged: () => void | Promise<void>;
}

/// Qué pasaría (o qué está por pasar) con un dispositivo.
function PlanSummary({ ev }: { ev: SyncPlanEvent }) {
  const p = ev.plan;
  const rows: [string, string][] = [];
  if (p.pullFiles.length)
    rows.push(['Files to receive', `${p.pullFiles.length} · ${formatBytes(ev.bytesIn)}`]);
  if (p.pushFiles.length)
    rows.push(['Files to send', `${p.pushFiles.length} · ${formatBytes(ev.bytesOut)}`]);
  if (p.pullMeta || p.pushMeta) rows.push(['Metadata updates', `${p.pullMeta} in · ${p.pushMeta} out`]);
  if (p.pullPlaylists || p.pushPlaylists)
    rows.push(['Playlists', `${p.pullPlaylists} in · ${p.pushPlaylists} out`]);
  if (p.pullMemberships || p.pushMemberships)
    rows.push(['Playlist entries', `${p.pullMemberships} in · ${p.pushMemberships} out`]);
  if (p.deletesIn || p.deletesOut)
    rows.push(['Deletions', `${p.deletesIn} here · ${p.deletesOut} there`]);

  return (
    <div className="plan">
      {rows.length === 0 ? (
        <div className="plan-row">
          <span>Already in sync</span>
        </div>
      ) : (
        rows.map(([label, value]) => (
          <div className="plan-row" key={label}>
            <span>{label}</span>
            <strong>{value}</strong>
          </div>
        ))
      )}
      {(p.outOfScopeIn > 0 || p.outOfScopeOut > 0) && (
        <div className="plan-row">
          <span>Out of selection</span>
          <strong>
            {p.outOfScopeIn} here · {p.outOfScopeOut} there
          </strong>
        </div>
      )}
      {p.unhashed > 0 && (
        <div className="plan-row">
          <span>Still hashing</span>
          <strong>{p.unhashed} tracks</strong>
        </div>
      )}
      <p className="set-note">Preview only — nothing has been transferred.</p>
    </div>
  );
}

/// Transferencia en curso. Con archivos de 40 MB por una red doméstica, sin
/// esto la app parece colgada.
function TransferProgress({ p }: { p: SyncProgress }) {
  const pct = p.total > 0 ? Math.min(100, (p.done / p.total) * 100) : 0;
  return (
    <div className="plan">
      <div className="plan-row">
        <span>
          {p.sending ? 'Sending' : 'Receiving'} {p.fileIndex}/{p.fileTotal} · {p.filename}
        </span>
        <strong>
          {formatBytes(p.done)} / {formatBytes(p.total)}
        </strong>
      </div>
      <div className="xfer-bar">
        <div className="xfer-fill" style={{ width: `${pct}%` }} />
      </div>
    </div>
  );
}

function directionLabel(direction: string): string {
  if (direction === 'send') return 'Only sends';
  if (direction === 'receive') return 'Only receives';
  if (direction === 'off') return 'Paused';
  return 'Sends and receives';
}

function scopeLabel(scope: Scope): string {
  return scope.mode === 'all' ? 'everything' : `${scope.selected.length} playlist(s) selected`;
}

interface ScopeEditorProps {
  nodes: PlaylistNode[];
  scope: Scope;
  onMode: (mode: string) => void;
  onDirection: (direction: string) => void;
  /// Varias filas de una: un click en una carpeta escribe todo su subárbol.
  onToggle: (changes: { uid: string; on: boolean }[]) => void;
}

/// Lo que hace un dispositivo: dirección y qué se lleva. Es lo mismo para este
/// dispositivo y para cualquier otro — la dirección describe al dispositivo,
/// no al vínculo, así que entre dos algo se mueve sólo si uno manda y el otro
/// recibe.
function ScopeEditor({ nodes, scope, onMode, onDirection, onToggle }: ScopeEditorProps) {
  const selected = useMemo(() => new Set(scope.selected), [scope.selected]);

  /// El árbol se recorre UNA vez por render, de abajo hacia arriba.
  ///
  /// Antes cada nodo salía a buscar su propio subárbol, y cada paso de esa
  /// búsqueda filtraba la lista entera de nodos. Con unas cuantas playlists eso
  /// son millones de comparaciones por render — y hay un render al abrir el
  /// panel y otro en cada tilde. De ahí venía el freeze, no de las consultas.
  ///
  /// Sale todo de acá:
  /// - `kids`: hijos por padre, ya ordenados.
  /// - `leaves`: uids de las playlists que cuelgan de cada nodo (o el suyo, si
  ///   es una). Sólo las playlists guardan estado; una carpeta es su suma.
  /// - `onCount`: cuántas de ésas sincronizan hoy, contando lo heredado de una
  ///   carpeta marcada. Eso último es cómo se guardaba antes y hay bases con
  ///   esas filas, así que hay que seguir leyéndolo bien.
  const view = useMemo(() => {
    const kids = new Map<number | null, PlaylistNode[]>();
    for (const n of nodes) {
      const list = kids.get(n.parentId);
      list ? list.push(n) : kids.set(n.parentId, [n]);
    }
    for (const list of kids.values()) list.sort((a, b) => a.position - b.position);

    const leaves = new Map<number, string[]>();
    const onCount = new Map<number, number>();
    const walk = (n: PlaylistNode, inherited: boolean): [string[], number] => {
      const on = inherited || (!!n.uid && selected.has(n.uid));
      let mine: string[] = [];
      let count = 0;
      if (n.kind === 'playlist') {
        if (n.uid) {
          mine = [n.uid];
          count = on ? 1 : 0;
        }
      } else {
        for (const k of kids.get(n.id) ?? []) {
          const [ls, c] = walk(k, on);
          mine = mine.concat(ls);
          count += c;
        }
      }
      leaves.set(n.id, mine);
      onCount.set(n.id, count);
      return [mine, count];
    };
    for (const root of kids.get(null) ?? []) walk(root, false);
    return { kids, leaves, onCount };
  }, [nodes, selected]);

  /// Un click escribe el subárbol entero, explícito, y apaga las filas de
  /// carpeta que hubiera.
  ///
  /// Antes se guardaba la marca EN la carpeta y el subárbol salía por herencia.
  /// Eso hacía imposible desmarcar una sola playlist de adentro: la fila de la
  /// carpeta la volvía a incluir del lado Rust, así que el tilde no se podía
  /// sacar y había que deshabilitar los hijos para que no mintiera. Con todo
  /// explícito, lo que se ve en la pantalla es exactamente lo que hay guardado.
  function toggle(n: PlaylistNode, on: boolean) {
    const targets = new Set(view.leaves.get(n.id) ?? []);
    const wanted = new Set<string>();
    for (const p of nodes) {
      if (!p.uid || p.kind !== 'playlist') continue;
      const nowOn = (view.onCount.get(p.id) ?? 0) > 0;
      if (targets.has(p.uid) ? on : nowOn) wanted.add(p.uid);
    }
    const changes = [
      ...[...wanted].filter((u) => !selected.has(u)).map((uid) => ({ uid, on: true })),
      // Acá caen también las filas de carpeta: no están en `wanted` —que es
      // sólo de playlists— así que se apagan solas.
      ...[...selected].filter((u) => !wanted.has(u)).map((uid) => ({ uid, on: false })),
    ];
    if (changes.length > 0) onToggle(changes);
  }

  const render = (parentId: number | null, depth: number) =>
    (view.kids.get(parentId) ?? []).map((n) => {
        const total = (view.leaves.get(n.id) ?? []).length;
        const onCount = view.onCount.get(n.id) ?? 0;
        // La carpeta refleja a sus hijas y nada más: todas marcadas es tilde,
        // algunas es guioncito, ninguna es vacío.
        const on = total > 0 && onCount === total;
        const partial = onCount > 0 && onCount < total;
        return (
          <div key={n.id}>
            <label className="scope-node" style={{ paddingLeft: 4 + depth * 16 }}>
              <input
                type="checkbox"
                checked={on}
                ref={(el) => {
                  if (el) el.indeterminate = partial;
                }}
                // Una carpeta vacía no tiene nada que marcar. Los hijos de una
                // carpeta marcada SÍ se pueden desmarcar de a uno.
                disabled={total === 0}
                onChange={(e) => toggle(n, e.target.checked)}
              />
              <span>{n.name}</span>
              {n.kind === 'playlist' && <small>{n.trackCount}</small>}
            </label>
            {render(n.id, depth + 1)}
          </div>
        );
      });

  return (
    <>
      <div className="set-row">
        <div className="set-label">
          <span>Sync direction</span>
          <small>Something moves between two devices only if one sends and the other receives</small>
        </div>
        <select
          className="set-select"
          value={scope.direction}
          onChange={(e) => onDirection(e.target.value)}
        >
          <option value="both">Sends and receives</option>
          <option value="send">Only sends</option>
          <option value="receive">Only receives</option>
          <option value="off">Paused</option>
        </select>
      </div>
      <div className="set-row">
        <div className="set-label">
          <span>What it keeps</span>
          <small>
            Unselected playlists fade out, then disappear from the library once you free
            their space. They stay listed here so you can bring them back.
          </small>
        </div>
        <select className="set-select" value={scope.mode} onChange={(e) => onMode(e.target.value)}>
          <option value="all">Everything</option>
          <option value="selected">Selected playlists</option>
        </select>
      </div>
      {scope.mode === 'selected' && (
        <div className="scope-tree">
          {nodes.length === 0 ? (
            <p className="set-note">No playlists yet.</p>
          ) : (
            render(null, 0)
          )}
        </div>
      )}
    </>
  );
}

export default function Sync({ nodes, onClose, onStatus, onLibraryChanged }: Props) {
  const [myUid, setMyUid] = useState('');
  const [deviceName, setDeviceNameState] = useState('');
  const [savedName, setSavedName] = useState('');
  const [autoSync, setAutoSync] = useState(true);
  const [peers, setPeers] = useState<Peer[]>([]);
  const [pairing, setPairing] = useState<PairingRequest | null>(null);
  const [busyPeer, setBusyPeer] = useState<string | null>(null);
  const [plans, setPlans] = useState<Record<string, SyncPlanEvent>>({});
  const [progress, setProgress] = useState<Record<string, SyncProgress>>({});
  const [storage, setStorage] = useState<Storage | null>(null);
  const [myScope, setMyScope] = useState<Scope>({ mode: 'all', direction: 'both', selected: [] });
  const [scanning, setScanning] = useState(false);
  const [freeing, setFreeing] = useState(false);

  // Red y batería. `null` en un campo = no se sabe, que es distinto de saber
  // que no: una PC de escritorio no tiene batería, y ahí la opción no se
  // muestra en vez de mostrarse sin sentido.
  const [conditions, setConditions] = useState<Conditions | null>(null);
  const [limits, setLimits] = useState<SyncLimits | null>(null);

  // Server de archivo: no se descubre, se escribe. Se acuerda del host entre
  // intentos (equivocarse en el token es lo más probable) pero nunca del
  // token.
  const [serverHost, setServerHost] = useState('');
  const [serverPort, setServerPort] = useState('7420');
  const [serverToken, setServerToken] = useState('');
  const [addingServer, setAddingServer] = useState(false);

  /// Resumen por dispositivo para la lista: dirección y selección. La
  /// dirección describe un vínculo, así que no existe para "este dispositivo"
  /// suelto — pero sí tiene que verse sin entrar a cada uno.
  const [summaries, setSummaries] = useState<Record<string, string>>({});

  // Detalle de un dispositivo. `null` = la lista.
  const [openUid, setOpenUid] = useState<string | null>(null);
  const [peerScope, setPeerScope] = useState<Scope>({
    mode: 'all',
    direction: 'both',
    selected: [],
  });
  const [history, setHistory] = useState<LogEntry[]>([]);

  const reloadPeers = useCallback(() => {
    listPeers().then(setPeers).catch(() => {});
  }, []);

  /// El estado de espacio se recalcula recorriendo la biblioteca, así que no
  /// puede correr una vez por cada `library-changed`: durante un sync llegan de
  /// a montones y el celular se traba. Con la última alcanza.
  const localTimer = useRef<ReturnType<typeof setTimeout>>();
  const reloadLocal = useCallback((immediate = false) => {
    clearTimeout(localTimer.current);
    const run = () => {
      storageStatus().then(setStorage).catch(() => {});
    };
    // Al abrir el panel no hay nada que agrupar: la espera es puro retraso
    // mirando un panel vacío. El respiro es para las ráfagas de después.
    if (immediate) run();
    else localTimer.current = setTimeout(run, 600);
  }, []);

  useEffect(() => {
    if (!('__TAURI_INTERNALS__' in window)) return;
    deviceIdentity()
      .then(([uid, name]) => {
        setMyUid(uid);
        setDeviceNameState(name);
        setSavedName(name);
        return getScope(uid).then(setMyScope);
      })
      .catch(() => {});
    getAutoSyncP2p().then(setAutoSync).catch(() => {});
    syncConditions()
      .then(({ now, limits }) => {
        setConditions(now);
        setLimits(limits);
      })
      .catch(() => {});
    reloadPeers();
    reloadLocal(true);

    const uns = [
      listen('peers-changed', reloadPeers),
      // Envuelto: el handler recibe el evento, y pasárselo como `immediate`
      // saltearía el respiro justo en las ráfagas para las que existe.
      listen('library-changed', () => reloadLocal()),
      listen<PairingRequest>('pairing-request', (e) => setPairing(e.payload)),
      listen<PairingDone>('pairing-done', (e) => {
        setPairing(null);
        setBusyPeer(null);
        const { name, ok, error } = e.payload;
        onStatus(ok ? `Paired with ${name}` : `${name}: ${error ?? 'pairing failed'}`);
        reloadPeers();
      }),
      listen('peer-hello', () => setBusyPeer(null)),
      listen<SyncPlanEvent>('sync-plan', (e) => {
        setBusyPeer(null);
        setPlans((p) => ({ ...p, [e.payload.uid]: e.payload }));
      }),
      listen<SyncProgress>('sync-progress', (e) => {
        setProgress((p) => ({ ...p, [e.payload.uid]: e.payload }));
      }),
      listen<SyncDone>('sync-done', (e) => {
        const { uid, name, received, sent, failed, organized, auto, error } = e.payload;
        setBusyPeer(null);
        setProgress((p) => {
          const next = { ...p };
          delete next[uid];
          return next;
        });
        // El plan viejo quedó obsoleto: lo que se transfirió ya no falta.
        setPlans((p) => {
          const next = { ...p };
          delete next[uid];
          return next;
        });
        reloadLocal();
        if (auto && !error && !received && !sent && !organized && !failed) return;
        if (error) onStatus(`${name}: ${error}`);
        else {
          const parts = [];
          if (received) parts.push(`${received} received`);
          if (sent) parts.push(`${sent} sent`);
          if (organized) parts.push(`${organized} playlist/metadata updates`);
          if (failed) parts.push(`${failed} failed`);
          onStatus(parts.length ? `${name}: ${parts.join(', ')}` : `${name}: nothing to transfer`);
        }
        onLibraryChanged();
      }),
    ];
    const timer = setInterval(reloadPeers, 15000);
    return () => {
      uns.forEach((un) => un.then((f) => f()));
      clearInterval(timer);
      clearTimeout(localTimer.current);
    };
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [reloadPeers, reloadLocal, onStatus]);

  // Resumen de cada dispositivo vinculado, para no tener que entrar a todos
  // para saber cómo está configurado.
  useEffect(() => {
    const paired = peers.filter((p) => p.paired).map((p) => p.uid);
    if (paired.length === 0) return;
    let alive = true;
    Promise.all(
      paired.map(async (uid) => {
        const sc = await getScope(uid);
        return [uid, `${directionLabel(sc.direction)} · ${scopeLabel(sc)}`] as const;
      }),
    )
      .then((rows) => alive && setSummaries(Object.fromEntries(rows)))
      .catch(() => {});
    return () => {
      alive = false;
    };
  }, [peers]);

  // Al abrir el detalle de un dispositivo se leen sus reglas y su scope.
  useEffect(() => {
    if (!openUid) return;
    getScope(openUid).then(setPeerScope).catch(() => {});
    syncHistory(openUid).then(setHistory).catch(() => {});
  }, [openUid]);

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

  async function saveName() {
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

  /// El tilde se pone en el acto y el guardado va por atrás, sin `await`.
  ///
  /// Esperar la ida y vuelta antes de soltar el click hacía que tildar dos
  /// cosas seguidas se sintiera trabado, y no había nada que ganar: el estado
  /// que mira la pantalla es el de acá, y si el guardado falla se avisa y se
  /// recarga del backend, que es la única fuente de verdad.
  function changeScope(deviceUid: string, next: Scope, apply: () => Promise<void>) {
    if (deviceUid === myUid) setMyScope(next);
    else setPeerScope(next);
    apply()
      .then(() => {
        if (deviceUid === myUid) reloadLocal();
      })
      .catch((e) => {
        onStatus(String(e));
        // El optimismo salió mal: volver a lo que dice la DB, que es lo único
        // que vale.
        getScope(deviceUid)
          .then(deviceUid === myUid ? setMyScope : setPeerScope)
          .catch(() => {});
      });
  }

  function scopeHandlers(deviceUid: string, scope: Scope) {
    return {
      onMode: (mode: string) =>
        changeScope(deviceUid, { ...scope, mode }, () => setScopeMode(deviceUid, mode)),
      onDirection: (direction: string) =>
        changeScope(deviceUid, { ...scope, direction }, () =>
          setScopeDirection(deviceUid, direction),
        ),
      onToggle: (changes: { uid: string; on: boolean }[]) => {
        const off = new Set(changes.filter((c) => !c.on).map((c) => c.uid));
        const on = changes.filter((c) => c.on).map((c) => c.uid);
        const selected = [...new Set([...scope.selected.filter((s) => !off.has(s)), ...on])];
        changeScope(deviceUid, { ...scope, selected }, () =>
          setScopePlaylists(deviceUid, changes),
        );
      },
    };
  }

  async function doFreeSpace() {
    setFreeing(true);
    try {
      const [n, bytes] = await freeSpace();
      onStatus(n ? `Freed ${formatBytes(bytes)} from ${n} file(s)` : 'Nothing to free');
      reloadLocal();
      onLibraryChanged();
    } catch (e) {
      onStatus(String(e));
    } finally {
      setFreeing(false);
    }
  }

  // El código va sobre todo lo demás: es una decisión de seguridad y no puede
  // quedar escondida detrás de un scroll.
  if (pairing) {
    return (
      <Modal
        title={pairing.incoming ? `${pairing.name} wants to pair` : `Pair with ${pairing.name}`}
        onClose={() => decide(false)}
      >
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

  const openPeer = peers.find((p) => p.uid === openUid);

  if (openUid && openPeer) {
    const handlers = scopeHandlers(openUid, peerScope);
    const busy = busyPeer === openUid;
    const isServer = openPeer.platform === PLATFORM_SERVER;
    return (
      <Modal title={openPeer.name} onClose={onClose} wide>
        <div className="settings">
          <button className="sync-back" onClick={() => setOpenUid(null)}>
            <ChevronLeft size={14} /> All devices
          </button>

          <section>
            <div className="set-row">
              <div className="set-label">
                {/* Un server no está ni deja de estar "en esta red": vive
                    afuera y se lo llama por su dirección. */}
                <span>
                  {isServer
                    ? openPeer.online
                      ? 'Reachable'
                      : 'Unreachable'
                    : openPeer.online
                      ? 'On this network'
                      : 'Not on this network'}
                </span>
                <small>
                  {isServer
                    ? `Archive server · ${openPeer.addrs[0] ?? '?'}:${openPeer.port}`
                    : `${openPeer.platform} · ${openPeer.paired ? 'Paired' : 'Not paired'}`}
                </small>
              </div>
              <div className="set-control">
                {openPeer.online && openPeer.paired && (
                  <>
                    <button
                      disabled={busy}
                      onClick={() => {
                        setBusyPeer(openUid);
                        previewSync(openUid).catch((e) => {
                          setBusyPeer(null);
                          onStatus(String(e));
                        });
                      }}
                    >
                      Preview
                    </button>
                    <button
                      className="primary"
                      disabled={busy}
                      onClick={() => {
                        setBusyPeer(openUid);
                        syncFiles(openUid).catch((e) => {
                          setBusyPeer(null);
                          onStatus(String(e));
                        });
                      }}
                    >
                      Sync now
                    </button>
                  </>
                )}
              </div>
            </div>
            {progress[openUid] && <TransferProgress p={progress[openUid]} />}
            {!progress[openUid] && plans[openUid] && <PlanSummary ev={plans[openUid]} />}
          </section>

          <section>
            <h4>What {openPeer.name} does</h4>
            {/* Un server no elige: existe para tener todo y para poder
                devolvértelo. Dejarlo configurable sería dejarte romper el
                respaldo en silencio — con media biblioteca seleccionada el
                archivo queda con agujeros, y en "solo envía" el día que
                quieras recuperar no te devuelve nada. */}
            {isServer ? (
              <>
                <div className="set-row">
                  <div className="set-label">
                    <span>Takes everything, gives everything back</span>
                    <small>Not editable — an archive with gaps is not an archive</small>
                  </div>
                </div>
                <p className="set-note">
                  {openPeer.name} accepts whatever a device sends it and hands back whatever that
                  device is missing. Picking only some playlists on a device changes nothing here —
                  it still backs up everything it has. Same song from three devices takes up space
                  once: files are identified by their contents.
                </p>
                {/* Lo que el server acepte no alcanza: si este dispositivo no
                    manda, no manda tampoco acá. Decirlo en el panel del server
                    es el único lugar donde alguien lo va a leer a tiempo. */}
                {(myScope.direction === 'receive' || myScope.direction === 'off') && (
                  <p className="set-note warn">
                    This device is set to{' '}
                    <strong>{myScope.direction === 'off' ? 'paused' : 'only receive'}</strong>, so it
                    sends nothing — not even to {openPeer.name}. Nothing you import here is backed
                    up until you change that above, under this device.
                  </p>
                )}
                <div className="set-row">
                  <div className="set-label">
                    <span>Restore everything here</span>
                    <small>
                      Brings back every track, playlist and folder this device is missing. Nothing
                      here is overwritten or removed — it only fills gaps.
                    </small>
                  </div>
                  <button
                    disabled={busy || !openPeer.online}
                    onClick={() => {
                      setBusyPeer(openUid);
                      syncFiles(openUid).catch((e) => {
                        setBusyPeer(null);
                        onStatus(String(e));
                      });
                    }}
                  >
                    {busy ? 'Restoring…' : 'Restore'}
                  </button>
                </div>
              </>
            ) : (
              <>
                <ScopeEditor nodes={nodes} scope={peerScope} {...handlers} />
                <p className="set-note">
                  These belong to {openPeer.name} and can be edited from any device. Unchecking
                  stops syncing those files — it never deletes anything; {openPeer.name} keeps what
                  it already has until someone frees the space <em>on that device</em>. A track that
                  is also in a playlist you left checked stays in.
                </p>
              </>
            )}
          </section>

          <section>
            <h4>History</h4>
            {history.length === 0 ? (
              <p className="set-note">Nothing yet.</p>
            ) : (
              <ul className="hist">
                {history.map((h, i) => (
                  <li key={i}>
                    <span>{h.kind}</span>
                    <small>{h.detail}</small>
                    <time>{formatWhen(h.ts)}</time>
                  </li>
                ))}
              </ul>
            )}
          </section>

          <section>
            <div className="set-row">
              <div className="set-label">
                <span>Unlink this device</span>
                <small>Stops all syncing. Nothing is deleted on either side.</small>
              </div>
              <button
                className="danger-btn"
                onClick={async () => {
                  try {
                    await unpairDevice(openUid);
                    onStatus(`Unpaired ${openPeer.name}`);
                    setOpenUid(null);
                    reloadPeers();
                  } catch (e) {
                    onStatus(String(e));
                  }
                }}
              >
                Unlink
              </button>
            </div>
          </section>
        </div>
      </Modal>
    );
  }

  const myHandlers = scopeHandlers(myUid, myScope);

  return (
    <Modal title="Sync" onClose={onClose} wide>
      <div className="settings">
        <section>
          <h4>This device</h4>
          <div className="set-row">
            <div className="set-label">
              <span>Name</span>
              <small>What other devices see on your network</small>
            </div>
            <input
              className="set-input"
              value={deviceName}
              maxLength={48}
              onChange={(e) => setDeviceNameState(e.target.value)}
              onBlur={saveName}
              onKeyDown={(e) => {
                if (e.key === 'Enter') (e.target as HTMLInputElement).blur();
              }}
            />
          </div>
          <div className="set-row">
            <div className="set-label">
              <span>Sync automatically</span>
              <small>When something changes here, when a device shows up, and periodically</small>
            </div>
            <Switch
              checked={autoSync}
              onChange={async (v) => {
                setAutoSync(v);
                try {
                  await setAutoSyncP2p(v);
                } catch (e) {
                  onStatus(String(e));
                }
              }}
            />
          </div>
          {storage && (
            <div className="set-row">
              <div className="set-label">
                <span>Storage</span>
                <small>
                  {formatBytes(storage.libraryBytes)} in {storage.tracksPresent} files
                  {storage.tracksAbsent > 0 && ` · ${storage.tracksAbsent} not downloaded`}
                </small>
              </div>
              <div className="set-control">
                {storage.freeableCount > 0 ? (
                  <button onClick={doFreeSpace} disabled={freeing}>
                    {freeing ? 'Freeing…' : `Free ${formatBytes(storage.freeableBytes)}`}
                  </button>
                ) : (
                  <span className="set-value">Nothing to free</span>
                )}
              </div>
            </div>
          )}
          {storage && storage.freeableCount > 0 && (
            <p className="set-note">
              {storage.freeableCount} file(s) are outside this device’s selection and already live on
              another linked device. Freeing moves them to the trash for 30 days; the tracks stay in
              your library, greyed out, and come back if you select their playlist again.
            </p>
          )}
          <ScopeEditor nodes={nodes} scope={myScope} {...myHandlers} />
          {myScope.mode === 'selected' && (
            <p className="set-note">
              Tracks left out are dimmed in the library and stop syncing here — nothing is removed
              until you free the space. A track that also sits in a checked playlist stays in.
            </p>
          )}
        </section>

        <section>
          <h4>Devices</h4>
          <div className="set-row">
            <div className="set-label">
              <span>On this network</span>
              <small>
                {peers.length === 0 ? 'Looking for other devices running Sway…' : `${peers.length} found`}
              </small>
            </div>
            <button onClick={rescan} disabled={scanning}>
              {scanning ? 'Scanning…' : 'Refresh'}
            </button>
          </div>
          {peers.length > 0 && (
            <ul className="peer-list">
              {peers.map((p) => {
                const incompatible = p.online && p.proto !== SYNC_PROTO;
                return (
                  <li key={p.uid} className={'peer' + (p.online ? '' : ' offline')}>
                    <div className="set-label">
                      <span>{p.name}</span>
                      <small>
                        {p.paired && summaries[p.uid]
                          ? summaries[p.uid]
                          : // Un server nunca está "en esta red": vive afuera y
                            // se lo llama por su dirección, así que decirlo
                            // sería mentir en los dos estados.
                            p.platform === PLATFORM_SERVER
                            ? `Archive server · ${p.addrs[0] ?? '?'}:${p.port}`
                            : !p.online
                              ? 'Not on this network'
                              : `${p.platform} · ${p.addrs[0] ?? 'no address'}:${p.port}`}
                      </small>
                    </div>
                    <div className="set-control">
                      <span className={'peer-badge' + (p.paired && p.online ? ' ok' : '')}>
                        {!p.online
                          ? p.platform === PLATFORM_SERVER
                            ? 'Unreachable'
                            : 'Offline'
                          : incompatible
                            ? 'Other version'
                            : p.paired
                              ? 'Paired'
                              : 'Not paired'}
                      </span>
                      {p.paired ? (
                        <button onClick={() => setOpenUid(p.uid)}>
                          Settings <ChevronRight size={13} />
                        </button>
                      ) : p.platform === PLATFORM_SERVER ? (
                        // Un server no se vincula con el código de seis
                        // dígitos: hay que darle su token. Ofrecer el botón de
                        // siempre mandaría el flujo de la red local, que no lo
                        // lleva, y el server contestaría que el token no es.
                        <button disabled title="Add it again below, with its token">
                          Needs token
                        </button>
                      ) : (
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
                          {busyPeer === p.uid ? 'Connecting…' : 'Pair'}
                        </button>
                      )}
                    </div>
                    {progress[p.uid] && <TransferProgress p={progress[p.uid]} />}
                  </li>
                );
              })}
            </ul>
          )}
          <p className="set-note">
            Sync moves files in both directions, then playlists, folders, order and metadata. Each
            device decides what it keeps. Deleting a track deletes it everywhere — but never
            straight out: the file sits in each device's trash for 30 days first.
          </p>
        </section>

        {limits && conditions && (conditions.metered !== null || conditions.batteryPct !== null) && (
          <section>
            <h4>When to sync automatically</h4>
            {/* Sólo se muestra lo que este dispositivo puede medir. Una PC de
                escritorio no tiene batería: preguntar por ella sería ruido. */}
            {conditions.metered !== null && (
              <div className="set-row">
                <div className="set-label">
                  <span>Sync over metered connections</span>
                  <small>
                    This network is {conditions.metered ? 'metered' : 'not metered'}. Only syncing
                    with the archive server uses your data plan — devices on the same network never
                    do.
                  </small>
                </div>
                <Switch
                  checked={limits.onMetered}
                  onChange={(on) => {
                    setLimits({ ...limits, onMetered: on });
                    setSyncLimits(on, limits.minBattery).catch((e) => onStatus(String(e)));
                  }}
                />
              </div>
            )}
            {conditions.batteryPct !== null && (
              <div className="set-row">
                <div className="set-label">
                  <span>Pause below {limits.minBattery}%</span>
                  <small>
                    Battery is at {conditions.batteryPct}%
                    {conditions.charging ? ' and charging — limits do not apply' : ''}. Set to 0 to
                    never pause.
                  </small>
                </div>
                <input
                  className="set-slider"
                  type="range"
                  min={0}
                  max={50}
                  step={5}
                  value={limits.minBattery}
                  onChange={(e) => setLimits({ ...limits, minBattery: Number(e.target.value) })}
                  onPointerUp={() =>
                    setSyncLimits(limits.onMetered, limits.minBattery).catch((e) =>
                      onStatus(String(e)),
                    )
                  }
                />
              </div>
            )}
            <p className="set-note">
              These only hold back automatic syncing. Hitting Sync on a device always runs — it is
              the way out when the system is wrong about the network.
            </p>
          </section>
        )}

        <section>
          <h4>Archive server</h4>
          <div className="set-row">
            <div className="set-label">
              <span>Add a server</span>
              <small>
                A server keeps a copy of everything and stays reachable from outside your home
                network. It has no screen, so it takes a token instead of a code.
              </small>
            </div>
          </div>
          <div className="set-row">
            <input
              className="set-input"
              placeholder="host or address"
              value={serverHost}
              onChange={(e) => setServerHost(e.target.value)}
              disabled={addingServer}
            />
            <input
              className="set-input port"
              placeholder="7420"
              inputMode="numeric"
              value={serverPort}
              onChange={(e) => setServerPort(e.target.value)}
              disabled={addingServer}
            />
          </div>
          <div className="set-row">
            <input
              className="set-input"
              type="password"
              placeholder="pairing token"
              value={serverToken}
              onChange={(e) => setServerToken(e.target.value)}
              disabled={addingServer}
            />
            <button
              className="primary"
              disabled={addingServer || !serverHost.trim() || !serverToken.trim()}
              onClick={() => {
                const port = Number(serverPort.trim() || '7420');
                if (!Number.isInteger(port) || port < 1 || port > 65535) {
                  onStatus('Port must be a number between 1 and 65535');
                  return;
                }
                setAddingServer(true);
                pairWithServer(serverHost.trim(), port, serverToken.trim())
                  .then((name) => {
                    // El token no se guarda ni se deja escrito: ya cumplió, y
                    // es una contraseña.
                    setServerToken('');
                    onStatus(`${name} paired`);
                    reloadPeers();
                  })
                  .catch((e) => onStatus(String(e)))
                  .finally(() => setAddingServer(false));
              }}
            >
              {addingServer ? 'Connecting…' : 'Add server'}
            </button>
          </div>
          <p className="set-note">
            The token is in the server's <code>sway-server.toml</code>. Once paired, the server
            shows up in the list above like any other device.
          </p>
        </section>
      </div>
    </Modal>
  );
}

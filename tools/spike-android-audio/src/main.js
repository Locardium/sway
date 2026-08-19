// Spike Media3: reproducir un FLAC LOCAL via tauri-plugin-native-audio (ExoPlayer).
import {
  initialize,
  setSource,
  play,
  pause,
  seekTo,
  getState,
  addStateListener,
} from 'tauri-plugin-native-audio-api';

const logEl = document.getElementById('log');
const posEl = document.getElementById('pos');

function log(msg, cls = 'info') {
  const line = document.createElement('div');
  line.className = cls;
  line.textContent = `[${new Date().toLocaleTimeString()}] ${msg}`;
  logEl.appendChild(line);
  logEl.scrollTop = logEl.scrollHeight;
}
function fmt(s) {
  if (s == null || isNaN(s)) return '--:--';
  s = Math.max(0, Math.floor(s));
  return `${Math.floor(s / 60)}:${String(s % 60).padStart(2, '0')}`;
}

// FLAC en el storage INTERNO de la app (la app lo lee sin permisos, sin FUSE).
const FLAC_PATH = 'file:///data/data/com.sway.spikeaudio/files/sample.flac';

document.getElementById('init').addEventListener('click', async () => {
  try {
    log('initialize()...');
    await initialize();
    log('initialize OK', 'ok');
    log(`setSource("${FLAC_PATH}")...`);
    await setSource({ src: FLAC_PATH, id: 1, title: 'Sample FLAC', artist: 'Spike' });
    log('setSource OK — FLAC cargado', 'ok');
  } catch (e) {
    log('FALLO init/setSource: ' + JSON.stringify(e), 'err');
  }
});

document.getElementById('play').addEventListener('click', async () => {
  try { await play(); log('play() OK', 'ok'); } catch (e) { log('play FALLO: ' + JSON.stringify(e), 'err'); }
});
document.getElementById('seek').addEventListener('click', async () => {
  try { await seekTo(90); log('seekTo(90) OK', 'ok'); } catch (e) { log('seek FALLO: ' + JSON.stringify(e), 'err'); }
});
document.getElementById('pause').addEventListener('click', async () => {
  try { await pause(); log('pause() OK', 'ok'); } catch (e) { log('pause FALLO: ' + JSON.stringify(e), 'err'); }
});

// Escuchar estado (posicion/duracion/estado)
try {
  addStateListener((st) => {
    const p = st?.position ?? st?.positionSeconds ?? st?.currentTime;
    const d = st?.duration ?? st?.durationSeconds;
    posEl.textContent = `pos: ${fmt(p)} / ${fmt(d)}  [${st?.state ?? st?.playbackState ?? '?'}]`;
    if (st?.error) log('state.error: ' + JSON.stringify(st.error), 'err');
  });
  log('state listener conectado', 'info');
} catch (e) {
  log('addStateListener no disponible: ' + e, 'info');
}

// Poll de respaldo por si el listener no emite
setInterval(async () => {
  try {
    const st = await getState();
    if (st) {
      const p = st.position ?? st.positionSeconds ?? st.currentTime;
      const d = st.duration ?? st.durationSeconds;
      posEl.textContent = `pos: ${fmt(p)} / ${fmt(d)}  [${st.state ?? st.playbackState ?? '?'}]`;
    }
  } catch {}
}, 1000);

log('listo. 1) Initialize  2) Play  3) Seek  4) Pause', 'info');

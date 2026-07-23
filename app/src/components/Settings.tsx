import { useState } from 'react';
import { Modal } from './Modal';

interface Props {
  trackCount: number;
  volume: number;
  onClose: () => void;
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
export default function Settings({ trackCount, volume, onClose }: Props) {
  const [gapless, setGapless] = useState(true);
  const [autoplay, setAutoplay] = useState(true);
  const [crossfade, setCrossfade] = useState(0);
  const [compact, setCompact] = useState(false);
  const [normalize, setNormalize] = useState(false);
  const [theme, setTheme] = useState('dark');
  const [accent, setAccent] = useState('sky');

  const accents: { id: string; color: string }[] = [
    { id: 'sky', color: 'oklch(80% 0.11 220)' },
    { id: 'green', color: 'oklch(80% 0.15 155)' },
    { id: 'violet', color: 'oklch(72% 0.15 300)' },
    { id: 'amber', color: 'oklch(80% 0.13 75)' },
    { id: 'rose', color: 'oklch(72% 0.16 15)' },
  ];

  return (
    <Modal title="Settings" onClose={onClose}>
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
              className="set-slider"
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

        <section>
          <h4>Export</h4>
          <div className="set-row">
            <div className="set-label">
              <span>Rekordbox / iTunes XML</span>
              <small>Coming in the next phase</small>
            </div>
            <button disabled>Export…</button>
          </div>
          <div className="set-row">
            <div className="set-label">
              <span>Serato crates</span>
              <small>Coming in the next phase</small>
            </div>
            <button disabled>Export…</button>
          </div>
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

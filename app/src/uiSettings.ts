import { useEffect, useRef, useState } from 'react';
import { getUiSetting, setUiSetting } from './api';

/// A small piece of UI state kept in the database — which lives in
/// `<Music>/Sway` — rather than in the webview's `localStorage`, which sits in
/// the browser profile under AppData. Nothing Sway owns belongs there.
///
/// Reading is a round trip now, so the value starts at `initial` and is
/// replaced when the stored one lands. Writes are held back until that has
/// happened, or the first render would overwrite what is stored with the
/// default.
///
/// `legacyKey`, when given, is a `localStorage` key to carry across once and
/// then drop — but only after the value is safely in the db, so a failed write
/// can't throw the setting away with nowhere to have put it.
export function useUiSetting<T>(
  key: string,
  initial: T,
  decode: (raw: string) => T | null,
  encode: (value: T) => string,
  opts?: {
    legacyKey?: string;
    /// When false the value is plain component state: never read, never
    /// written. Used where a stored value would be wrong for the device
    /// (a desktop volume must not hold the phone at half level).
    enabled?: boolean;
    /// How long writes are collapsed for. The default suits values that
    /// change while dragging; a discrete toggle just waits this long before
    /// its single write.
    writeDelayMs?: number;
  },
) {
  const [value, setValue] = useState<T>(initial);
  const loaded = useRef(false);
  // Held in refs so an inline `decode`/`encode` doesn't re-run the effects on
  // every render.
  const codec = useRef({ decode, encode });
  codec.current = { decode, encode };

  const enabled = opts?.enabled ?? true;
  const legacyKey = opts?.legacyKey;
  const writeDelayMs = opts?.writeDelayMs ?? 250;

  useEffect(() => {
    if (!enabled) return;
    let alive = true;
    (async () => {
      let raw: string | null = null;
      try {
        raw = await getUiSetting(key);
      } catch {
        // Nothing stored we can reach: `initial` stands.
      }
      let carriedOver = true; // nothing to carry
      if (raw == null && legacyKey) {
        const legacy = localStorage.getItem(legacyKey);
        if (legacy != null) {
          raw = legacy;
          carriedOver = false;
          try {
            await setUiSetting(key, legacy);
            carriedOver = true;
          } catch {
            // Retried next launch; the key stays put until then.
          }
        }
      }
      if (!alive) return;
      if (raw != null) {
        const parsed = codec.current.decode(raw);
        if (parsed != null) setValue(parsed);
      }
      loaded.current = true;
      if (carriedOver && legacyKey) localStorage.removeItem(legacyKey);
    })();
    return () => {
      alive = false;
    };
  }, [key, legacyKey, enabled]);

  // Dragging a volume slider or a column divider changes the value on every
  // pointer move. localStorage shrugged that off; a db write per tick would
  // not, so writes are throttled: the first change schedules one, and it
  // persists whatever the value has become by the time it fires. The last
  // change of a drag always schedules a timer of its own, so the value that
  // ends up stored is the one the user let go on.
  const pending = useRef<string | null>(null);
  const timer = useRef<ReturnType<typeof setTimeout> | null>(null);

  useEffect(() => {
    if (!enabled || !loaded.current) return;
    pending.current = codec.current.encode(value);
    if (timer.current != null) return; // a write is already on its way
    timer.current = setTimeout(() => {
      timer.current = null;
      const v = pending.current;
      if (v != null) setUiSetting(key, v).catch(() => {});
    }, writeDelayMs);
    // Deliberately not cleared on unmount: letting the last write land is
    // worth more than the stray timer it costs on the way out.
  }, [key, value, enabled, writeDelayMs]);

  return [value, setValue] as const;
}

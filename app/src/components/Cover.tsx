import { useEffect, useRef, useState } from 'react';
import { Music } from 'lucide-react';
import { coverThumb } from '../api';

// Global cover cache by track id (data-URL or null if it has none).
const cache = new Map<number, string | null>();
const pending = new Map<number, Promise<string | null>>();

function fetchCover(id: number): Promise<string | null> {
  if (cache.has(id)) return Promise.resolve(cache.get(id)!);
  let p = pending.get(id);
  if (!p) {
    p = coverThumb(id)
      .catch(() => null)
      .then((v) => {
        cache.set(id, v);
        pending.delete(id);
        return v;
      });
    pending.set(id, p);
  }
  return p;
}

interface Props {
  trackId: number | null;
  className?: string;
  /** true: loads immediately (player/panel). false: waits to become visible (rows). */
  eager?: boolean;
}

export default function Cover({ trackId, className, eager }: Props) {
  const ref = useRef<HTMLDivElement>(null);
  const [src, setSrc] = useState<string | null>(
    trackId != null ? cache.get(trackId) ?? null : null,
  );
  const [loaded, setLoaded] = useState(trackId != null && cache.has(trackId));

  useEffect(() => {
    if (trackId == null) {
      setSrc(null);
      setLoaded(false);
      return;
    }
    if (cache.has(trackId)) {
      setSrc(cache.get(trackId)!);
      setLoaded(true);
      return;
    }
    setSrc(null);
    setLoaded(false);
    let alive = true;
    const load = () => fetchCover(trackId).then((v) => {
      if (alive) {
        setSrc(v);
        setLoaded(true);
      }
    });
    if (eager) {
      load();
      return () => {
        alive = false;
      };
    }
    const el = ref.current;
    if (!el) return;
    const io = new IntersectionObserver((entries) => {
      if (entries.some((e) => e.isIntersecting)) {
        io.disconnect();
        load();
      }
    });
    io.observe(el);
    return () => {
      alive = false;
      io.disconnect();
    };
  }, [trackId, eager]);

  return (
    <div ref={ref} className={'cover ' + (className ?? '')}>
      {src ? (
        <img src={src} alt="" draggable={false} />
      ) : (
        <Music size={loaded ? 16 : 14} className="cover-fallback" aria-hidden="true" />
      )}
    </div>
  );
}

#!/usr/bin/env node
// Phase 0 PoC — "iTunes Music Library.xml" generator
//
// Scans an audio folder and emits a plist that replicates the exact format
// of iTunes/Music on Windows, so that BOTH Serato AND Rekordbox can import it
// (Rekordbox is stricter than Serato about the format).
//
// Usage:
//   node generate.mjs "C:\\path\\to\\your\\Music" [output.xml]
//
// - Each top-level subfolder becomes a playlist, plus a
//   master "Library" playlist with everything.
// - Tags (Name/Artist/Album/Genre/duration/sample rate/BPM/year) via
//   music-metadata; if it fails, falls back to the filename.

import { promises as fs } from 'node:fs';
import path from 'node:path';
import crypto from 'node:crypto';

let parseFile = null;
try {
  ({ parseFile } = await import('music-metadata'));
} catch {
  console.warn('[warn] music-metadata not installed — using filename as title. Run `pnpm install`.\n');
}

const AUDIO_EXT = new Set(['.flac', '.mp3', '.wav', '.m4a', '.aac', '.aif', '.aiff', '.ogg', '.opus']);

const KIND_BY_EXT = {
  '.flac': 'FLAC audio file',
  '.mp3': 'MPEG audio file',
  '.wav': 'WAV audio file',
  '.m4a': 'AAC audio file',
  '.aac': 'AAC audio file',
  '.aif': 'AIFF audio file',
  '.aiff': 'AIFF audio file',
  '.ogg': 'Ogg audio file',
  '.opus': 'Opus audio file',
};

function xmlEscape(s) {
  // XML 1.0 forbids NUL and most control chars. ID3 uses NUL to
  // separate multiple values (e.g. multiple Album Artist), and they end up raw in
  // the tags. Serato REJECTS the XML if a NUL appears → needs sanitizing.
  // Collapse runs of control chars (except tab/LF/CR) into a " / " separator.
  // Safe for URLs (they don't contain control chars).
  return String(s)
    .replace(/[\x00-\x08\x0B\x0C\x0E-\x1F]+/g, ' / ')
    .replace(/&/g, '&amp;')
    .replace(/</g, '&lt;')
    .replace(/>/g, '&gt;')
    .replace(/"/g, '&quot;');
}

// Date in iTunes format: 2026-07-23T03:11:37Z (UTC, no milliseconds).
function itunesDate(d) {
  return new Date(d).toISOString().replace(/\.\d{3}Z$/, 'Z');
}

// <Location> in iTunes Windows format: file://localhost/C:/Folder/file.flac
// with each segment percent-encoded. The drive's ':' is kept as-is.
function toItunesLocation(absPath) {
  const isWin = /^[a-zA-Z]:[\\/]/.test(absPath);
  const normalized = absPath.replace(/\\/g, '/');
  const parts = normalized.split('/');
  const encoded = parts.map((seg, i) => {
    if (isWin && i === 0 && /^[a-zA-Z]:$/.test(seg)) return seg;
    return encodeURIComponent(seg);
  });
  const joined = encoded.join('/');
  return isWin
    ? `file://localhost/${joined}`
    : `file://localhost${joined.startsWith('/') ? '' : '/'}${joined}`;
}

// Persistent ID = 16 uppercase hex chars, stable (derived from the path).
function persistentId(seed) {
  return crypto.createHash('md5').update(seed).digest('hex').slice(0, 16).toUpperCase();
}

async function walk(dir) {
  const out = [];
  let entries;
  try {
    entries = await fs.readdir(dir, { withFileTypes: true });
  } catch (e) {
    console.warn(`[warn] could not read ${dir}: ${e.message}`);
    return out;
  }
  for (const ent of entries) {
    const full = path.join(dir, ent.name);
    if (ent.isDirectory()) out.push(...(await walk(full)));
    else if (ent.isFile() && AUDIO_EXT.has(path.extname(ent.name).toLowerCase())) out.push(full);
  }
  return out;
}

async function readMeta(file) {
  const ext = path.extname(file).toLowerCase();
  const base = path.basename(file, ext);
  const meta = {
    name: base, artist: '', albumArtist: '', album: '', genre: '',
    totalTimeMs: 0, sampleRate: 0, bitRateKbps: 0,
    trackNumber: 0, discNumber: 0, year: 0, bpm: 0,
  };
  if (parseFile) {
    try {
      const m = await parseFile(file, { duration: true });
      const c = m.common || {};
      const f = m.format || {};
      if (c.title) meta.name = c.title;
      if (c.artist) meta.artist = c.artist;
      if (c.albumartist) meta.albumArtist = c.albumartist;
      if (c.album) meta.album = c.album;
      if (Array.isArray(c.genre) && c.genre.length) meta.genre = c.genre[0];
      if (c.track && c.track.no) meta.trackNumber = c.track.no;
      if (c.disk && c.disk.no) meta.discNumber = c.disk.no;
      if (c.year) meta.year = c.year;
      if (c.bpm) meta.bpm = Math.round(c.bpm);
      if (f.duration) meta.totalTimeMs = Math.round(f.duration * 1000);
      if (f.sampleRate) meta.sampleRate = f.sampleRate;
      if (f.bitrate) meta.bitRateKbps = Math.round(f.bitrate / 1000);
    } catch (e) {
      console.warn(`[warn] unreadable tags in ${path.basename(file)}: ${e.message}`);
    }
  }
  return meta;
}

function kv(key, type, value) {
  const inner = type === 'string' ? xmlEscape(value) : value;
  return `\t\t\t<key>${xmlEscape(key)}</key><${type}>${inner}</${type}>\n`;
}

// Field order copied from a real iTunes export (Rekordbox expects it this way).
function trackDict(id, file, meta, stat) {
  const ext = path.extname(file).toLowerCase();
  const mtime = stat ? stat.mtime : new Date();
  let s = `\t\t<key>${id}</key>\n\t\t<dict>\n`;
  s += kv('Track ID', 'integer', id);
  if (stat && stat.size) s += kv('Size', 'integer', stat.size);
  if (meta.totalTimeMs) s += kv('Total Time', 'integer', meta.totalTimeMs);
  if (meta.discNumber) s += kv('Disc Number', 'integer', meta.discNumber);
  if (meta.trackNumber) s += kv('Track Number', 'integer', meta.trackNumber);
  if (meta.year) s += kv('Year', 'integer', meta.year);
  if (meta.bpm) s += kv('BPM', 'integer', meta.bpm);
  s += kv('Date Modified', 'date', itunesDate(mtime));
  s += kv('Date Added', 'date', itunesDate(mtime));
  if (meta.bitRateKbps) s += kv('Bit Rate', 'integer', meta.bitRateKbps);
  if (meta.sampleRate) s += kv('Sample Rate', 'integer', meta.sampleRate);
  s += kv('Persistent ID', 'string', persistentId(file));
  s += kv('Track Type', 'string', 'File');
  s += kv('Name', 'string', meta.name);
  if (meta.artist) s += kv('Artist', 'string', meta.artist);
  if (meta.albumArtist) s += kv('Album Artist', 'string', meta.albumArtist);
  if (meta.album) s += kv('Album', 'string', meta.album);
  if (meta.genre) s += kv('Genre', 'string', meta.genre);
  s += kv('Kind', 'string', KIND_BY_EXT[ext] || 'Audio file');
  s += kv('Location', 'string', toItunesLocation(file));
  s += `\t\t</dict>\n`;
  return s;
}

// Master "Library" playlist (contains all tracks).
function masterPlaylist(playlistId, trackIds) {
  let s = `\t\t<dict>\n`;
  s += `\t\t\t<key>Master</key><true/>\n`;
  s += kv('Playlist ID', 'integer', playlistId);
  s += kv('Playlist Persistent ID', 'string', persistentId('playlist:Library'));
  s += `\t\t\t<key>All Items</key><true/>\n`;
  s += `\t\t\t<key>Visible</key><false/>\n`;
  s += kv('Name', 'string', 'Library');
  s += `\t\t\t<key>Playlist Items</key>\n\t\t\t<array>\n`;
  for (const tid of trackIds) s += `\t\t\t\t<dict>\n\t\t\t\t\t<key>Track ID</key><integer>${tid}</integer>\n\t\t\t\t</dict>\n`;
  s += `\t\t\t</array>\n\t\t</dict>\n`;
  return s;
}

// Smart Info/Criteria blocks that iTunes puts on EVERY folder. Serato seems to
// need them to recognize the folder. Loaded from folder-smart-blocks.json.
let FOLDER_SMART = null; // { smartInfo, smartCriteria } base64, or null

function dataBlock(key, base64) {
  let s = `\t\t\t<key>${key}</key>\n\t\t\t<data>\n`;
  for (let i = 0; i < base64.length; i += 68) s += `\t\t\t${base64.slice(i, i + 68)}\n`;
  s += `\t\t\t</data>\n`;
  return s;
}

// iTunes folder. KEY: besides Folder=true it needs a Playlist Items
// with the Track IDs of ALL its descendant playlists (the union). Without that,
// Serato won't show the folder. iTunes builds it this way for every folder.
function folderEntry(name, playlistId, persistId, parentPersistId, trackIds) {
  let s = `\t\t<dict>\n`;
  s += kv('Playlist ID', 'integer', playlistId);
  if (parentPersistId) s += kv('Parent Persistent ID', 'string', parentPersistId);
  s += kv('Playlist Persistent ID', 'string', persistId);
  s += `\t\t\t<key>All Items</key><true/>\n`;
  s += `\t\t\t<key>Folder</key><true/>\n`;
  s += kv('Name', 'string', name);
  if (FOLDER_SMART) {
    s += dataBlock('Smart Info', FOLDER_SMART.smartInfo);
    s += dataBlock('Smart Criteria', FOLDER_SMART.smartCriteria);
  }
  s += `\t\t\t<key>Playlist Items</key>\n\t\t\t<array>\n`;
  for (const tid of trackIds) s += `\t\t\t\t<dict>\n\t\t\t\t\t<key>Track ID</key><integer>${tid}</integer>\n\t\t\t\t</dict>\n`;
  s += `\t\t\t</array>\n\t\t</dict>\n`;
  return s;
}

// Normal (leaf) playlist; if parentPersistId != null, it goes inside a folder.
function playlistEntry(name, playlistId, persistId, parentPersistId, trackIds) {
  let s = `\t\t<dict>\n`;
  s += kv('Playlist ID', 'integer', playlistId);
  if (parentPersistId) s += kv('Parent Persistent ID', 'string', parentPersistId);
  s += kv('Playlist Persistent ID', 'string', persistId);
  s += `\t\t\t<key>All Items</key><true/>\n`;
  s += kv('Name', 'string', name);
  s += `\t\t\t<key>Playlist Items</key>\n\t\t\t<array>\n`;
  for (const tid of trackIds) s += `\t\t\t\t<dict>\n\t\t\t\t\t<key>Track ID</key><integer>${tid}</integer>\n\t\t\t\t</dict>\n`;
  s += `\t\t\t</array>\n\t\t</dict>\n`;
  return s;
}

async function main() {
  const args = process.argv.slice(2);
  const flat = args.includes('--flat');
  // --folder-smart=minimal|full : adds Smart Info/Criteria to folders
  // (iTunes does it on every folder; Serato seems to need it to show them).
  const fsArg = (args.find((a) => a.startsWith('--folder-smart=')) || '').split('=')[1];
  if (fsArg === 'minimal' || fsArg === 'full') {
    const blocks = JSON.parse(await fs.readFile(new URL('./folder-smart-blocks.json', import.meta.url), 'utf8'));
    FOLDER_SMART = {
      smartInfo: blocks.smartInfo,
      smartCriteria: fsArg === 'full' ? blocks.smartCriteriaFull : blocks.smartCriteriaMinimal,
    };
    console.log(`[folder-smart=${fsArg}] Smart Info/Criteria added to folders.`);
  }
  const positional = args.filter((a) => !a.startsWith('--'));
  const root = positional[0];
  const outPath = positional[1] || path.join(process.cwd(), 'iTunes Music Library.xml');
  if (!root) {
    console.error('Usage: node generate.mjs "C:\\\\path\\\\to\\\\Music" [output.xml] [--flat] [--folder-smart=minimal|full]');
    console.error('  --flat: flat playlists (no folders).');
    console.error('  --folder-smart: adds smart metadata to folders (Serato compat).');
    process.exit(1);
  }
  const absRoot = path.resolve(root);
  console.log(`Scanning: ${absRoot}`);
  const files = (await walk(absRoot)).sort();
  if (files.length === 0) {
    console.error('No audio files found.');
    process.exit(1);
  }
  console.log(`Found ${files.length} files. Reading tags...`);

  const tracks = [];
  let id = 1;
  for (const file of files) {
    const [meta, stat] = await Promise.all([readMeta(file), fs.stat(file).catch(() => null)]);
    tracks.push({ id: id++, file, meta, stat });
  }

  // --- Build the directory tree from the track paths ---
  // filesByDir: absolute dir -> [trackId] (files DIRECTLY in that dir)
  // childrenByDir: absolute dir -> Set(immediate absolute subdir)
  const filesByDir = new Map();
  const childrenByDir = new Map();
  const getArr = (m, k) => { if (!m.has(k)) m.set(k, []); return m.get(k); };
  const getSet = (m, k) => { if (!m.has(k)) m.set(k, new Set()); return m.get(k); };
  for (const t of tracks) {
    const d = path.dirname(t.file);
    getArr(filesByDir, d).push(t.id);
    // register the ancestor chain up to absRoot
    let cur = d;
    while (cur !== absRoot && cur.startsWith(absRoot + path.sep)) {
      const parent = path.dirname(cur);
      getSet(childrenByDir, parent).add(cur);
      cur = parent;
    }
  }

  // Recursively emit folders + playlists. A dir with subdirs => Folder
  // (and if it also has direct files, a child playlist with those files).
  // A leaf dir => normal playlist.
  const playlistBlocks = [];
  let pid = 1001;
  // Track IDs of a dir's ENTIRE subtree (memoized). The folder needs them
  // in its Playlist Items so Serato shows it.
  const descCache = new Map();
  function descendantTrackIds(dir) {
    if (descCache.has(dir)) return descCache.get(dir);
    let ids = [...(filesByDir.get(dir) || [])];
    for (const sub of [...(childrenByDir.get(dir) || [])].sort()) ids = ids.concat(descendantTrackIds(sub));
    descCache.set(dir, ids);
    return ids;
  }

  // Hierarchical mode (default): nested iTunes folders (Rekordbox full).
  function emitDir(dir, parentPersistId) {
    const subdirs = [...(childrenByDir.get(dir) || [])].sort();
    const directFiles = filesByDir.get(dir) || [];
    const name = path.basename(dir);
    if (subdirs.length > 0) {
      const folderPersist = persistentId('folder:' + dir);
      playlistBlocks.push(folderEntry(name, pid++, folderPersist, parentPersistId, descendantTrackIds(dir)));
      if (directFiles.length > 0) {
        playlistBlocks.push(playlistEntry(name, pid++, persistentId('pl:' + dir), folderPersist, directFiles));
      }
      for (const sub of subdirs) emitDir(sub, folderPersist);
    } else {
      playlistBlocks.push(playlistEntry(name, pid++, persistentId('pl:' + dir), parentPersistId, directFiles));
    }
  }
  // Flat mode (--flat): one playlist per dir with files, name = relative
  // path joined with " - "; no folders or Parent (to test Serato compat).
  function emitFlat(dir) {
    const directFiles = filesByDir.get(dir) || [];
    if (directFiles.length > 0) {
      const rel = path.relative(absRoot, dir) || path.basename(absRoot) || 'Root';
      const name = rel.split(path.sep).join(' - ');
      playlistBlocks.push(playlistEntry(name, pid++, persistentId('pl:' + dir), null, directFiles));
    }
    for (const sub of [...(childrenByDir.get(dir) || [])].sort()) emitFlat(sub);
  }
  // Top level: direct subdirs of absRoot + loose files at the root.
  if (flat) {
    emitFlat(absRoot);
  } else {
    const topSubdirs = [...(childrenByDir.get(absRoot) || [])].sort();
    for (const sub of topSubdirs) emitDir(sub, null);
    const rootFiles = filesByDir.get(absRoot) || [];
    if (rootFiles.length > 0) {
      playlistBlocks.push(playlistEntry(path.basename(absRoot) || 'Root', pid++, persistentId('pl:' + absRoot), null, rootFiles));
    }
  }

  const musicFolder = toItunesLocation(absRoot.endsWith(path.sep) ? absRoot : absRoot + path.sep);

  let xml = '';
  xml += '<?xml version="1.0" encoding="UTF-8"?>\n';
  xml += '<!DOCTYPE plist PUBLIC "-//Apple Computer//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">\n';
  xml += '<plist version="1.0">\n<dict>\n';
  xml += '\t<key>Major Version</key><integer>1</integer>\n';
  xml += '\t<key>Minor Version</key><integer>1</integer>\n';
  xml += '\t<key>Application Version</key><string>12.13.10.3</string>\n';
  xml += `\t<key>Date</key><date>${itunesDate(Date.now())}</date>\n`;
  xml += '\t<key>Features</key><integer>5</integer>\n';
  xml += '\t<key>Show Content Ratings</key><true/>\n';
  xml += `\t<key>Library Persistent ID</key><string>${persistentId('library:' + absRoot)}</string>\n`;

  xml += '\t<key>Tracks</key>\n\t<dict>\n';
  for (const t of tracks) xml += trackDict(t.id, t.file, t.meta, t.stat);
  xml += '\t</dict>\n';

  xml += '\t<key>Playlists</key>\n\t<array>\n';
  xml += masterPlaylist(1000, tracks.map((t) => t.id));
  for (const block of playlistBlocks) xml += block;
  xml += '\t</array>\n';

  // iTunes puts Music Folder AT THE END, after Playlists.
  xml += `\t<key>Music Folder</key><string>${xmlEscape(musicFolder)}</string>\n`;
  xml += '</dict>\n</plist>\n';

  await fs.writeFile(outPath, xml, 'utf8');
  console.log(`\n✓ Written: ${outPath}`);
  console.log(`  ${tracks.length} tracks, ${playlistBlocks.length} folders/playlists + "Library".`);
  console.log('\nImport it into Rekordbox (Preferences → Advanced → Database → iTunes) and into Serato.');
  console.log("Goes into a separate sidebar node; it does NOT touch your Collection.");
}

main().catch((e) => { console.error(e); process.exit(1); });

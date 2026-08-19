# Phase 0 — iTunes XML generator PoC

Generates an `iTunes Music Library.xml` from an audio folder, to **validate that Rekordbox and Serato import it** (including FLAC) before building the entire app.

## Usage

```bash
cd tools/itunes-xml-poc
pnpm install          # only the first time (installs music-metadata to read tags)
node generate.mjs "C:\Users\User\Music" "iTunes Music Library.xml"
```

- Arg 1: root folder of your music (scans recursively).
- Arg 2 (optional): output file. Default: `iTunes Music Library.xml` in the cwd.

Each **top-level subfolder** becomes a playlist, plus a master `Library` playlist with everything.

## How to test the import (does not affect your current Collection)

The XML loads into a **separate sidebar node**, apart from your Collection. Your current playlists/cues remain intact.

**Rekordbox:**
1. Preferences → Advanced → Database → **iTunes**.
2. Check "Enable iTunes library sync" and point it to the generated `iTunes Music Library.xml`.
3. An **iTunes** tree appears in the sidebar → verify tracks, playlists, order, and that the **FLAC files play**.

**Serato:**
1. In the left panel, expand **iTunes** (Serato automatically reads the iTunes library if the XML is in the standard location, or you can set the path).
2. Verify tracks/playlists and FLAC playback.

## What we're validating

- [ ] Rekordbox imports the XML and shows tracks + playlists in the correct order.
- [ ] The **FLAC** files play in Rekordbox.
- [ ] Serato imports the XML and shows the same thing.
- [ ] The FLAC files play in Serato.
- [ ] Paths with spaces/special characters resolve correctly (no "file not found").

If something fails, we note the dialect quirk here and adjust the generator. This code is **throwaway** (PoC): the production version goes in Rust inside the core (Phase 2).

## Notes

- `<Location>` format: `file://localhost/C:/...` with each segment percent-encoded (same as iTunes on Windows).
- `Persistent ID` is derived from the path (stable across runs).
- If `music-metadata` isn't installed, it falls back to the filename as the title (the PoC still runs).

# Sway 🎧

> **A music library manager and player built by a DJ, for DJs, with love ❤️**

Sway is currently in **Beta**. Since this is a passion project, expect some bugs! I welcome all suggestions, bug reports, and code contributions to make it better.

---

## The Story Behind Sway (Why I built it)

As a DJ, organizing a local music collection (FLACs, MP3s, WAVs) can be a real headache. For years, I relied on **iTunes** to curate my library. It had a simple, powerful way to order tracks manually, and by exporting an iTunes XML file, I could easily import my structured playlists and folders directly into legacy DJ software like **Rekordbox** and **Serato**.

However, iTunes eventually presented three critical dealbreakers:
1. **It became obsolete**: Apple replaced iTunes with Apple Music, which completely removed the iTunes XML export feature, breaking my entire DJ library prep workflow.
2. **Unsupported formats**: It lacked native support for high-quality formats like **FLAC**, forcing annoying file conversions.
3. **Impossibly complex sync**: Trying to keep my music library synchronized across my two PCs and my Android phone was incredibly frustrating, slow, or outright impossible.

I built **Sway** to solve these exact problems. It is a modern, lightweight desktop and mobile application designed to replace the iTunes/Explorer flow, allowing DJs to easily manage, pre-listen, sync, and project their playlist hierarchies onto legacy DJ platforms.

---

## Key Features

* **Legacy Export First**: Sway natively creates standard iTunes-compatible XML files, ensuring your manual playlist orders and folder hierarchies carry over perfectly to Rekordbox and Serato.
* **All Formats Welcome**: Native support for **FLAC**, MP3, WAV, and more. No conversions needed.
* **Safe-by-Design File Integrity**: Your files are sacred. Sway uses a built-in virtual recycle bin to store out-of-scope files for 30 days before removal. You'll never accidentally lose a track from your hard drive again.
* **Real-time P2P Synchronization**: Easily sync your tracks and database between multiple PCs and your phone over your local network using fast peer-to-peer (P2P) transfers.
* **Your Own Backup & Remote Server**: Run a headless backup server where all your tracks from all devices are safely uploaded. You can also fetch tracks from this server remotely in real time using secure P2P connections.
* **Advanced Player & Volume Normalization**: Pre-listen to your sets with seamless crossfades, gapless playback, and smart volume normalization targeting **-6.0 LUFS** (no more sudden volume jumps between tracks!).

---

## Future Roadmap 🚀

Here is what is planned for future releases:
- **UI/UX Polish** (Coming soon): Improving layout density, shortcuts, and overall looks.
- **Native BPM Scanning**: Analyze tempo directly inside the app.
- **Comprehensive Docs**: Written guides on workflow setups.
- **macOS & iOS Support**: Planned for future phases once the project gains traction (currently limited by testing hardware!).

---

## Technical Overview (For Developers)

Sway is built as a Rust and pnpm monorepo using **Tauri v2** and **React 18**.

### Project Structure
```
sway/
├── app/                  # Tauri frontend application (Vite + React + TS)
│   ├── src/              # React UI (TrackTable, PlayerBar, Sync, etc.)
│   └── src-tauri/        # Tauri application backend (desktop playback, watchers, platform APIs)
├── crates/
│   ├── core/             # Central engine (SQLite db, sync logic, encryption, ID3, wire protocol)
│   ├── native-audio/     # Custom fork of tauri-plugin-native-audio for Android/iOS playback
│   └── server/           # Headless synchronization server (axum/tokio-based CLI)
└── scripts/              # Release automation scripts
```

- **`crates/core`**: Decoupled engine handling SQLite database merges (`rusqlite`), `blake3` file hashing, metadata parsing, and the secure **ChaCha20-Poly1305** encrypted raw socket network protocol (`wire.rs`).
- **`crates/server`**: Headless backend daemon designed to run on a Linux VPS (like Ubuntu headless) to act as a centralized backup node.
- **`app/src-tauri`**: Controls OS-specific file watchers, loudness analysis, and iTunes XML generation.
- **`app/`**: A Vite single-page application communicating with Tauri's Rust API. On Android, it routes playback commands natively to Media3/ExoPlayer.

---

## Build & Setup Instructions

### Prerequisites
Make sure you have installed:
- **Rust** 1.75+
- **Node.js** 18+ and **pnpm**
- **Android SDK** (if testing/building for Android)

### Install Node Dependencies
```bash
pnpm install
```

### Running in Development

- **Run Desktop App**:
  ```bash
  pnpm run dev
  ```
- **Run Android App**:
  ```bash
  pnpm run dev:android
  ```
- **Run Headless Server**:
  ```bash
  pnpm run server:dev
  ```

### Compiling Release Bundles

- **Build Windows Desktop Installer (`.msi`)**:
  ```bash
  pnpm run build:win
  ```
- **Build Android App (`.apk`)**:
  ```bash
  pnpm run build:android
  ```
- **Build Linux Server** (uses `cargo-zigbuild`):
  ```bash
  pnpm run server:build-linux
  ```
- **Build Windows Server**:
  ```bash
  pnpm run server:build-win
  ```
- **Build All Targets**:
  ```bash
  pnpm run build:all
  ```

---

## License

This project is proprietary. All rights reserved.

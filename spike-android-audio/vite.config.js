import { defineConfig } from 'vite';

// Tauri android dev inyecta TAURI_DEV_HOST (IP de la PC) para que el celu
// alcance el dev server por la red.
const host = process.env.TAURI_DEV_HOST;

export default defineConfig({
  clearScreen: false,
  server: {
    port: 1420,
    strictPort: true,
    host: host || false,
    hmr: host ? { protocol: 'ws', host, port: 1421 } : undefined,
    watch: { ignored: ['**/src-tauri/**'] },
  },
  build: { target: 'esnext' },
});

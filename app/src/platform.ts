import { platform } from '@tauri-apps/plugin-os';

// Fuera de Tauri (browser normal en dev sin webview) platform() no aplica.
export const isAndroid = (): boolean => {
  if (!('__TAURI_INTERNALS__' in window)) return false;
  try {
    return platform() === 'android';
  } catch {
    return false;
  }
};

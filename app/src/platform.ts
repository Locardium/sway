import { platform } from '@tauri-apps/plugin-os';

// Outside Tauri (a regular browser in dev without a webview), platform() doesn't apply.
export const isAndroid = (): boolean => {
  if (!('__TAURI_INTERNALS__' in window)) return false;
  try {
    return platform() === 'android';
  } catch {
    return false;
  }
};

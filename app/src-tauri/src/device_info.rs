//! Nombre por defecto de este dispositivo — el que ven los demas en la lista
//! de sync antes de que el usuario lo cambie a mano.
//!
//! Tiene que ser reconocible de un vistazo: con dos PCs y un celu, "Android"
//! y "Sway" no distinguen nada.
//!
//! En Android el nombre solo existe del lado de Java (`Settings.Global` /
//! `android.os.Build`), asi que lo resuelve `MainActivity` y lo deja en la
//! variable de entorno `SWAY_DEVICE_NAME` antes de arrancar el runtime de
//! Rust. Se intento leerlo por JNI desde aca y **no funciona**:
//! `ndk_context::android_context()` hace `.expect(...)` y Tauri nunca
//! inicializa ese contexto, asi que el panic aborta el proceso en el arranque.

/// Nombres que genero una version anterior y que no identifican al
/// dispositivo. Si el nombre guardado es uno de estos se recalcula, en vez de
/// dejar al celu llamandose "Android" para siempre.
pub const PLACEHOLDERS: &[&str] = &["Android", "Sway", "localhost"];

/// Variable que puebla `MainActivity` en Android. En desktop no existe y se
/// cae al nombre del equipo.
const ENV_DEVICE_NAME: &str = "SWAY_DEVICE_NAME";

pub fn default_device_name() -> String {
    [ENV_DEVICE_NAME, "COMPUTERNAME", "HOSTNAME"]
        .iter()
        .find_map(|k| std::env::var(k).ok())
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty() && !PLACEHOLDERS.contains(&s.as_str()))
        .unwrap_or_else(|| {
            if cfg!(target_os = "android") { "Android".into() } else { "Sway".into() }
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_name_is_never_empty_and_never_a_placeholder_on_desktop() {
        let name = default_device_name();
        assert!(!name.is_empty());
        if !cfg!(target_os = "android") {
            assert_ne!(name, "Android");
        }
    }
}

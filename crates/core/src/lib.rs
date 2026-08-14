//! El motor de Sway: biblioteca, identidad de archivos y sincronización.
//!
//! Acá vive todo lo que no necesita que haya una pantalla. La app Tauri
//! (`app/src-tauri`) y el server de archivo (Fase 6.2) son dos frentes sobre
//! este mismo crate, y esa es la razón por la que existe: el server corre
//! headless en un Ubuntu sin escritorio y no puede compilar `tauri`.
//!
//! Lo que NO entra: pairing con confirmación humana, descubrimiento mDNS,
//! export a iTunes XML, reproducción. Eso sigue del lado de la app.
//!
//! El punto de entrada del sync es `engine::Host`: quien use este crate le
//! dice al motor cómo llegar a la base, dónde están los archivos y qué hacer
//! cuando algo cambia.

/// Dónde se dejan los tiempos, además del log. Lo setea quien use el crate
/// (en la app, el `setup` de Tauri).
static PERF_FILE: std::sync::OnceLock<std::path::PathBuf> = std::sync::OnceLock::new();

/// TEMPORAL — diagnóstico.
///
/// En Android los `log::*` van a logcat, pero hay dispositivos que lo traen
/// apagado a nivel sistema (`live.logcat=disable`, que ni el shell de adb puede
/// cambiar): ahí el buffer entero devuelve cero líneas y no hay forma de ver un
/// tiempo. Un archivo al lado de la DB se puede sacar con `run-as` sin tocar
/// ninguna configuración del teléfono.
pub fn perf_line(line: &str) {
    use std::io::Write;
    let Some(path) = PERF_FILE.get() else { return };
    if let Ok(mut f) = std::fs::OpenOptions::new().create(true).append(true).open(path) {
        let _ = writeln!(f, "{} {line}", db::now_ms());
    }
}

/// Dónde escribir los tiempos. Sin esto, `perf_line` no hace nada.
pub fn set_perf_file(path: std::path::PathBuf) {
    let _ = PERF_FILE.set(path);
}

pub mod db;
pub mod device_info;
pub mod engine;
pub mod hashing;
pub mod id3_sanitize;
pub mod import;
pub mod manifest;
pub mod merge;
pub mod rank;
pub mod scope;
pub mod transfer;
pub mod trash;
pub mod wire;

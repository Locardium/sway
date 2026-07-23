#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    let mut builder = tauri::Builder::default();

    #[cfg(any(target_os = "android", target_os = "ios"))]
    {
        builder = builder.plugin(tauri_plugin_native_audio::init());
    }

    builder
        .run(tauri::generate_context!())
        .expect("error while running tauri application");
}

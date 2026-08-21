// Command names are snake_case here and in `permissions/`, but the Kotlin
// methods they reach are lowerCamelCase: tauri runs the name through
// `heck::AsLowerCamelCase` before handing it to the Android plugin
// (`webview/mod.rs`, the `#[cfg(mobile)]` fallback), and `PluginHandle` looks
// the method up by its exact Kotlin name. So `set_next_source` -> `setNextSource`.
const COMMANDS: &[&str] = &[
    "initialize",
    "register_listener",
    "remove_listener",
    "set_source",
    "play",
    "pause",
    "seek_to",
    "set_rate",
    "get_state",
    "get_progress_checkpoint",
    "clear_progress_checkpoint",
    "dispose",
    // Added by the Sway fork, see FORK.md.
    "set_volume",
    "set_source_gain",
    "set_next_source",
    "skip_to_next",
    "set_crossfade",
    "list_output_devices",
    "set_output_device",
];

fn main() {
    tauri_plugin::Builder::new(COMMANDS)
        .android_path("android")
        .ios_path("ios")
        .build();
}

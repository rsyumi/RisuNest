fn main() {
    tauri_plugin::Builder::new(&[
        "state",
        "begin",
        "end",
        "request_notifications",
        "notify",
        "open_settings",
        "pick_file",
        "export_file",
        "discard_file",
        "publication",
        "generation_progress",
    ])
    .ios_path("ios")
    .build();
}

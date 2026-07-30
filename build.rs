const COMMANDS: &[&str] = &["get_metadata", "get_tracks"];

fn main() {
   tauri_plugin::Builder::new(COMMANDS).build();
}

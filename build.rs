const COMMANDS: &[&str] = &[
   "get_metadata",
   "get_tracks",
   "get_cover",
   "get_thumbnails",
   "get_subtitles",
];

fn main() {
   tauri_plugin::Builder::new(COMMANDS).build();
}

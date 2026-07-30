//! # Tauri Plugin Media Parser
//!
//! A Tauri plugin for parsing media files, providing access to metadata,
//! tracks, frames, and subtitles from both local files and remote HTTP streams.
//!
//! ## Features
//!
//! - Extract metadata (title, artist, album, duration, etc.) from media formats
//! - Support for local file paths and remote URLs (HTTP/HTTPS)
//! - Custom HTTP headers for authenticated requests
//! - Async streaming with efficient range requests
//!
//! ## Usage
//!
//! ### Rust (Plugin Registration)
//!
//! Register the plugin in your Tauri application:
//!
//! ```rust,ignore,no_run
//! fn main() {
//!     tauri::Builder::default()
//!         .plugin(tauri_plugin_media_parser::init())
//!         .run(tauri::generate_context!())
//!         .expect("error while running tauri application");
//! }
//! ```
//!
//! ### TypeScript (Frontend)
//!
//! ```typescript,ignore
//! import { getMetadata } from 'tauri-plugin-media-parser';
//!
//! // Local file
//! const metadata = await getMetadata('/path/to/video.mp4');
//!
//! // Remote URL
//! const metadata = await getMetadata('https://example.com/video.mp4');
//!
//! // With authentication headers
//! const metadata = await getMetadata('https://example.com/video.mp4', {
//!     headers: { 'Authorization': 'Bearer token123' }
//! });
//!
//! console.log(`Duration: ${metadata.duration / metadata.timescale} seconds`);
//! ```

use tauri::{Runtime, plugin::TauriPlugin};

mod commands;
mod error;

pub use error::{Error, Result};

/// Initializes the media-parser plugin.
///
/// Call this function in your Tauri application's builder to register
/// the plugin and enable its commands.
///
/// # Example
///
/// ```rust,ignore,no_run
/// fn main() {
///     tauri::Builder::default()
///         .plugin(tauri_plugin_media_parser::init())
///         .run(tauri::generate_context!())
///         .expect("error while running tauri application");
/// }
/// ```
pub fn init<R: Runtime>() -> TauriPlugin<R> {
   tauri::plugin::Builder::new("media-parser")
      .invoke_handler(tauri::generate_handler![
         commands::get_metadata,
         commands::get_tracks
      ])
      .build()
}

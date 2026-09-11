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

use std::collections::HashMap;
use tauri::{Manager, Runtime, plugin::TauriPlugin};

mod commands;
mod envelope;
mod error;
mod session_cache;
mod source;
mod subtitle_command;

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
   Builder::new().build()
}

/// Configures HTTP defaults on all platforms. Per-call headers override these defaults.
#[derive(Default)]
pub struct Builder {
   user_agent: Option<String>,
   headers: HashMap<String, String>,
   origins: Option<Vec<String>>,
}

impl Builder {
   /// Creates a builder without HTTP defaults.
   pub fn new() -> Self {
      Self::default()
   }

   /// Sets the user agent, taking precedence over `User-Agent` in default headers.
   pub fn user_agent(mut self, value: impl Into<String>) -> Self {
      self.user_agent = Some(value.into());
      self
   }

   /// Adds default headers. Later values replace earlier ones regardless of name casing.
   /// Without `default_headers_origins`, defaults go to any URL requested by the frontend.
   pub fn default_headers<K: Into<String>, V: Into<String>>(
      mut self,
      headers: impl IntoIterator<Item = (K, V)>,
   ) -> Self {
      self.headers.extend(
         headers
            .into_iter()
            .map(|(name, value)| (name.into().to_ascii_lowercase(), value.into())),
      );
      self
   }

   /// Restricts all HTTP defaults, including the user agent, to these HTTP(S) origins.
   /// Calls accumulate origins; an initially empty list allows none. Unset, defaults are global.
   /// Other destinations still receive per-call headers, but none of these defaults.
   /// Invalid URLs or non-HTTP(S) schemes fail plugin initialization.
   pub fn default_headers_origins(
      mut self,
      origins: impl IntoIterator<Item = impl Into<String>>,
   ) -> Self {
      self
         .origins
         .get_or_insert_with(Vec::new)
         .extend(origins.into_iter().map(Into::into));
      self
   }

   fn into_headers(mut self) -> HashMap<String, String> {
      if let Some(user_agent) = self.user_agent {
         self.headers.insert("user-agent".into(), user_agent);
      }
      self.headers
   }

   /// Builds the plugin. Invalid HTTP headers fail plugin initialization.
   pub fn build<R: Runtime>(mut self) -> TauriPlugin<R> {
      let origins = self.origins.take();
      let headers = self.into_headers();
      tauri::plugin::Builder::new("media-parser")
         .setup(move |app, _api| {
            for (name, value) in &headers {
               tauri::http::HeaderName::from_bytes(name.as_bytes())?;
               tauri::http::HeaderValue::from_str(value)?;
            }
            app.manage(source::DefaultHeaders::new(headers, origins)?);
            app.manage(commands::ThumbnailSessions::default());
            app.manage(subtitle_command::SubtitleSessions::default());
            Ok(())
         })
         .invoke_handler(tauri::generate_handler![
            commands::get_metadata,
            commands::get_tracks,
            commands::get_cover,
            commands::get_thumbnails,
            subtitle_command::get_subtitles
         ])
         .build()
   }
}

#[cfg(test)]
mod tests {
   use super::*;

   #[test]
   fn default_header_origins_accumulate_and_limit_where_defaults_are_sent() {
      let mut builder = Builder::new()
         .user_agent("app/1.0")
         .default_headers_origins(Vec::<String>::new())
         .default_headers_origins(["https://first.example"])
         .default_headers_origins(["https://second.example"]);
      let origins = builder.origins.take();
      let defaults = source::DefaultHeaders::new(builder.into_headers(), origins).unwrap();
      for source in ["https://first.example/file", "https://second.example/file"] {
         assert_eq!(
            defaults.merge(source, None).headers,
            Some(HashMap::from([("user-agent".into(), "app/1.0".into())]))
         );
      }
      assert_eq!(
         defaults.merge("https://third.example/file", None).headers,
         None
      );
   }

   #[test]
   fn user_agent_overrides_default_header_in_either_setter_order() {
      for builder in [
         Builder::new()
            .user_agent("my-app/1.0")
            .default_headers([("User-Agent", "other/1.0")]),
         Builder::new()
            .default_headers([("User-Agent", "other/1.0")])
            .user_agent("my-app/1.0"),
      ] {
         assert_eq!(
            builder.into_headers(),
            HashMap::from([("user-agent".into(), "my-app/1.0".into())]),
         );
      }
   }
}

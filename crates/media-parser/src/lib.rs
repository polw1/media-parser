//! # Media Parser
//!
//! An async-first library for parsing media file metadata from local files
//! and remote HTTP streams. Supports MP4/M4A/MOV and MP3 formats with
//! partial reads via HTTP range requests.
//!
//! ## Quick Start
//!
//! ### Parse a local file
//!
//! ```no_run
//! use media_parser::{MediaParser, FileStreamReader};
//!
//! #[tokio::main]
//! async fn main() -> media_parser::Result<()> {
//!     let reader = FileStreamReader::new("song.mp3")?;
//!     let parser = MediaParser::new(reader);
//!     let metadata = parser.metadata().await?;
//!
//!     println!("Title: {:?}", metadata.get("title"));
//!     println!("Artist: {:?}", metadata.get("artist"));
//!     println!("Duration: {:?}", metadata.duration);
//!     Ok(())
//! }
//! ```
//!
//! ### Parse a remote file via HTTP
//!
//! ```no_run
//! use media_parser::{MediaParser, HttpStreamReader};
//!
//! #[tokio::main]
//! async fn main() -> media_parser::Result<()> {
//!     let url = "https://example.com/video.mp4";
//!     let reader = HttpStreamReader::new(url).await?;
//!     let parser = MediaParser::new(reader);
//!     let metadata = parser.metadata().await?;
//!
//!     println!("Format: {}", metadata.format);
//!     println!("Duration: {}ms", metadata.duration);
//!     Ok(())
//! }
//! ```
//!
//! ## API Overview
//!
//! ### Entry Points
//!
//! | Type | Description |
//! |------|-------------|
//! | [`MediaParser`] | High-level parser handle wrapping a stream reader |
//! | [`FileStreamReader`] | Read from local filesystem |
//! | [`HttpStreamReader`] | Read from HTTP/HTTPS URLs with range requests |
//!
//! ### Core Types
//!
//! | Type | Description |
//! |------|-------------|
//! | [`Metadata`] | Container for all extracted metadata fields |
//! | [`VideoTrackMeta`] | Video track information (codec, dimensions, framerate) |
//! | [`AudioTrackMeta`] | Audio track information (codec, channels, sample rate) |
//! | [`SubtitleTrack`] | Subtitle track with cues and timing |
//!
//! ### Registry Functions
//!
//! For lower-level access, use the registry functions directly:
//!
//! ```no_run
//! use media_parser::{detect_format, parse_metadata, supported_formats};
//!
//! // List supported formats
//! for format in supported_formats() {
//!     println!("{}: {:?}", format.name, format.extensions);
//! }
//! ```
//!
//! ## Architecture
//!
//! The library is organized in layers, each with a single responsibility:
//!
//! ```text
//! ┌─────────────────────────────────────────────┐
//! │            Application Layer                │
//! │      MediaParser<R: StreamReader>           │
//! │  High-level API: metadata(), tracks(), etc  │
//! ├─────────────────────────────────────────────┤
//! │             Registry Layer                  │
//! │   detect_format() → dispatch to parser      │
//! │   Format detection by markers + extension   │
//! ├─────────────────────────────────────────────┤
//! │             Format Layer                    │
//! │    mp4::parse()      mp3::parse()           │
//! │    Format-specific parsing logic            │
//! ├─────────────────────────────────────────────┤
//! │             Stream Layer                    │
//! │   FileStreamReader    HttpStreamReader      │
//! │   Unified async read interface              │
//! └─────────────────────────────────────────────┘
//! ```
//!
//! ## Supported Formats
//!
//! | Format | Extensions | Metadata |
//! |--------|------------|----------|
//! | MP4/M4A/MOV | `.mp4`, `.m4a`, `.m4v`, `.mov` | iTunes tags, video/audio tracks |
//! | MP3 | `.mp3` | ID3v2 tags, frame-accurate duration |
//!
//! ## Error Handling
//!
//! All fallible operations return [`Result<T>`], which is an alias for
//! `std::result::Result<T, MediaParserError>`. The [`MediaParserError`] enum
//! covers I/O errors, parse failures, and unsupported formats.
//!
//! ```no_run
//! use media_parser::{MediaParser, FileStreamReader, MediaParserError};
//!
//! match FileStreamReader::new("file.mp4") {
//!     Ok(reader) => { /* ... */ }
//!     Err(MediaParserError::Io(e)) => eprintln!("I/O error: {}", e),
//!     Err(e) => eprintln!("Error: {}", e),
//! }
//! ```

pub mod errors;
pub mod format;
pub mod helpers;
pub mod stream;
pub mod types;
use std::time::Duration;

// Public API
pub use errors::{MediaParserError, Result};
pub use format::mp4::atoms::Mp4Nav;
pub use format::registry::{
   detect_format, get_format_info, is_supported, parse_metadata, parse_tracks, supported_formats,
};
pub use stream::{FileStreamReader, HttpStreamReader, StreamReader};
pub use types::{
   AudioTrackMeta, BaseTrackMeta, Frame, Meta, Metadata, PixelFormat, SubtitleCue, SubtitleTrack,
   SubtitleTrackMeta, TrackFilter, TrackType, UnknownTrackMeta, VideoTrackMeta,
};

/// High-level parser handle.
#[derive(Debug)]
pub struct MediaParser<R: StreamReader> {
   reader: R,
}

impl<R: StreamReader> MediaParser<R> {
   pub fn new(reader: R) -> Self {
      Self { reader }
   }

   /// Extract metadata from the media file.
   pub async fn metadata(&self) -> Result<Metadata> {
      format::registry::parse_metadata(&self.reader).await
   }

   // Extract all tracks from the media file.
   pub async fn tracks(&self) -> Result<Vec<TrackType>> {
      format::registry::parse_tracks(&self.reader).await
   }

   /// Extract subtitle tracks from the media file.
   pub async fn subtitles(&self, filter: Option<TrackFilter>) -> Result<Vec<SubtitleTrack>> {
      // TODO: Implement actual subtitle parsing
      let _ = filter; // Suppress unused parameter warning
      Ok(vec![])
   }

   /// Extract a single frame from a video track at the specified timestamp.
   pub async fn frame(&self, track_id: u32, timestamp: Duration) -> Result<Frame> {
      // TODO: Implement actual frame extraction
      Ok(Frame {
         track_id,
         width: 1920,
         height: 1080,
         timestamp,
         format: PixelFormat::Yuv420p,
         data: vec![0; 1920 * 1080 * 3 / 2],  // YUV420p size
         strides: Some(vec![1920, 960, 960]), // Y, U, V strides
      })
   }

   /// Extract multiple frames from a video track at the specified timestamps.
   pub async fn frames(&self, track_id: u32, timestamps: &[Duration]) -> Result<Vec<Frame>> {
      // TODO: Implement actual frame extraction
      let mut frames = Vec::new();

      for &timestamp in timestamps {
         frames.push(Frame {
            track_id,
            width: 1920,
            height: 1080,
            timestamp,
            format: PixelFormat::Yuv420p,
            data: vec![0; 1920 * 1080 * 3 / 2],  // YUV420p size
            strides: Some(vec![1920, 960, 960]), // Y, U, V strides
         });
      }

      Ok(frames)
   }

   /// List all supported format names.
   pub fn supported_formats() -> Vec<&'static str> {
      supported_formats().map(|s| s.name).collect()
   }

   /// Check if a file extension is supported.
   pub fn is_supported(extension: &str) -> bool {
      is_supported(extension)
   }
}

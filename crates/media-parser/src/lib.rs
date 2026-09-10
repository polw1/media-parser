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
//! ### Extract MP4/H.264 thumbnails
//!
//! [`ThumbnailIndex`](format::mp4::ThumbnailIndex) parses an MP4 video index
//! once and can reuse it for multiple exact-frame or keyframe requests. Input
//! timestamps are [`Duration`](std::time::Duration) values; each returned
//! [`Frame`] reports the actual presentation time of the decoded frame.
//!
//! ```no_run
//! use std::time::Duration;
//! use media_parser::{FileStreamReader, format::mp4::{ThumbnailIndex, ThumbnailOptions}};
//!
//! #[tokio::main]
//! async fn main() -> media_parser::Result<()> {
//!     let reader = FileStreamReader::new("video.mp4")?;
//!     let index = ThumbnailIndex::read(&reader, 0).await?;
//!     let timestamps = [Duration::ZERO, Duration::from_secs(5)];
//!     let frames = index
//!         .keyframes(&reader, &timestamps, ThumbnailOptions::default())
//!         .await?;
//!
//!     for frame in frames {
//!         println!("JPEG at {:?}: {} bytes", frame.timestamp, frame.data.len());
//!     }
//!     Ok(())
//! }
//! ```
//!
//! Thumbnail extraction supports H.264/AVC video tracks in MP4-family
//! containers. [`ThumbnailIndex::keyframes`](format::mp4::ThumbnailIndex::keyframes)
//! returns preceding keyframes; use
//! [`ThumbnailIndex::frames`](format::mp4::ThumbnailIndex::frames) for exact
//! requested frames. Output preserves aspect ratio, never upscales, and fits
//! a 320×320 bounding box by default; customize
//! [`ThumbnailOptions::size`](format::mp4::ThumbnailOptions::size) with
//! [`ThumbnailSize`](format::mp4::ThumbnailSize). For practical development
//! performance, enable optimized
//! dependencies in the consuming application's `Cargo.toml`:
//!
//! ```toml
//! [profile.dev.package."*"]
//! opt-level = 2
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
//! | [`ThumbnailIndex`](format::mp4::ThumbnailIndex) | Reusable MP4/H.264 thumbnail index |
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

mod decoders;
pub mod errors;
pub mod format;
pub mod helpers;
pub mod stream;
pub mod types;

// Public API
pub use decoders::h264::JpegQuality;
pub use errors::{MediaParserError, Result};
pub use format::mp4::atoms::Mp4Nav;
pub use format::registry::{
   detect_format, get_format_info, is_supported, parse_cover, parse_metadata, parse_subtitles,
   parse_tracks, supported_formats,
};
pub use stream::{FileStreamReader, HttpStreamReader, StreamReader};
pub use types::{
   AudioTrackMeta, BaseTrackMeta, CoverArt, Frame, Meta, Metadata, PixelFormat, SubtitleCue,
   SubtitleTrack, SubtitleTrackMeta, TrackFilter, TrackType, UnknownTrackMeta, VideoTrackMeta,
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

   /// Extract embedded cover artwork, when present.
   pub async fn cover(&self) -> Result<Option<CoverArt>> {
      format::registry::parse_cover(&self.reader).await
   }

   /// Extract subtitle tracks from the media file.
   pub async fn subtitles(&self, filter: Option<TrackFilter>) -> Result<Vec<SubtitleTrack>> {
      format::registry::parse_subtitles(&self.reader, filter, None).await
   }

   /// Extract subtitle tracks overlapping the half-open range `[start, end)`.
   pub async fn subtitles_in_range(
      &self,
      filter: Option<TrackFilter>,
      range: (std::time::Duration, std::time::Duration),
   ) -> Result<Vec<SubtitleTrack>> {
      format::validate_subtitle_range(Some(range))?;
      format::registry::parse_subtitles(&self.reader, filter, Some(range)).await
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

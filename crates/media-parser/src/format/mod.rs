//! # Format Detection and Parsing
//!
//! This module detects file formats and dispatches to format-specific parsers.
//!
//! ## Supported Formats
//!
//! | Format | Extensions | MIME Types |
//! |--------|------------|------------|
//! | MP4    | mp4, m4a, m4v, mov | video/mp4, audio/mp4 |
//! | MP3    | mp3 | audio/mpeg |
//!
//! ## Usage
//!
//! The parser detects the format and calls the appropriate parser:
//!
//! ```no_run
//! use media_parser::{MediaParser, FileStreamReader};
//!
//! # async fn example() -> media_parser::Result<()> {
//! // Parse MP4 file - calls mp4::read_metadata
//! let mp4_reader = FileStreamReader::new("video.mp4")?;
//! let mp4_metadata = MediaParser::new(mp4_reader).metadata().await?;
//! println!("MP4 duration: {} / {} = {:.2}s",
//!     mp4_metadata.duration,
//!     mp4_metadata.timescale,
//!     mp4_metadata.duration as f64 / mp4_metadata.timescale as f64);
//!
//! // Parse MP3 file - calls mp3::read_metadata
//! let mp3_reader = FileStreamReader::new("audio.mp3")?;
//! let mp3_metadata = MediaParser::new(mp3_reader).metadata().await?;
//! println!("MP3 duration: {}ms", mp3_metadata.duration);
//!
//! // Both return the same Metadata struct
//! for meta in &mp4_metadata {
//!     println!("{}: {}", meta.name, meta.value);
//! }
//! # Ok(())
//! # }
//! ```
//!
//! ## Format Detection
//!
//! Formats are detected by byte signatures in the file header:
//!
//! ```no_run
//! use media_parser::detect_format;
//!
//! // MP4 files have "ftyp" at offset 4
//! let mp4_header = [0, 0, 0, 32, b'f', b't', b'y', b'p'];
//! let format = detect_format(&mp4_header);
//! assert!(format.is_some());
//! assert_eq!(format.unwrap().signature.name, "MP4/M4A/MOV");
//!
//! // MP3 files start with "ID3" or frame sync bytes
//! let mp3_header = [b'I', b'D', b'3', 0, 0, 0, 0, 0];
//! let format = detect_format(&mp3_header);
//! assert!(format.is_some());
//! assert_eq!(format.unwrap().signature.name, "MP3");
//! ```

pub mod mp3;
pub mod mp4;
pub mod registry;
pub mod signatures;

use crate::Result;
use crate::stream::StreamReader;
use crate::types::{Metadata, TrackType};
use std::future::Future;
use std::pin::Pin;

/// Async parser function type.
///
/// A function that reads from a stream and returns parsed metadata.
pub type AsyncParser =
   for<'a> fn(&'a dyn StreamReader) -> Pin<Box<dyn Future<Output = Result<Metadata>> + Send + 'a>>;

/// Async track parser function type.
pub type AsyncTrackParser =
   for<'a> fn(
      &'a dyn StreamReader,
   ) -> Pin<Box<dyn Future<Output = Result<Vec<TrackType>>> + Send + 'a>>;

/// Format signature for identification.
///
/// Contains the patterns used to detect a file format:
/// byte markers, file extensions, and MIME types.
#[derive(Debug, Clone)]
pub struct FormatSignature {
   /// Human-readable name
   pub name: &'static str,
   /// File extensions associated with this format
   pub extensions: &'static [&'static str],
   /// Byte markers: (offset, expected_bytes)
   pub markers: &'static [(usize, &'static [u8])],
   /// MIME types
   pub mime_types: &'static [&'static str],
}

/// Format definition combining signature and parser.
///
/// Links a format's identification data with its parsing implementation.
pub struct Format {
   pub signature: FormatSignature,
   pub parser: AsyncParser,
   pub track_parser: AsyncTrackParser,
}

impl Format {
   pub const fn new(
      signature: FormatSignature,
      parser: AsyncParser,
      track_parser: AsyncTrackParser,
   ) -> Self {
      Self {
         signature,
         parser,
         track_parser,
      }
   }

   /// Check if this format matches the given header bytes.
   pub fn matches_bytes(&self, header: &[u8]) -> bool {
      self.signature.markers.iter().any(|(offset, pattern)| {
         header.len() >= offset + pattern.len()
            && &header[*offset..*offset + pattern.len()] == *pattern
      })
   }

   /// Check if this format matches the given extension
   pub fn matches_extension(&self, ext: &str) -> bool {
      let ext_clean = ext.trim_start_matches('.');
      self
         .signature
         .extensions
         .iter()
         .any(|e| e.eq_ignore_ascii_case(ext_clean))
   }
}

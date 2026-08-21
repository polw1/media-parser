//! # Format Registry
//!
//! Registry of supported formats and their parsers.
//! New formats are added by registering them in the `FORMATS` table.
//!
//! ```text
//! ┌──────────────┬─────────────────┐
//! │   Format     │     Parser      │
//! ├──────────────┼─────────────────┤
//! │   MP4        │   mp4::parse    │
//! │   MP3        │   mp3::parse    │
//! │   MKV(todo)  │   mkv::parse    │
//! └──────────────┴─────────────────┘
//! ```

use super::{Format, FormatSignature};
use crate::Result;
use crate::errors::MediaParserError;
use crate::stream::StreamReader;
use crate::types::{CoverArt, Metadata, TrackType};
use std::sync::LazyLock;

/// Global registry of supported formats.
static FORMATS: LazyLock<Vec<&'static Format>> = LazyLock::new(|| {
   vec![
      &super::mp4::FORMAT,
      &super::mp3::FORMAT,
      // TODO! &super::mkv::FORMAT,
      // TODO! &super::webm::FORMAT,
   ]
});

/// Detects format from header bytes.
pub fn detect_format(header: &[u8]) -> Option<&'static Format> {
   FORMATS.iter().find(|f| f.matches_bytes(header)).copied()
}

/// Detects format from file extension.
pub fn detect_format_by_extension(ext: &str) -> Option<&'static Format> {
   FORMATS.iter().find(|f| f.matches_extension(ext)).copied()
}

async fn detect_format_async(reader: &dyn StreamReader) -> Result<&'static Format> {
   let mut header = [0u8; 32];
   reader.read_at(0, &mut header).await?;

   detect_format(&header).ok_or_else(|| {
      MediaParserError::InvalidFormat("Could not detect format from file header".to_string())
   })
}

/// Parses metadata by detecting format and dispatching to the appropriate parser.
pub async fn parse_metadata(reader: &dyn StreamReader) -> Result<Metadata> {
   let format = detect_format_async(reader).await?;
   (format.parser)(reader).await
}

/// Parses track metadata by detecting format and dispatching to the appropriate parser.
pub async fn parse_tracks(reader: &dyn StreamReader) -> Result<Vec<TrackType>> {
   let format = detect_format_async(reader).await?;
   (format.track_parser)(reader).await
}

/// Parses embedded cover artwork.
pub async fn parse_cover(reader: &dyn StreamReader) -> Result<Option<CoverArt>> {
   let format = detect_format_async(reader).await?;
   (format.cover_parser)(reader).await
}

/// Returns an iterator over all supported format signatures.
pub fn supported_formats() -> impl Iterator<Item = &'static FormatSignature> {
   FORMATS.iter().map(|f| &f.signature)
}

/// Checks if a format is supported by extension.
pub fn is_supported(ext: &str) -> bool {
   detect_format_by_extension(ext).is_some()
}

/// Returns format info for the given extension, if supported.
pub fn get_format_info(ext: &str) -> Option<&'static FormatSignature> {
   detect_format_by_extension(ext).map(|f| &f.signature)
}

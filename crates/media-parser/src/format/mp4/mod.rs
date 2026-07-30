//! # MP4/M4A/MOV Format Implementation
//!
//! Parser for MP4 container format and variants (M4A, M4V, MOV).
//!
//! ## Module Structure
//!
//! ```text
//! mp4/
//! ├── mod.rs          # Format registration and public API
//! ├── metadata.rs     # Duration, timescale, tags extraction
//! ├── subtitles.rs    # Subtitle track extraction (TODO)
//! ├── thumbnails.rs   # Thumbnail/poster extraction (TODO)
//! └── atoms/          # Box parsing utilities
//!     ├── types.rs    # Mp4Box enum
//!     ├── iter.rs     # Mp4BoxIter, iter_boxes
//!     ├── nav.rs      # find_box_ref, Mp4Nav trait
//!     └── moov.rs     # find_moov_box
//! ```
//!
//! ## MP4 Box Structure
//!
//! ```text
//! [ftyp] - File type and compatibility
//! [moov] - Movie metadata container
//!   ├── [mvhd] - Movie header (duration, timescale)
//!   ├── [trak] - Track container (one per track)
//!   │   ├── [tkhd] - Track header
//!   │   └── [mdia] - Media information
//!   └── [udta] - User data
//!       └── [meta] - Metadata container
//!           └── [ilst] - iTunes-style metadata tags
//! [mdat] - Media data (audio/video samples)
//! ```

pub mod atoms;
pub mod metadata;
pub mod subtitles;
mod thumbnail_io;
pub mod thumbnails;
pub mod tracks;

use crate::Result;
use crate::format::{
   AsyncCoverParser, AsyncFrameParser, AsyncFramesParser, AsyncParser, AsyncTrackParser, Format,
};
use crate::stream::StreamReader;
use crate::types::{CoverArt, Frame, Metadata, TrackType};
use std::future::Future;
use std::pin::Pin;
use std::time::Duration;

/// MP4 format signature for detection.
pub use crate::format::signatures::MP4 as SIGNATURE;

/// Parser entry point for the registry.
fn parse(reader: &dyn StreamReader) -> Pin<Box<dyn Future<Output = Result<Metadata>> + Send + '_>> {
   Box::pin(parse_mp4(reader))
}

fn parse_tracks(
   reader: &dyn StreamReader,
) -> Pin<Box<dyn Future<Output = Result<Vec<TrackType>>> + Send + '_>> {
   Box::pin(tracks::read_tracks(reader))
}

fn parse_cover(
   reader: &dyn StreamReader,
) -> Pin<Box<dyn Future<Output = Result<Option<CoverArt>>> + Send + '_>> {
   Box::pin(read_cover(reader))
}

fn parse_frame(
   reader: &dyn StreamReader,
   track_id: u32,
   timestamp: Duration,
) -> Pin<Box<dyn Future<Output = Result<Frame>> + Send + '_>> {
   Box::pin(thumbnails::read_frame(reader, track_id, timestamp))
}

fn parse_frames<'a>(
   reader: &'a dyn StreamReader,
   track_id: u32,
   timestamps: &'a [Duration],
) -> Pin<Box<dyn Future<Output = Result<Vec<Frame>>> + Send + 'a>> {
   Box::pin(thumbnails::read_frames(reader, track_id, timestamps))
}

/// MP4 format definition registered in the global table.
pub static FORMAT: Format = Format::new(
   SIGNATURE,
   parse as AsyncParser,
   parse_tracks as AsyncTrackParser,
   parse_cover as AsyncCoverParser,
   parse_frame as AsyncFrameParser,
   parse_frames as AsyncFramesParser,
);

/// Main parsing function.
async fn parse_mp4(reader: &dyn StreamReader) -> Result<Metadata> {
   metadata::read_metadata(reader).await
}

pub async fn read_cover(reader: &dyn StreamReader) -> Result<Option<CoverArt>> {
   let moov = atoms::find_and_read_moov_box(reader).await?;
   let moov_payload = atoms::parse_moov_payload(&moov)?;
   Ok(atoms::parse_cover_art(moov_payload))
}

// Re-export for direct access
pub use metadata::read_metadata;
pub use thumbnails::{ThumbnailIndex, read_frame, read_frames, read_keyframes};
pub use tracks::read_tracks;

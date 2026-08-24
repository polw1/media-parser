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
//! ├── subtitles.rs    # Bounded tx3g/wvtt/stpp/text extraction
//! ├── thumbnails.rs   # H.264 thumbnail/keyframe extraction
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
//!
//! ## Subtitle entry points
//!
//! [`SubtitleIndex`] is the canonical API for repeated MP4 range extraction:
//! build it once, then call [`SubtitleIndex::subtitles`] with different filters
//! and half-open ranges. The reader passed to each call must expose the same
//! immutable source bytes used to construct the index. [`read_subtitles`] and
//! [`read_subtitles_in_range`] are thin one-shot wrappers that rebuild the
//! index. Use [`MediaParser::subtitles_in_range`](crate::MediaParser::subtitles_in_range)
//! when format detection is desired.
//!
//! Clients making repeated range requests should retain one [`SubtitleIndex`]
//! across those requests. Returned cue times are absolute and non-rebased, so
//! callers are responsible for clamping and rebasing cues when producing a
//! different output timeline. The scalar edit offset models only one non-empty,
//! normal-rate edit-list segment; empty, multi-segment, malformed, and non-1×
//! edit lists use zero offset.

pub mod atoms;
pub mod metadata;
mod sample_io;
pub mod subtitles;
pub mod thumbnails;
pub mod tracks;

use crate::Result;
use crate::format::{AsyncCoverParser, AsyncParser, AsyncSubtitleParser, AsyncTrackParser, Format};
use crate::stream::StreamReader;
use crate::types::{CoverArt, Metadata, SubtitleTrack, TrackFilter, TrackType};
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

fn parse_subtitles(
   reader: &dyn StreamReader,
   filter: Option<TrackFilter>,
   range: Option<(Duration, Duration)>,
) -> Pin<Box<dyn Future<Output = Result<Vec<SubtitleTrack>>> + Send + '_>> {
   Box::pin(async move {
      SubtitleIndex::read(reader)
         .await?
         .subtitles(reader, filter, range)
         .await
   })
}

/// MP4 format definition registered in the global table.
pub static FORMAT: Format = Format::new(
   SIGNATURE,
   parse as AsyncParser,
   parse_tracks as AsyncTrackParser,
   parse_cover as AsyncCoverParser,
   parse_subtitles as AsyncSubtitleParser,
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
pub use crate::decoders::h264::ThumbnailSize;
pub use metadata::read_metadata;
pub use subtitles::{
   MAX_SUBTITLE_OUTPUT_BYTES, SUBTITLE_CUE_PROJECTION_BYTES,
   SUBTITLE_ENVELOPE_PROJECTED_BASE_BYTES, SUBTITLE_TRACK_PROJECTION_BYTES, SubtitleIndex,
   read_subtitles, read_subtitles_in_range,
};
pub use thumbnails::{
   MAX_THUMBNAIL_OUTPUTS, ThumbnailIndex, ThumbnailOptions, read_frame, read_frames, read_keyframes,
};
pub use tracks::read_tracks;

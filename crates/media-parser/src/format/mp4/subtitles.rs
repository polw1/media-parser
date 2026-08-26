//! # MP4 Subtitle Extraction
//!
//! Bounded extraction of subtitle tracks from MP4-family files.
//!
//! ## Supported Subtitle Formats
//!
//! - **tx3g**: 3GPP Timed Text
//! - **wvtt**: WebVTT in MP4
//! - **stpp**: Timed Text Markup Language
//! - **text**: QuickTime text
//!
//! CEA-608/708 captions carried in video samples are not decoded.
//!
//! ## Selection and timing
//!
//! Passing no [`TrackFilter`](crate::TrackFilter) returns every valid supported
//! track only when their combined work fits the aggregate request budgets.
//! [`TrackFilter::TrackId`](crate::TrackFilter::TrackId) is the narrowest
//! selector. [`TrackFilter::Language`](crate::TrackFilter::Language) may select
//! a group of tracks and matches ASCII case-insensitively. A filter with no
//! match returns an empty vector.
//! Unfiltered and language-filtered requests skip recoverably malformed or
//! unsupported tracks; explicitly selecting such a track by ID returns an
//! error. Container-wide, I/O, and aggregate-budget failures reject the
//! complete request explicitly; extraction never returns a partial track
//! prefix or silently selects fewer tracks.
//! [`TrackFilter::TrackId(0)`](crate::TrackFilter::TrackId) is an ordinary,
//! literal track ID in this core API; the Tauri command alone uses zero as a
//! request for the first valid supported subtitle track.
//!
//! Ranges are half-open: `[start, end)`. A cue is selected when its end is
//! after `start` and its start is before `end`. Returned times remain absolute
//! to the source and are neither clipped nor rebased. Only a single, non-empty,
//! normal-rate MP4 edit-list segment is modeled as a scalar presentation offset
//! before selection; empty, multi-segment, malformed, or non-1× edit lists use
//! a zero offset. [`SubtitleCue::cue_id`](crate::SubtitleCue::cue_id) is the
//! stable, one-based MP4 sample index, so the same cue keeps its ID across full
//! and ranged requests. [`SubtitleTrack::base`](crate::SubtitleTrack::base)
//! carries a raw media duration in timescale ticks, while cue times are
//! [`Duration`](std::time::Duration) values.
//! Use a range to narrow extraction when even one selected track is
//! individually too dense for the request budgets.
//!
//! ## Reusing an index
//!
//! [`SubtitleIndex`] retains compact sample tables but no sample payloads or
//! decoded cue text. Reuse it for repeated range requests over the same source:
//!
//! ```no_run
//! use std::time::Duration;
//! use media_parser::{
//!     FileStreamReader, TrackFilter,
//!     format::mp4::SubtitleIndex,
//! };
//!
//! # async fn example() -> media_parser::Result<()> {
//! let reader = FileStreamReader::new("video.mp4")?;
//! let index = SubtitleIndex::read(&reader).await?;
//! let tracks = index
//!     .subtitles(
//!         &reader,
//!         Some(TrackFilter::Language("eng".into())),
//!         Some((Duration::from_secs(30), Duration::from_secs(60))),
//!     )
//!     .await?;
//!
//! for track in tracks {
//!     for cue in track.cues {
//!         println!("{}: {}", cue.cue_id, cue.text);
//!     }
//! }
//! # Ok(())
//! # }
//! ```
//!
//! The reader used for extraction must address exactly the same immutable
//! source bytes used to build the index. Reusing an index after the source
//! changes is a caller error. [`read_subtitles`] and
//! [`read_subtitles_in_range`] are convenience functions that build a temporary
//! index on every call.
//!
//! Clients making repeated range requests should retain one index. Since
//! extraction returns absolute source times, callers must clamp and rebase cues
//! when their output uses a different timeline.
//!
//! ## Resource limits
//!
//! Index construction scans at most 1,000 `trak` boxes, accounts at most
//! 200,000 subtitle samples, and retains at most 32 MiB. Each extraction
//! request selects at most 200,000 samples and cues, accepts at most 1 MiB per
//! sample, reads at most 64 MiB of logical sample data and 96 MiB physically,
//! decodes at most 32 MiB of text, and projects at most
//! [`MAX_SUBTITLE_OUTPUT_BYTES`] (64 MiB) of output. Coalesced reads are limited
//! to 4,096 regions of at most 8 MiB each, with at most a 64 KiB gap joined into
//! a region. Budgets are aggregate across all selected tracks and use checked,
//! fallible allocation paths.
//!
//! ## Box Structure for Subtitles
//!
//! ```text
//! [moov]
//!   └── [trak] (handler_type = 'text' or 'sbtl')
//!       └── [mdia]
//!           ├── [hdlr] - Handler reference (identifies subtitle track)
//!           └── [minf]
//!               └── [stbl]
//!                   ├── [stsd] - Sample description (tx3g, wvtt, etc.)
//!                   ├── [stts] - Time-to-sample table
//!                   ├── [stsc] - Sample-to-chunk table
//!                   ├── [stsz] - Sample sizes
//!                   └── [stco/co64] - Chunk offsets
//! [mdat] - Contains actual subtitle data
//! ```

mod budget;
mod index;
mod output;
mod text;

const SUBTITLE_ENVELOPE_PREFIX_BYTES: usize = std::mem::size_of::<u32>();
const SUBTITLE_ENVELOPE_EMPTY_HEADER_BYTES: usize = br#"{"version":1,"entries":[]}"#.len();
/// Conservative projection charged before any subtitle track or cue is retained.
pub const SUBTITLE_ENVELOPE_PROJECTED_BASE_BYTES: usize =
   SUBTITLE_ENVELOPE_PREFIX_BYTES + SUBTITLE_ENVELOPE_EMPTY_HEADER_BYTES;

pub use index::{
   MAX_SUBTITLE_OUTPUT_BYTES, SUBTITLE_CUE_PROJECTION_BYTES, SUBTITLE_TRACK_PROJECTION_BYTES,
   SubtitleIndex, read_subtitles, read_subtitles_in_range,
};

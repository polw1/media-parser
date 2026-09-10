//! MP4 atom (box) parsing utilities.
//!
//! This module provides types and functions for navigating and parsing
//! MP4/QuickTime container format atoms (also called boxes).
//!
//! ## Module Structure
//!
//! ```text
//! atoms/
//! ├── mod.rs      # Re-exports
//! ├── read.rs     # read_box - THE SINGLE PRIMITIVE
//! ├── types.rs    # Mp4Box enum
//! ├── iter.rs     # Mp4BoxIter, iter_boxes, find_box
//! ├── nav.rs      # find_box_ref, Mp4Nav trait
//! ├── moov.rs     # find_and_read_moov_box
//! └── tags.rs     # tag_name, fourcc_to_key
//! ```

mod budget;
mod cover;
mod iter;
mod media;
mod moov;
mod nav;
mod read;
mod sample_timing;
mod samples;
mod tags;
mod types;

// Re-export public items
pub(super) use budget::RetainedBudget;
pub(super) use budget::TableParseError;
pub(super) use budget::TableResult;
pub(super) use cover::parse_cover_art;
pub use iter::{Mp4BoxIter, iter_boxes};
pub use moov::find_and_read_moov_box;
pub(super) use moov::parse_moov_payload;
pub use nav::{Mp4Nav, find_box_ref};
pub use read::{BoxRead, read_box};
pub use tags::{fourcc_to_key, tag_name};
pub use types::Mp4Box;

// Track parsing helpers are internal to the MP4 module.
pub(super) use media::{
   SampleDescriptionEntry, audio_params, fourcc_string, parse_hdlr, parse_mdhd, parse_stsd,
   parse_stsd_entries_bounded, parse_tkhd, stts_sample_count, visual_dimensions,
};
pub(super) use sample_timing::SampleTiming;
pub(super) use sample_timing::SampleTimingTable;
pub(super) use sample_timing::{
   CompositionOffset, PresentationTimeline, duration_to_ticks, parse_ctts, stts_duration_ticks,
   ticks_to_duration, track_presentation_offset,
};
pub(super) use samples::{
   SampleLocator, SampleSizes, StscEntry, nearest_sync_sample, next_sync_sample, parse_avc_config,
   parse_chunk_offsets, parse_chunk_offsets_bounded, parse_sample_sizes,
   parse_sample_sizes_bounded, parse_stsc, parse_stsc_bounded, parse_stss,
   range_uses_description_index, sample_description_index, sample_size, table_entries,
   validate_sample_tables,
};

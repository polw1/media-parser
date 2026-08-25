#[cfg(test)]
use super::SUBTITLE_ENVELOPE_PROJECTED_BASE_BYTES;
use super::budget::{IndexBudget, RequestBudget, RequestLimits};
use super::output::{output_string, track_properties};
use super::text::{DecodeSampleError, decode_sample};
use crate::errors::{MediaParserError, Result};
use crate::format::mp4::atoms::{
   Mp4Nav, SampleDescriptionEntry, SampleSizes, SampleTiming, SampleTimingTable, StscEntry,
   TableParseError, TableResult, find_and_read_moov_box, iter_boxes, parse_chunk_offsets_bounded,
   parse_hdlr, parse_mdhd, parse_moov_payload, parse_sample_sizes_bounded, parse_stsc_bounded,
   parse_stsd_entries_bounded, parse_tkhd, read_box, ticks_to_duration, track_presentation_offset,
   validate_sample_tables,
};
use crate::format::mp4::sample_io::{
   SampleReadBudget, SampleReadError, SampleReadLimits, read_samples_coalesced_classified,
};
use crate::format::validate_subtitle_range;
use crate::helpers::read_u32_be;
use crate::stream::StreamReader;
use crate::types::{BaseTrackMeta, SubtitleCue, SubtitleTrack, TrackFilter};
use std::collections::HashSet;
use std::time::Duration;

pub const MAX_SUBTITLE_OUTPUT_BYTES: usize = 64 * 1024 * 1024;
pub const SUBTITLE_TRACK_PROJECTION_BYTES: usize = 512;
pub const SUBTITLE_CUE_PROJECTION_BYTES: usize = 160;

const MAX_TRAKS: usize = 1000;
const MAX_INDEXED_SUBTITLE_SAMPLES: usize = 200_000;
const MAX_RETAINED_INDEX_BYTES: usize = 32 * 1024 * 1024;
const MAX_SELECTED_SUBTITLE_SAMPLES: usize = 200_000;
const MAX_SUBTITLE_CUES: usize = 200_000;
const MAX_DECODED_TEXT_BYTES: usize = 32 * 1024 * 1024;

const SUBTITLE_READ_LIMITS: SampleReadLimits = SampleReadLimits {
   max_samples: MAX_SELECTED_SUBTITLE_SAMPLES,
   max_sample_bytes: 1024 * 1024,
   max_logical_bytes: 64 * 1024 * 1024,
   max_physical_bytes: 96 * 1024 * 1024,
   max_regions: 4096,
   max_region_bytes: 8 * 1024 * 1024,
   max_coalesce_gap_bytes: 64 * 1024,
};

const REQUEST_LIMITS: RequestLimits = RequestLimits {
   max_samples: MAX_SELECTED_SUBTITLE_SAMPLES,
   max_cues: MAX_SUBTITLE_CUES,
   max_decoded_text_bytes: MAX_DECODED_TEXT_BYTES,
   max_output_bytes: MAX_SUBTITLE_OUTPUT_BYTES,
};

// A ceiling ordered below the one it contains would reject every request
// instead of bounding it, so the two limit sets are checked for coherence at
// compile time rather than pinned to their literals by a test.
const _: () =
   assert!(SUBTITLE_READ_LIMITS.max_sample_bytes <= SUBTITLE_READ_LIMITS.max_region_bytes);
const _: () =
   assert!(SUBTITLE_READ_LIMITS.max_region_bytes <= SUBTITLE_READ_LIMITS.max_logical_bytes);
const _: () =
   assert!(SUBTITLE_READ_LIMITS.max_logical_bytes <= SUBTITLE_READ_LIMITS.max_physical_bytes);
const _: () = assert!(SUBTITLE_READ_LIMITS.max_regions <= SUBTITLE_READ_LIMITS.max_samples);
const _: () = assert!(SUBTITLE_READ_LIMITS.max_samples == REQUEST_LIMITS.max_samples);
const _: () = assert!(REQUEST_LIMITS.max_decoded_text_bytes <= REQUEST_LIMITS.max_output_bytes);
const _: () = assert!(REQUEST_LIMITS.max_cues <= REQUEST_LIMITS.max_samples);
const _: () = assert!(REQUEST_LIMITS.max_output_bytes == MAX_SUBTITLE_OUTPUT_BYTES);

#[derive(Debug)]
pub struct SubtitleIndex {
   tracks: Vec<IndexedTrackState>,
}

/// Measured index cost, reported so tests can re-run a build at the exact
/// limits a real file needs. Production discards it.
#[cfg_attr(
   not(test),
   expect(dead_code, reason = "only the test harness reads the measured usage")
)]
struct IndexUsage {
   samples: usize,
   retained_bytes: usize,
}

#[derive(Debug)]
#[expect(
   clippy::large_enum_variant,
   reason = "inline tracks keep all retained allocation in the fallible, charged index vector"
)]
enum IndexedTrackState {
   Ready(IndexedTrack),
   Rejected { id: u32, reason: &'static str },
}

#[derive(Debug)]
struct IndexedTrack {
   id: u32,
   descriptions: Vec<SampleDescriptionEntry>,
   codec_description_index: usize,
   language: Option<String>,
   timescale: u32,
   duration: u64,
   handler: [u8; 4],
   presentation_offset: i64,
   timing: SampleTimingTable,
   sizes: SampleSizes,
   stsc: Vec<StscEntry>,
   chunk_offsets: Vec<u64>,
}

impl IndexedTrack {
   fn codec(&self) -> &str {
      &self.descriptions[self.codec_description_index].codec
   }
}

#[derive(Clone, Copy)]
struct SelectedCue {
   sample_index: u32,
   start_time: Duration,
   end_time: Duration,
}

enum SelectCuesError {
   Track(MediaParserError),
   Fatal(MediaParserError),
}

#[expect(
   clippy::large_enum_variant,
   reason = "the transient parse result moves the already-accounted track without a hidden Box allocation"
)]
enum TrackParse {
   Skip,
   Ready(IndexedTrack),
   Rejected { id: u32, reason: &'static str },
}

impl SubtitleIndex {
   pub async fn read(reader: &dyn StreamReader) -> Result<Self> {
      Self::read_with_limits(
         reader,
         MAX_TRAKS,
         MAX_INDEXED_SUBTITLE_SAMPLES,
         MAX_RETAINED_INDEX_BYTES,
      )
      .await
   }

   async fn read_with_limits(
      reader: &dyn StreamReader,
      max_traks: usize,
      max_samples: usize,
      max_retained_bytes: usize,
   ) -> Result<Self> {
      Self::build_with_limits(reader, max_traks, max_samples, max_retained_bytes)
         .await
         .map(|(index, _usage)| index)
   }

   async fn build_with_limits(
      reader: &dyn StreamReader,
      max_traks: usize,
      max_samples: usize,
      max_retained_bytes: usize,
   ) -> Result<(Self, IndexUsage)> {
      let moov = find_and_read_moov_box(reader).await?;
      let payload = parse_moov_payload(&moov)?;
      let mut budget = IndexBudget::new(max_samples, max_retained_bytes);
      let mut tracks = Vec::new();
      let chapter_track_ids = disabled_chapter_track_ids(payload, max_traks)?;

      for (fourcc, trak) in iter_boxes(payload) {
         if &fourcc != b"trak" {
            continue;
         }
         match parse_track(trak, &chapter_track_ids, &mut budget)? {
            TrackParse::Skip => {}
            TrackParse::Ready(track) => {
               push_track(&mut tracks, IndexedTrackState::Ready(track), &mut budget)?
            }
            TrackParse::Rejected { id, reason } => push_track(
               &mut tracks,
               IndexedTrackState::Rejected { id, reason },
               &mut budget,
            )?,
         }
      }
      drop(moov);
      let usage = IndexUsage {
         samples: budget.samples(),
         retained_bytes: budget.retained.used_bytes(),
      };
      Ok((Self { tracks }, usage))
   }

   pub async fn subtitles(
      &self,
      reader: &dyn StreamReader,
      filter: Option<TrackFilter>,
      range: Option<(Duration, Duration)>,
   ) -> Result<Vec<SubtitleTrack>> {
      validate_subtitle_range(range)?;
      self
         .subtitles_with_limits(reader, filter, range, REQUEST_LIMITS)
         .await
   }

   async fn subtitles_with_limits(
      &self,
      reader: &dyn StreamReader,
      filter: Option<TrackFilter>,
      range: Option<(Duration, Duration)>,
      limits: RequestLimits,
   ) -> Result<Vec<SubtitleTrack>> {
      self
         .subtitles_with_request_and_read_limits(
            reader,
            filter,
            range,
            limits,
            SUBTITLE_READ_LIMITS,
         )
         .await
   }

   async fn subtitles_with_request_and_read_limits(
      &self,
      reader: &dyn StreamReader,
      filter: Option<TrackFilter>,
      range: Option<(Duration, Duration)>,
      limits: RequestLimits,
      sample_read_limits: SampleReadLimits,
   ) -> Result<Vec<SubtitleTrack>> {
      let mut output = Vec::new();
      let mut sample_read_budget = SampleReadBudget::default();
      let mut request = RequestBudget::new(limits)?;

      for indexed in &self.tracks {
         let track = match indexed {
            IndexedTrackState::Rejected { id, reason } => {
               if matches!(filter.as_ref(), Some(TrackFilter::TrackId(wanted)) if wanted == id) {
                  return Err(MediaParserError::SubtitleError((*reason).to_owned()));
               }
               if !matches!(filter.as_ref(), Some(TrackFilter::TrackId(_))) {
                  tracing::warn!(
                     track_id = *id,
                     reason,
                     "skipping rejected MP4 subtitle track"
                  );
               }
               continue;
            }
            IndexedTrackState::Ready(track) => track,
         };
         if !track_matches(track, filter.as_ref()) {
            continue;
         }

         request.charge_projection(SUBTITLE_TRACK_PROJECTION_BYTES)?;
         let selected = match select_cues(track, range, &mut request) {
            Ok(selected) => selected,
            Err(SelectCuesError::Track(error)) => {
               handle_track_failure(filter.as_ref(), track.id, error)?;
               continue;
            }
            Err(SelectCuesError::Fatal(error)) => return Err(error),
         };
         let mut sample_indices = Vec::new();
         sample_indices
            .try_reserve_exact(selected.len())
            .map_err(|_| {
               MediaParserError::Other("MP4 subtitle sample selection allocation failed".to_owned())
            })?;
         sample_indices.extend(selected.iter().map(|cue| cue.sample_index));
         let samples = match read_samples_coalesced_classified(
            reader,
            &sample_indices,
            &track.sizes,
            &track.stsc,
            &track.chunk_offsets,
            sample_read_limits,
            &mut sample_read_budget,
         )
         .await
         {
            Ok(samples) => samples,
            Err(SampleReadError::Track(error)) => {
               handle_track_failure(filter.as_ref(), track.id, error)?;
               continue;
            }
            Err(SampleReadError::Fatal(error)) => return Err(error),
         };

         let mut cues = Vec::new();
         cues.try_reserve_exact(selected.len()).map_err(|_| {
            MediaParserError::Other("MP4 subtitle cue allocation failed".to_owned())
         })?;
         let mut track_error = None;
         for selected_cue in selected {
            let Some(data) = samples.get(&selected_cue.sample_index) else {
               track_error = Some(MediaParserError::InvalidFormat(format!(
                  "missing subtitle sample {}",
                  selected_cue.sample_index
               )));
               break;
            };
            let text = match decode_sample(track.codec(), data.as_slice()) {
               Ok(Some(text)) => text,
               Ok(None) => continue,
               Err(DecodeSampleError::Track(error)) => {
                  track_error = Some(error);
                  break;
               }
               Err(DecodeSampleError::Fatal(error)) => return Err(error),
            };
            request.charge_decoded_text(text.len())?;
            request.charge_cue(1)?;
            request.charge_projection(SUBTITLE_CUE_PROJECTION_BYTES)?;
            request.charge_projection(text.len())?;
            cues.push(SubtitleCue {
               cue_id: selected_cue.sample_index,
               start_time: selected_cue.start_time,
               end_time: selected_cue.end_time,
               text,
            });
         }
         if let Some(error) = track_error {
            handle_track_failure(filter.as_ref(), track.id, error)?;
            continue;
         }

         let properties = track_properties(track.handler, track.sizes.sample_count, cues.len())?;
         output.try_reserve(1).map_err(|_| {
            MediaParserError::Other("MP4 subtitle track allocation failed".to_owned())
         })?;
         output.push(SubtitleTrack {
            base: BaseTrackMeta {
               id: track.id,
               codec: output_string(track.codec())?,
               language: track.language.as_deref().map(output_string).transpose()?,
               timescale: track.timescale,
               duration: track.duration,
               properties,
            },
            cues,
         });
      }
      Ok(output)
   }
}

fn handle_track_failure(
   filter: Option<&TrackFilter>,
   track_id: u32,
   error: MediaParserError,
) -> Result<()> {
   if matches!(filter, Some(TrackFilter::TrackId(id)) if *id == track_id) {
      return Err(error);
   }
   tracing::warn!(track_id, error = %error, "skipping malformed MP4 subtitle track");
   Ok(())
}

pub async fn read_subtitles(
   reader: &dyn StreamReader,
   filter: Option<TrackFilter>,
) -> Result<Vec<SubtitleTrack>> {
   read_subtitles_in_range(reader, filter, None).await
}

pub async fn read_subtitles_in_range(
   reader: &dyn StreamReader,
   filter: Option<TrackFilter>,
   range: Option<(Duration, Duration)>,
) -> Result<Vec<SubtitleTrack>> {
   validate_subtitle_range(range)?;
   SubtitleIndex::read(reader)
      .await?
      .subtitles(reader, filter, range)
      .await
}

/// Why a `trak` did not become an [`IndexedTrack`].
enum TrackReject {
   /// Not a subtitle track at all. Nothing was wrong with it.
   Skip,
   /// A subtitle track this parser cannot use. Recorded against the track so an
   /// explicit selection by ID can report it while other requests skip it.
   Reason(&'static str),
   /// The request cannot continue on any track.
   Fatal(MediaParserError),
}

impl From<MediaParserError> for TrackReject {
   fn from(error: MediaParserError) -> Self {
      Self::Fatal(error)
   }
}

/// Rejects the track on malformed table data and the whole request on a budget
/// or allocation failure, sharing [`table_fatal`]'s mapping for the latter.
fn retained<T>(result: TableResult<T>) -> std::result::Result<T, TrackReject> {
   match result {
      Ok(value) => Ok(value),
      Err(TableParseError::Invalid(reason)) => Err(TrackReject::Reason(reason)),
      Err(fatal) => Err(TrackReject::Fatal(table_fatal(fatal))),
   }
}

fn disabled_chapter_track_ids(payload: &[u8], max_traks: usize) -> Result<HashSet<u32>> {
   let mut disabled_track_ids = HashSet::new();
   let mut trak_count = 0usize;
   for (fourcc, trak) in iter_boxes(payload) {
      if &fourcc != b"trak" {
         continue;
      }
      trak_count = trak_count
         .checked_add(1)
         .ok_or_else(|| MediaParserError::InvalidFormat("MP4 trak count overflow".to_owned()))?;
      if trak_count > max_traks {
         return Err(MediaParserError::InvalidFormat(format!(
            "track count exceeds limit of {max_traks}"
         )));
      }
      if let Some(header) = trak.nav(&[*b"tkhd"]).and_then(parse_tkhd)
         && !header.track_enabled
      {
         try_push_unique_track_id(&mut disabled_track_ids, header.id)?;
      }
   }

   let mut chapter_track_ids = HashSet::new();
   for (fourcc, trak) in iter_boxes(payload) {
      if &fourcc != b"trak" {
         continue;
      }
      let Some(source) = trak.nav(&[*b"tkhd"]).and_then(parse_tkhd) else {
         continue;
      };
      if !source.track_enabled {
         continue;
      }
      collect_enabled_chapter_references(
         trak,
         source.id,
         &disabled_track_ids,
         &mut chapter_track_ids,
      )?;
   }
   Ok(chapter_track_ids)
}

fn collect_enabled_chapter_references(
   trak: &[u8],
   source_track_id: u32,
   disabled_track_ids: &HashSet<u32>,
   chapter_track_ids: &mut HashSet<u32>,
) -> Result<()> {
   let mut offset = 0usize;
   while offset < trak.len() {
      let Some(child) = read_box(trak, offset) else {
         if trak
            .get(offset.saturating_add(4)..offset.saturating_add(8))
            .is_some_and(|fourcc| fourcc == b"tref")
         {
            tracing::warn!(
               track_id = source_track_id,
               "ignoring malformed MP4 tref framing"
            );
         }
         break;
      };
      offset += child.total_size;
      if &child.fourcc != b"tref" {
         continue;
      }
      collect_tref_chapter_ids(
         child.payload,
         source_track_id,
         disabled_track_ids,
         chapter_track_ids,
      )?;
   }
   Ok(())
}

fn collect_tref_chapter_ids(
   tref: &[u8],
   source_track_id: u32,
   disabled_track_ids: &HashSet<u32>,
   chapter_track_ids: &mut HashSet<u32>,
) -> Result<()> {
   let mut offset = 0usize;
   while offset < tref.len() {
      let Some(reference) = read_box(tref, offset) else {
         tracing::warn!(
            track_id = source_track_id,
            "ignoring malformed MP4 tref/chap framing"
         );
         break;
      };
      offset += reference.total_size;
      if &reference.fourcc != b"chap" {
         continue;
      }
      if !reference
         .payload
         .len()
         .is_multiple_of(std::mem::size_of::<u32>())
      {
         tracing::warn!(
            track_id = source_track_id,
            "ignoring MP4 tref/chap with malformed track ID payload"
         );
         continue;
      }
      for id in reference.payload.chunks_exact(std::mem::size_of::<u32>()) {
         let id = u32::from_be_bytes(id.try_into().expect("chunks_exact yields four bytes"));
         if id != 0 && disabled_track_ids.contains(&id) {
            try_push_unique_track_id(chapter_track_ids, id)?;
         }
      }
   }
   Ok(())
}

fn try_push_unique_track_id(ids: &mut HashSet<u32>, id: u32) -> Result<()> {
   if ids.contains(&id) {
      return Ok(());
   }
   ids.try_reserve(1).map_err(|_| {
      MediaParserError::Other("MP4 chapter track classification allocation failed".to_owned())
   })?;
   ids.insert(id);
   Ok(())
}

fn parse_track(
   trak: &[u8],
   chapter_track_ids: &HashSet<u32>,
   budget: &mut IndexBudget,
) -> Result<TrackParse> {
   let Some(tkhd) = trak.nav(&[*b"tkhd"]).and_then(parse_tkhd) else {
      tracing::warn!("skipping MP4 trak whose header is too damaged to recover its ID");
      return Ok(TrackParse::Skip);
   };
   if !tkhd.track_enabled && chapter_track_ids.contains(&tkhd.id) {
      return Ok(TrackParse::Skip);
   }
   let id = tkhd.id;
   match indexed_track(trak, id, budget) {
      Ok(track) => Ok(TrackParse::Ready(track)),
      Err(TrackReject::Skip) => Ok(TrackParse::Skip),
      Err(TrackReject::Reason(reason)) => Ok(TrackParse::Rejected { id, reason }),
      Err(TrackReject::Fatal(error)) => Err(error),
   }
}

fn indexed_track(
   trak: &[u8],
   id: u32,
   budget: &mut IndexBudget,
) -> std::result::Result<IndexedTrack, TrackReject> {
   use TrackReject::Reason;
   const BAD_REFERENCE: &str = "invalid subtitle sample description reference";

   let mdia = trak
      .nav(&[*b"mdia"])
      .ok_or(Reason("subtitle track missing mdia"))?;
   let handler = mdia
      .nav(&[*b"hdlr"])
      .and_then(parse_hdlr)
      .ok_or(Reason("subtitle track has invalid handler"))?;
   if !matches!(&handler, b"sbtl" | b"subt" | b"text" | b"clcp") {
      return Err(TrackReject::Skip);
   }
   let mdhd = mdia
      .nav(&[*b"mdhd"])
      .and_then(parse_mdhd)
      .ok_or(Reason("subtitle track has invalid media header"))?;
   if mdhd.timescale == 0 {
      return Err(Reason("subtitle track has zero timescale"));
   }
   let stbl = mdia
      .nav(&[*b"minf", *b"stbl"])
      .ok_or(Reason("subtitle track missing sample table"))?;

   let stsz = stbl
      .nav(&[*b"stsz"])
      .ok_or(Reason("subtitle track missing stsz"))?;
   let raw_sample_count =
      read_u32_be(stsz, 8).ok_or(Reason("subtitle track has malformed stsz"))?;
   budget.charge_samples(usize::try_from(raw_sample_count).map_err(|_| {
      MediaParserError::InvalidFormat("subtitle sample count is too large".to_owned())
   })?)?;

   let stsd = stbl
      .nav(&[*b"stsd"])
      .ok_or(Reason("subtitle track missing stsd"))?;
   let descriptions = retained(parse_stsd_entries_bounded(stsd, &mut budget.retained))?;
   let stts = stbl
      .nav(&[*b"stts"])
      .ok_or(Reason("subtitle track missing stts"))?;
   let timing = retained(SampleTimingTable::parse(stts, &mut budget.retained))?;
   let sizes = retained(parse_sample_sizes_bounded(stsz, &mut budget.retained))?;
   let stsc_bytes = stbl
      .nav(&[*b"stsc"])
      .ok_or(Reason("subtitle track missing stsc"))?;
   let stsc = retained(parse_stsc_bounded(stsc_bytes, &mut budget.retained))?;
   let chunk_offsets = retained(parse_chunk_offsets_bounded(stbl, &mut budget.retained))?;

   if validate_sample_tables(
      stts,
      None,
      &sizes,
      &stsc,
      &chunk_offsets,
      None,
      descriptions.len(),
   )
   .is_none()
      || timing.sample_count() != sizes.sample_count
   {
      return Err(Reason("invalid subtitle sample tables"));
   }

   let first_description = stsc
      .first()
      .and_then(|entry| {
         usize::try_from(entry.sample_description_index)
            .ok()
            .and_then(|index| index.checked_sub(1))
      })
      .ok_or(Reason(BAD_REFERENCE))?;
   // Establishing the reference codec also reports an unsupported first entry
   // more precisely than the mixed-codec check below can.
   let codec = descriptions
      .get(first_description)
      .map(|entry| entry.codec.as_str())
      .ok_or(Reason(BAD_REFERENCE))?;
   if !supported_codec(codec) {
      return Err(Reason("unsupported referenced subtitle codec"));
   }
   for entry in &stsc {
      let description = usize::try_from(entry.sample_description_index)
         .ok()
         .and_then(|index| index.checked_sub(1))
         .and_then(|index| descriptions.get(index))
         .ok_or(Reason(BAD_REFERENCE))?;
      if !supported_codec(&description.codec) || description.codec != codec {
         return Err(Reason("mixed or unsupported referenced subtitle codecs"));
      }
   }

   let language = match mdhd.language {
      Some(language) => Some(retained_language(language, &mut budget.retained)?),
      None => None,
   };
   Ok(IndexedTrack {
      id,
      descriptions,
      codec_description_index: first_description,
      language,
      timescale: mdhd.timescale,
      duration: mdhd.duration,
      handler,
      presentation_offset: track_presentation_offset(trak),
      timing,
      sizes,
      stsc,
      chunk_offsets,
   })
}

fn retained_language(
   language: [u8; 3],
   retained: &mut crate::format::mp4::atoms::RetainedBudget,
) -> Result<String> {
   retained.charge_bytes(language.len()).map_err(table_fatal)?;
   let mut value = String::new();
   value
      .try_reserve_exact(language.len())
      .map_err(|_| MediaParserError::Other("MP4 subtitle index allocation failed".to_owned()))?;
   retained
      .charge_bytes(value.capacity().saturating_sub(language.len()))
      .map_err(table_fatal)?;
   value.push_str(
      std::str::from_utf8(&language).map_err(|_| {
         MediaParserError::InvalidFormat("invalid MP4 subtitle language".to_owned())
      })?,
   );
   Ok(value)
}

fn table_fatal(error: TableParseError) -> MediaParserError {
   match error {
      TableParseError::Invalid(reason) => MediaParserError::InvalidFormat(reason.to_owned()),
      TableParseError::BudgetExceeded => {
         MediaParserError::Other("MP4 subtitle index retained budget exceeded".to_owned())
      }
      TableParseError::AllocationFailed => {
         MediaParserError::Other("MP4 subtitle index allocation failed".to_owned())
      }
   }
}

fn push_track(
   tracks: &mut Vec<IndexedTrackState>,
   track: IndexedTrackState,
   budget: &mut IndexBudget,
) -> Result<()> {
   if tracks.len() == tracks.capacity() {
      budget
         .retained
         .try_reserve_vec_exact(tracks, 1)
         .map_err(table_fatal)?;
   }
   tracks.push(track);
   Ok(())
}

fn supported_codec(codec: &str) -> bool {
   matches!(codec, "tx3g" | "wvtt" | "stpp" | "text")
}

fn track_matches(track: &IndexedTrack, filter: Option<&TrackFilter>) -> bool {
   match filter {
      None => true,
      Some(TrackFilter::TrackId(id)) => track.id == *id,
      Some(TrackFilter::Language(language)) => track
         .language
         .as_deref()
         .is_some_and(|track_language| track_language.eq_ignore_ascii_case(language)),
   }
}

fn select_cues(
   track: &IndexedTrack,
   range: Option<(Duration, Duration)>,
   request: &mut RequestBudget,
) -> std::result::Result<Vec<SelectedCue>, SelectCuesError> {
   let mut count = 0usize;
   for timing in track.timing.iter() {
      if cue_timing(track, timing, range)
         .map_err(SelectCuesError::Track)?
         .is_some()
      {
         count = count.checked_add(1).ok_or_else(|| {
            SelectCuesError::Fatal(MediaParserError::Other(
               "too many selected subtitle samples".to_owned(),
            ))
         })?;
      }
   }
   request
      .charge_selected_samples(count)
      .map_err(SelectCuesError::Fatal)?;

   let mut selected = allocate_selected_cues(count)?;
   for timing in track.timing.iter() {
      if let Some(cue) = cue_timing(track, timing, range).map_err(SelectCuesError::Track)? {
         selected.push(cue);
      }
   }
   Ok(selected)
}

fn allocate_selected_cues(
   capacity: usize,
) -> std::result::Result<Vec<SelectedCue>, SelectCuesError> {
   let mut selected = Vec::new();
   selected.try_reserve_exact(capacity).map_err(|_| {
      SelectCuesError::Fatal(MediaParserError::Other(
         "MP4 subtitle sample selection allocation failed".to_owned(),
      ))
   })?;
   Ok(selected)
}

fn cue_timing(
   track: &IndexedTrack,
   timing: SampleTiming,
   range: Option<(Duration, Duration)>,
) -> Result<Option<SelectedCue>> {
   let raw_start = i128::from(timing.start_tick) - i128::from(track.presentation_offset);
   let raw_end = raw_start
      .checked_add(i128::from(timing.duration_ticks))
      .ok_or_else(|| MediaParserError::SubtitleError("subtitle timing overflow".to_owned()))?;
   if raw_end <= 0 {
      return Ok(None);
   }
   let start_tick = u64::try_from(raw_start.max(0))
      .map_err(|_| MediaParserError::SubtitleError("subtitle timing is too large".to_owned()))?;
   let end_tick = u64::try_from(raw_end)
      .map_err(|_| MediaParserError::SubtitleError("subtitle timing is too large".to_owned()))?;
   let start_time = ticks_to_duration(start_tick, track.timescale);
   let end_time = ticks_to_duration(end_tick, track.timescale);
   if start_time >= end_time
      || range.is_some_and(|(start, end)| end_time <= start || start_time >= end)
   {
      return Ok(None);
   }
   Ok(Some(SelectedCue {
      sample_index: timing.sample_index,
      start_time,
      end_time,
   }))
}

#[cfg(test)]
mod tests {
   use super::*;
   use crate::format::mp4::atoms::RetainedBudget;
   use async_trait::async_trait;

   struct BytesReader(Vec<u8>);

   #[async_trait]
   impl StreamReader for BytesReader {
      async fn read_at(&self, offset: u64, buf: &mut [u8]) -> Result<usize> {
         let start = usize::try_from(offset).unwrap().min(self.0.len());
         let count = buf.len().min(self.0.len() - start);
         buf[..count].copy_from_slice(&self.0[start..start + count]);
         Ok(count)
      }

      async fn size(&self) -> Result<u64> {
         Ok(self.0.len() as u64)
      }
   }

   fn mp4_box(fourcc: &[u8; 4], payload: &[u8]) -> Vec<u8> {
      let size = u32::try_from(payload.len() + 8).unwrap();
      [size.to_be_bytes().as_slice(), fourcc.as_slice(), payload].concat()
   }

   fn full_box(fourcc: &[u8; 4], body: &[u8]) -> Vec<u8> {
      mp4_box(fourcc, &[&[0, 0, 0, 0], body].concat())
   }

   fn track_header(id: u32, duration: u32, enabled: bool) -> Vec<u8> {
      let mut tkhd = vec![0u8; 80];
      tkhd[8..12].copy_from_slice(&id.to_be_bytes());
      tkhd[16..20].copy_from_slice(&duration.to_be_bytes());
      mp4_box(
         b"tkhd",
         &[&[0, 0, 0, u8::from(enabled)], tkhd.as_slice()].concat(),
      )
   }

   fn index_track_with_enabled(
      id: u32,
      sample_count: u32,
      codec: &[u8; 4],
      enabled: bool,
   ) -> Vec<u8> {
      let tkhd = track_header(id, sample_count * 1000, enabled);

      let mut mdhd = vec![0u8; 20];
      mdhd[8..12].copy_from_slice(&1000u32.to_be_bytes());
      mdhd[12..16].copy_from_slice(&(sample_count * 1000).to_be_bytes());
      mdhd[16..18].copy_from_slice(&0x15c7u16.to_be_bytes());
      let mdhd = full_box(b"mdhd", &mdhd);
      let mut hdlr = vec![0u8; 8];
      hdlr[4..8].copy_from_slice(b"sbtl");
      let hdlr = full_box(b"hdlr", &hdlr);

      let entry = mp4_box(codec, &[0; 8]);
      let stsd = full_box(
         b"stsd",
         &[1u32.to_be_bytes().as_slice(), entry.as_slice()].concat(),
      );
      let stts = full_box(
         b"stts",
         &[
            1u32.to_be_bytes().as_slice(),
            sample_count.to_be_bytes().as_slice(),
            1000u32.to_be_bytes().as_slice(),
         ]
         .concat(),
      );
      let stsc = full_box(
         b"stsc",
         &[
            1u32.to_be_bytes().as_slice(),
            1u32.to_be_bytes().as_slice(),
            sample_count.to_be_bytes().as_slice(),
            1u32.to_be_bytes().as_slice(),
         ]
         .concat(),
      );
      let stsz = full_box(
         b"stsz",
         &[
            0u32.to_be_bytes().as_slice(),
            sample_count.to_be_bytes().as_slice(),
            vec![3u32.to_be_bytes(); usize::try_from(sample_count).unwrap()]
               .concat()
               .as_slice(),
         ]
         .concat(),
      );
      let stco = full_box(
         b"stco",
         &[1u32.to_be_bytes().as_slice(), 0u32.to_be_bytes().as_slice()].concat(),
      );
      let stbl = mp4_box(b"stbl", &[stsd, stts, stsc, stsz, stco].concat());
      let minf = mp4_box(b"minf", &stbl);
      let mdia = mp4_box(b"mdia", &[mdhd, hdlr, minf].concat());
      mp4_box(b"trak", &[tkhd, mdia].concat())
   }

   fn index_track(id: u32, sample_count: u32, codec: &[u8; 4]) -> Vec<u8> {
      index_track_with_enabled(id, sample_count, codec, false)
   }

   fn chapter_reference(ids: &[u32]) -> Vec<u8> {
      let payload = ids
         .iter()
         .flat_map(|id| id.to_be_bytes())
         .collect::<Vec<_>>();
      mp4_box(b"chap", &payload)
   }

   fn chapter_source_track(id: u32, enabled: bool, tref_payload: &[u8]) -> Vec<u8> {
      let mut hdlr = vec![0u8; 8];
      hdlr[4..8].copy_from_slice(b"vide");
      let mdia = mp4_box(b"mdia", &full_box(b"hdlr", &hdlr));
      let tref = mp4_box(b"tref", tref_payload);
      mp4_box(
         b"trak",
         &[track_header(id, 1000, enabled), tref, mdia].concat(),
      )
   }

   fn ready_track_ids(index: &SubtitleIndex) -> Vec<u32> {
      index
         .tracks
         .iter()
         .filter_map(|track| match track {
            IndexedTrackState::Ready(track) => Some(track.id),
            IndexedTrackState::Rejected { .. } => None,
         })
         .collect()
   }

   fn index_fixture(tracks: &[Vec<u8>]) -> BytesReader {
      BytesReader(mp4_box(b"moov", &tracks.concat()))
   }

   fn early_rejected_index_track(id: u32) -> Vec<u8> {
      let mut track = index_track(id, 1, b"tx3g");
      let stsz = track
         .windows(4)
         .position(|window| window == b"stsz")
         .expect("test stsz");
      track[stsz..stsz + 4].copy_from_slice(b"junk");
      track
   }

   fn independently_measured_retained_bytes(index: &SubtitleIndex) -> usize {
      let mut bytes = index.tracks.capacity() * std::mem::size_of::<IndexedTrackState>();
      for state in &index.tracks {
         let IndexedTrackState::Ready(track) = state else {
            continue;
         };
         bytes += track.descriptions.capacity() * std::mem::size_of::<SampleDescriptionEntry>();
         bytes += track
            .descriptions
            .iter()
            .map(|description| description.codec.capacity())
            .sum::<usize>();
         bytes += track
            .language
            .as_ref()
            .map_or(0, |language| language.capacity());
         bytes +=
            track.timing.entries_capacity() * std::mem::size_of_val(&track.timing.entries()[..1]);
         bytes += track.sizes.sizes.capacity() * std::mem::size_of::<u32>();
         bytes += track.sizes.size_prefixes_capacity() * std::mem::size_of::<u64>();
         bytes += track.stsc.capacity() * std::mem::size_of::<StscEntry>();
         bytes += track.chunk_offsets.capacity() * std::mem::size_of::<u64>();
      }
      bytes
   }

   fn one_cue_index() -> SubtitleIndex {
      let mut stts = vec![0u8; 8];
      stts[4..8].copy_from_slice(&1u32.to_be_bytes());
      stts.extend_from_slice(&1u32.to_be_bytes());
      stts.extend_from_slice(&1000u32.to_be_bytes());
      let mut retained = RetainedBudget::new(1024);
      let timing = SampleTimingTable::parse(&stts, &mut retained).unwrap();
      SubtitleIndex {
         tracks: vec![IndexedTrackState::Ready(IndexedTrack {
            id: 1,
            descriptions: vec![SampleDescriptionEntry {
               codec: "tx3g".to_owned(),
            }],
            codec_description_index: 0,
            language: Some("eng".to_owned()),
            timescale: 1000,
            duration: 1000,
            handler: *b"sbtl",
            presentation_offset: 0,
            timing,
            sizes: SampleSizes::fixed(1, 3).unwrap(),
            stsc: vec![StscEntry {
               first_chunk: 1,
               samples_per_chunk: 1,
               sample_description_index: 1,
            }],
            chunk_offsets: vec![0],
         })],
      }
   }

   #[tokio::test]
   async fn enabled_chapter_source_excludes_only_a_disabled_target() {
      let source = chapter_source_track(10, true, &chapter_reference(&[1]));
      let target = index_track_with_enabled(1, 1, b"text", false);
      let index = SubtitleIndex::read(&index_fixture(&[source, target]))
         .await
         .unwrap();

      assert!(ready_track_ids(&index).is_empty());
   }

   #[tokio::test]
   async fn enabled_chapter_target_remains_a_subtitle_track() {
      let source = chapter_source_track(10, true, &chapter_reference(&[1]));
      let target = index_track_with_enabled(1, 1, b"text", true);
      let index = SubtitleIndex::read(&index_fixture(&[source, target]))
         .await
         .unwrap();

      assert_eq!(ready_track_ids(&index), vec![1]);
   }

   #[tokio::test]
   async fn disabled_chapter_source_does_not_classify_its_target() {
      let source = chapter_source_track(10, false, &chapter_reference(&[1]));
      let target = index_track_with_enabled(1, 1, b"text", false);
      let index = SubtitleIndex::read(&index_fixture(&[source, target]))
         .await
         .unwrap();

      assert_eq!(ready_track_ids(&index), vec![1]);
   }

   #[tokio::test]
   async fn zero_chapter_reference_id_is_ignored() {
      let source = chapter_source_track(10, true, &chapter_reference(&[0]));
      let target = index_track_with_enabled(0, 1, b"text", false);
      let index = SubtitleIndex::read(&index_fixture(&[source, target]))
         .await
         .unwrap();

      assert_eq!(ready_track_ids(&index), vec![0]);
   }

   #[tokio::test]
   async fn malformed_chapter_payload_is_ignored_without_accepting_its_prefix() {
      let mut malformed_payload = 1u32.to_be_bytes().to_vec();
      malformed_payload.push(0xff);
      let tref = [
         mp4_box(b"chap", &malformed_payload),
         chapter_reference(&[2]),
      ]
      .concat();
      let source = chapter_source_track(10, true, &tref);
      let first_target = index_track_with_enabled(1, 1, b"text", false);
      let second_target = index_track_with_enabled(2, 1, b"text", false);
      let index = SubtitleIndex::read(&index_fixture(&[source, first_target, second_target]))
         .await
         .unwrap();

      assert_eq!(ready_track_ids(&index), vec![1]);
   }

   #[tokio::test]
   async fn malformed_chapter_framing_is_ignored_and_other_valid_atoms_still_apply() {
      let mut malformed = chapter_reference(&[1]);
      let oversized = malformed.len() as u32 + 1;
      malformed[0..4].copy_from_slice(&oversized.to_be_bytes());
      let tref = [chapter_reference(&[2]), malformed].concat();
      let source = chapter_source_track(10, true, &tref);
      let first_target = index_track_with_enabled(1, 1, b"text", false);
      let second_target = index_track_with_enabled(2, 1, b"text", false);
      let index = SubtitleIndex::read(&index_fixture(&[source, first_target, second_target]))
         .await
         .unwrap();

      assert_eq!(ready_track_ids(&index), vec![1]);
   }

   #[test]
   fn cue_selection_budget_and_allocation_failures_are_fatal() {
      let index = one_cue_index();
      let IndexedTrackState::Ready(track) = &index.tracks[0] else {
         panic!("test track is ready");
      };
      let mut request = RequestBudget::new(RequestLimits {
         max_samples: 0,
         max_cues: 1,
         max_decoded_text_bytes: 16,
         max_output_bytes: SUBTITLE_ENVELOPE_PROJECTED_BASE_BYTES + 1024,
      })
      .unwrap();

      assert!(matches!(
         select_cues(track, None, &mut request),
         Err(SelectCuesError::Fatal(_))
      ));
      assert!(matches!(
         allocate_selected_cues(usize::MAX),
         Err(SelectCuesError::Fatal(_))
      ));
   }

   #[tokio::test]
   async fn request_limits_accept_exact_boundaries() {
      let index = one_cue_index();
      let tracks = index
         .subtitles_with_limits(
            &BytesReader(vec![0, 1, b'x']),
            None,
            None,
            RequestLimits {
               max_samples: 1,
               max_cues: 1,
               max_decoded_text_bytes: 1,
               max_output_bytes: SUBTITLE_ENVELOPE_PROJECTED_BASE_BYTES
                  + SUBTITLE_TRACK_PROJECTION_BYTES
                  + SUBTITLE_CUE_PROJECTION_BYTES
                  + 1,
            },
         )
         .await
         .unwrap();

      assert_eq!(tracks[0].cues[0].text, "x");
   }

   #[tokio::test]
   async fn request_limits_reject_one_over_each_boundary() {
      let index = one_cue_index();
      let reader = BytesReader(vec![0, 1, b'x']);
      let baseline = RequestLimits {
         max_samples: 1,
         max_cues: 1,
         max_decoded_text_bytes: 1,
         max_output_bytes: SUBTITLE_ENVELOPE_PROJECTED_BASE_BYTES
            + SUBTITLE_TRACK_PROJECTION_BYTES
            + SUBTITLE_CUE_PROJECTION_BYTES
            + 1,
      };
      for limits in [
         RequestLimits {
            max_samples: 0,
            ..baseline
         },
         RequestLimits {
            max_cues: 0,
            ..baseline
         },
         RequestLimits {
            max_decoded_text_bytes: 0,
            ..baseline
         },
         RequestLimits {
            max_output_bytes: baseline.max_output_bytes - 1,
            ..baseline
         },
      ] {
         index
            .subtitles_with_limits(&reader, None, None, limits)
            .await
            .expect_err("one-over request boundary");
      }
   }

   #[tokio::test]
   async fn empty_result_requires_the_complete_projected_envelope_base() {
      let index = SubtitleIndex { tracks: Vec::new() };
      let limits = RequestLimits {
         max_samples: 0,
         max_cues: 0,
         max_decoded_text_bytes: 0,
         max_output_bytes: SUBTITLE_ENVELOPE_PROJECTED_BASE_BYTES,
      };

      let tracks = index
         .subtitles_with_limits(&BytesReader(Vec::new()), None, None, limits)
         .await
         .expect("the exact empty-envelope base should be accepted");
      assert!(tracks.is_empty());

      let error = index
         .subtitles_with_limits(
            &BytesReader(Vec::new()),
            None,
            None,
            RequestLimits {
               max_output_bytes: SUBTITLE_ENVELOPE_PROJECTED_BASE_BYTES - 1,
               ..limits
            },
         )
         .await
         .expect_err("one byte below the empty-envelope base must fail");
      assert!(error.to_string().contains("output budget exceeded"));
   }

   #[tokio::test]
   async fn real_index_limits_accept_exact_sample_and_retained_usage() {
      let reader = index_fixture(&[index_track(1, 2, b"tx3g")]);
      let (_, usage) = SubtitleIndex::build_with_limits(&reader, MAX_TRAKS, usize::MAX, usize::MAX)
         .await
         .unwrap();
      assert_eq!(usage.samples, 2);

      SubtitleIndex::read_with_limits(&reader, MAX_TRAKS, usage.samples, usage.retained_bytes)
         .await
         .expect("exact measured index limits");
      SubtitleIndex::read_with_limits(&reader, MAX_TRAKS, usage.samples - 1, usize::MAX)
         .await
         .expect_err("one-over aggregate sample work is fatal");
      SubtitleIndex::read_with_limits(&reader, MAX_TRAKS, usize::MAX, usage.retained_bytes - 1)
         .await
         .expect_err("one-over retained allocation is fatal");
   }

   #[tokio::test]
   async fn reported_retained_usage_matches_independent_live_capacity_sum() {
      let reader = index_fixture(&[early_rejected_index_track(1), index_track(2, 2, b"tx3g")]);
      let (index, usage) =
         SubtitleIndex::build_with_limits(&reader, MAX_TRAKS, usize::MAX, usize::MAX)
            .await
            .unwrap();

      assert!(matches!(
         index.tracks[0],
         IndexedTrackState::Rejected { id: 1, .. }
      ));
      assert!(matches!(index.tracks[1], IndexedTrackState::Ready(_)));
      assert_eq!(
         usage.retained_bytes,
         independently_measured_retained_bytes(&index)
      );
   }

   #[tokio::test]
   async fn rejected_track_work_remains_charged_for_following_valid_track() {
      let reader = index_fixture(&[index_track(1, 1, b"junk"), index_track(2, 1, b"tx3g")]);
      let (_, usage) = SubtitleIndex::build_with_limits(&reader, MAX_TRAKS, usize::MAX, usize::MAX)
         .await
         .unwrap();
      assert_eq!(usage.samples, 2);

      let index =
         SubtitleIndex::read_with_limits(&reader, MAX_TRAKS, usage.samples, usage.retained_bytes)
            .await
            .expect("rejected and valid track fit exact combined budget");
      let broad = index
         .subtitles(
            &reader,
            None,
            Some((Duration::from_secs(2), Duration::from_secs(3))),
         )
         .await
         .unwrap();
      assert_eq!(broad.len(), 1);
      assert_eq!(broad[0].base.id, 2);

      SubtitleIndex::read_with_limits(&reader, MAX_TRAKS, usage.samples - 1, usize::MAX)
         .await
         .expect_err("rejected sample work is not refunded");
      SubtitleIndex::read_with_limits(&reader, MAX_TRAKS, usize::MAX, usage.retained_bytes - 1)
         .await
         .expect_err("broad retained exhaustion remains fatal");
   }

   #[tokio::test]
   async fn real_request_uses_injected_subtitle_read_limits() {
      let mut limits = SUBTITLE_READ_LIMITS;
      limits.max_sample_bytes = 2;
      one_cue_index()
         .subtitles_with_request_and_read_limits(
            &BytesReader(vec![0, 1, b'x']),
            None,
            None,
            REQUEST_LIMITS,
            limits,
         )
         .await
         .expect_err("real request forwards subtitle sample limit");
   }
}

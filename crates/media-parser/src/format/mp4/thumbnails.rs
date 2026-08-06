//! MP4 thumbnail extraction from H.264 video tracks.

use super::atoms::{
   CompositionOffset, Mp4Nav, PresentationTimeline, SampleSizes, StscEntry, duration_to_ticks,
   find_and_read_moov_box, iter_boxes, nearest_sync_sample, next_sync_sample, parse_avc_config,
   parse_chunk_offsets, parse_ctts, parse_hdlr, parse_mdhd, parse_moov_payload, parse_sample_sizes,
   parse_stsc, parse_stss, parse_tkhd, range_uses_description_index, sample_description_index,
   stts_duration_ticks, table_entries, ticks_to_duration, validate_sample_tables,
};
use super::thumbnail_io::{MAX_SAMPLES_PER_THUMBNAIL_BATCH, read_samples_coalesced};
use crate::decoders::h264::{
   AvcConfig, DecodedImage, JpegQuality, OutputBudget, ThumbnailSize, decode_frames_to_jpeg,
};
use crate::errors::{MediaParserError, Result};
use crate::helpers::{read_u32_be, read_u64_be};
use crate::stream::StreamReader;
use crate::types::{Frame, PixelFormat};
use futures::stream::{self, StreamExt, TryStreamExt};
use std::collections::{BTreeMap, HashMap};
use std::time::Duration;

pub const MAX_THUMBNAIL_OUTPUTS: usize = 4_096;
const MAX_CONCURRENT_DECODES: usize = 4;

/// Encoding options for extracted thumbnails.
///
/// A struct rather than positional parameters because more knobs are expected
/// here (a target size, above all), and each one would otherwise have to be
/// threaded through five public entry points.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ThumbnailOptions {
   /// JPEG quality of the encoded frames. Defaults to [`JpegQuality::DEFAULT`].
   pub quality: JpegQuality,
   /// Maximum output dimensions. Defaults to a 320×320 bounding box.
   pub size: ThumbnailSize,
   /// Maximum total bytes represented by returned JPEG payloads. `None`
   /// preserves the unbounded behavior for direct crate callers.
   pub max_output_bytes: Option<usize>,
}

#[derive(Debug)]
struct VideoTrack {
   id: u32,
   timescale: u32,
   duration: u64,
   presentation_offset: i64,
}

#[derive(Debug)]
struct VideoSampleTables {
   stts: Vec<u8>,
   composition_offsets: Option<Vec<CompositionOffset>>,
   sizes: SampleSizes,
   stsc: Vec<StscEntry>,
   chunk_offsets: Vec<u64>,
   sync_samples: Option<Vec<u32>>,
   avc_configs: Vec<Option<AvcConfig>>,
}

/// Parsed MP4 video index that can be reused across thumbnail requests.
#[derive(Debug)]
pub struct ThumbnailIndex {
   track: VideoTrack,
   tables: VideoSampleTables,
   timeline: PresentationTimeline,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
struct Gop {
   start_sample: u32,
   end_sample: u32,
}

#[derive(Debug, Clone, Copy)]
struct ExactTarget {
   gop: Gop,
   sample_index: u32,
   presentation_tick: u64,
   output_index: usize,
}

/// One decode unit: the samples of a single GOP plus the presentation-order
/// positions to keep as JPEG outputs.
#[derive(Debug)]
struct DecodeJob {
   gop: Gop,
   avc_config: AvcConfig,
   samples: Vec<Vec<u8>>,
   output_indices: Vec<usize>,
   output_counts: Vec<usize>,
}

/// A planned decode job: the truncated GOP plus the presentation-order
/// positions of its targets within that range.
#[derive(Debug)]
struct JobPlan {
   gop: Gop,
   output_indices: Vec<usize>,
   output_counts: Vec<usize>,
}

impl ThumbnailIndex {
   /// Reads and parses the selected video track's sample index.
   pub async fn read(reader: &dyn StreamReader, track_id: u32) -> Result<Self> {
      let moov = find_and_read_moov_box(reader).await?;
      let moov_payload = parse_moov_payload(&moov)?;
      let (track, tables) = find_video_track(moov_payload, track_id)?.ok_or(
         MediaParserError::TrackNotFound(if track_id == 0 { 1 } else { track_id }),
      )?;
      let timeline = PresentationTimeline::new(
         &tables.stts,
         tables.composition_offsets.as_deref(),
         track.presentation_offset,
         tables.sizes.sample_count,
      )
      .ok_or_else(|| {
         MediaParserError::InvalidFormat("invalid video presentation timeline".to_string())
      })?;
      Ok(Self {
         track,
         tables,
         timeline,
      })
   }

   /// Extracts exact frames while reusing the parsed index.
   pub async fn frames(
      &self,
      reader: &dyn StreamReader,
      timestamps: &[Duration],
      options: ThumbnailOptions,
   ) -> Result<Vec<Frame>> {
      self.validate_timestamps(timestamps)?;
      let mut targets = timestamps
         .iter()
         .copied()
         .map(|timestamp| exact_target(&self.track, &self.tables, &self.timeline, timestamp))
         .collect::<Result<Vec<_>>>()?;
      let mut targets_by_gop = BTreeMap::<Gop, Vec<usize>>::new();
      for (index, target) in targets.iter().enumerate() {
         targets_by_gop.entry(target.gop).or_default().push(index);
      }
      validate_gop_sample_budget(&targets_by_gop, &targets)?;

      let mut plans = Vec::new();
      plans
         .try_reserve(targets_by_gop.len())
         .map_err(|_| MediaParserError::InvalidFormat("too many thumbnail GOPs".to_string()))?;
      for (gop, target_indices) in &targets_by_gop {
         plans.push(plan_gop_job(
            &self.timeline,
            *gop,
            target_indices,
            &mut targets,
         )?);
      }

      let wanted_samples = samples_for_gops(plans.iter().map(|plan| plan.gop))?;
      let mut samples = read_samples_coalesced(
         reader,
         &wanted_samples,
         &self.tables.sizes,
         &self.tables.stsc,
         &self.tables.chunk_offsets,
      )
      .await?;

      let mut jobs = Vec::new();
      jobs
         .try_reserve(plans.len())
         .map_err(|_| MediaParserError::InvalidFormat("too many thumbnail GOPs".to_string()))?;
      for plan in plans {
         let avc_config =
            avc_config_for_range(&self.tables, plan.gop.start_sample, plan.gop.end_sample)?.clone();
         let gop_len = plan
            .gop
            .end_sample
            .checked_sub(plan.gop.start_sample)
            .and_then(|count| count.checked_add(1))
            .and_then(|count| usize::try_from(count).ok())
            .ok_or_else(|| {
               MediaParserError::InvalidFormat("thumbnail GOP is too large".to_string())
            })?;
         let mut gop_samples = Vec::new();
         gop_samples.try_reserve(gop_len).map_err(|_| {
            MediaParserError::InvalidFormat("thumbnail GOP is too large".to_string())
         })?;
         for sample_index in plan.gop.start_sample..=plan.gop.end_sample {
            gop_samples.push(samples.remove(&sample_index).ok_or_else(|| {
               MediaParserError::InvalidFormat(format!("missing thumbnail sample {sample_index}"))
            })?);
         }
         jobs.push(DecodeJob {
            gop: plan.gop,
            avc_config,
            samples: gop_samples,
            output_indices: plan.output_indices,
            output_counts: plan.output_counts,
         });
      }

      let images = run_decode_jobs(
         jobs,
         options.quality,
         options.size,
         options.max_output_bytes,
      )
      .await?;
      assemble_frames(&self.track, targets, images)
   }

   /// Extracts the nearest preceding keyframe for each timestamp.
   pub async fn keyframes(
      &self,
      reader: &dyn StreamReader,
      timestamps: &[Duration],
      options: ThumbnailOptions,
   ) -> Result<Vec<Frame>> {
      self.validate_timestamps(timestamps)?;
      let targets = timestamps
         .iter()
         .copied()
         .map(|timestamp| keyframe_target(&self.track, &self.tables, &self.timeline, timestamp))
         .collect::<Result<Vec<_>>>()?;
      let mut unique_samples = targets
         .iter()
         .map(|target| target.sample_index)
         .collect::<Vec<_>>();
      unique_samples.sort_unstable();
      unique_samples.dedup();
      let mut output_count_by_sample = HashMap::new();
      for target in &targets {
         *output_count_by_sample
            .entry(target.sample_index)
            .or_insert(0) += 1usize;
      }
      let mut samples = read_samples_coalesced(
         reader,
         &unique_samples,
         &self.tables.sizes,
         &self.tables.stsc,
         &self.tables.chunk_offsets,
      )
      .await?;

      let mut jobs = Vec::new();
      jobs.try_reserve(unique_samples.len()).map_err(|_| {
         MediaParserError::InvalidFormat("too many thumbnail keyframes".to_string())
      })?;
      for sample_index in unique_samples {
         let avc_config = avc_config_for_range(&self.tables, sample_index, sample_index)?.clone();
         let sample = samples.remove(&sample_index).ok_or_else(|| {
            MediaParserError::InvalidFormat(format!("missing thumbnail sample {sample_index}"))
         })?;
         jobs.push(DecodeJob {
            gop: Gop {
               start_sample: sample_index,
               end_sample: sample_index,
            },
            avc_config,
            samples: vec![sample],
            output_indices: vec![0],
            output_counts: vec![output_count_by_sample[&sample_index]],
         });
      }

      let images = run_decode_jobs(
         jobs,
         options.quality,
         options.size,
         options.max_output_bytes,
      )
      .await?;
      assemble_frames(&self.track, targets, images)
   }

   fn validate_timestamps(&self, timestamps: &[Duration]) -> Result<()> {
      if timestamps.len() > MAX_THUMBNAIL_OUTPUTS {
         return Err(MediaParserError::InvalidFormat(format!(
            "too many thumbnail timestamps: {}",
            timestamps.len()
         )));
      }
      for timestamp in timestamps {
         if self.track.duration != 0
            && duration_to_ticks(*timestamp, self.track.timescale) >= self.track.duration
         {
            return Err(MediaParserError::InvalidFormat(format!(
               "thumbnail timestamp {timestamp:?} is outside the video track duration"
            )));
         }
      }
      Ok(())
   }
}

/// Decodes every job with bounded concurrency, returning the selected images
/// keyed by GOP and presentation-order position.
async fn run_decode_jobs(
   jobs: Vec<DecodeJob>,
   quality: JpegQuality,
   size: ThumbnailSize,
   max_output_bytes: Option<usize>,
) -> Result<HashMap<(Gop, usize), DecodedImage>> {
   let output_budget = OutputBudget::new(max_output_bytes);
   let decoded = stream::iter(jobs.into_iter().map(|job| {
      let output_budget = output_budget.clone();
      async move {
         let DecodeJob {
            gop,
            avc_config,
            samples,
            output_indices,
            output_counts,
         } = job;
         let decoded = tokio::task::spawn_blocking(move || {
            decode_frames_to_jpeg(
               &avc_config,
               &samples,
               &output_indices,
               &output_counts,
               quality,
               size,
               &output_budget,
            )
            .map(|images| (output_indices, images))
         })
         .await
         .map_err(|error| {
            MediaParserError::BlockingTask(format!("thumbnail decode task failed: {error}"))
         })?
         .map_err(|error| {
            MediaParserError::UnsupportedCodec(format!("H.264 decode failed: {error}"))
         })?;
         Ok::<_, MediaParserError>((gop, decoded))
      }
   }))
   .buffer_unordered(MAX_CONCURRENT_DECODES)
   .try_collect::<Vec<_>>()
   .await?;

   let mut images = HashMap::new();
   for (gop, (output_indices, decoded_images)) in decoded {
      for (output_index, image) in output_indices.into_iter().zip(decoded_images) {
         images.insert((gop, output_index), image);
      }
   }
   Ok(images)
}

/// Maps each target to its decoded image, preserving the request order.
///
/// Consumes `images`: JPEG payloads are megabytes each, so an image is cloned
/// only while another target still needs it and moved out on its last use.
fn assemble_frames(
   track: &VideoTrack,
   targets: Vec<ExactTarget>,
   mut images: HashMap<(Gop, usize), DecodedImage>,
) -> Result<Vec<Frame>> {
   let mut pending = HashMap::new();
   for target in &targets {
      *pending
         .entry((target.gop, target.output_index))
         .or_insert(0) += 1usize;
   }

   let mut frames = Vec::new();
   frames
      .try_reserve(targets.len())
      .map_err(|_| MediaParserError::InvalidFormat("too many thumbnail timestamps".to_string()))?;
   for target in targets {
      let key = (target.gop, target.output_index);
      let remaining = pending.get_mut(&key).map_or(0, |count| {
         *count -= 1;
         *count
      });
      let image = if remaining == 0 {
         images.remove(&key)
      } else {
         images.get(&key).cloned()
      };
      let image = image.ok_or_else(|| {
         MediaParserError::InvalidFormat(format!(
            "missing decoded thumbnail sample {}",
            target.sample_index
         ))
      })?;
      frames.push(frame_from_image(track, target.presentation_tick, image));
   }
   Ok(frames)
}

pub async fn read_frame(
   reader: &dyn StreamReader,
   track_id: u32,
   timestamp: Duration,
   options: ThumbnailOptions,
) -> Result<Frame> {
   read_frames(reader, track_id, &[timestamp], options)
      .await?
      .into_iter()
      .next()
      .ok_or_else(|| MediaParserError::InvalidFormat("no thumbnail extracted".to_string()))
}

/// Extracts multiple frames while parsing the MP4 index only once.
pub async fn read_frames(
   reader: &dyn StreamReader,
   track_id: u32,
   timestamps: &[Duration],
   options: ThumbnailOptions,
) -> Result<Vec<Frame>> {
   if timestamps.is_empty() {
      return Ok(Vec::new());
   }

   ThumbnailIndex::read(reader, track_id)
      .await?
      .frames(reader, timestamps, options)
      .await
}

/// Extracts nearest preceding keyframes while parsing the MP4 index once.
pub async fn read_keyframes(
   reader: &dyn StreamReader,
   track_id: u32,
   timestamps: &[Duration],
   options: ThumbnailOptions,
) -> Result<Vec<Frame>> {
   if timestamps.is_empty() {
      return Ok(Vec::new());
   }
   ThumbnailIndex::read(reader, track_id)
      .await?
      .keyframes(reader, timestamps, options)
      .await
}

fn find_video_track(
   moov_payload: &[u8],
   requested_track_id: u32,
) -> Result<Option<(VideoTrack, VideoSampleTables)>> {
   for (_, trak) in iter_boxes(moov_payload).filter(|(fourcc, _)| fourcc == b"trak") {
      let Some(tkhd) = trak.nav(&[*b"tkhd"]).and_then(parse_tkhd) else {
         continue;
      };
      if requested_track_id != 0 && tkhd.id != requested_track_id {
         continue;
      }

      let Some(mdia) = trak.nav(&[*b"mdia"]) else {
         continue;
      };
      let Some(handler) = mdia.nav(&[*b"hdlr"]).and_then(parse_hdlr) else {
         continue;
      };
      if &handler != b"vide" {
         continue;
      }
      let Some(mdhd) = mdia.nav(&[*b"mdhd"]).and_then(parse_mdhd) else {
         continue;
      };
      let Some(stbl) = mdia.nav(&[*b"minf", *b"stbl"]) else {
         continue;
      };
      let presentation_offset = match trak.nav(&[*b"edts", *b"elst"]) {
         Some(elst) => parse_elst_media_time(elst).ok_or_else(|| {
            MediaParserError::InvalidFormat(
               "video track uses an unsupported MP4 edit list".to_string(),
            )
         })?,
         None => 0,
      };

      let tables = parse_video_sample_tables(stbl)?;
      // Some files carry a zero mdhd duration; fall back to the duration
      // described by the sample timing table so timestamp validation and
      // clamping still work.
      let duration = if mdhd.duration == 0 {
         stts_duration_ticks(&tables.stts).unwrap_or(0)
      } else {
         mdhd.duration
      };
      return Ok(Some((
         VideoTrack {
            id: tkhd.id,
            timescale: mdhd.timescale,
            duration,
            presentation_offset,
         },
         tables,
      )));
   }
   Ok(None)
}

fn parse_video_sample_tables(stbl: &[u8]) -> Result<VideoSampleTables> {
   let stts = stbl
      .nav(&[*b"stts"])
      .ok_or_else(|| MediaParserError::InvalidFormat("video track missing stts".to_string()))?;
   let sizes = stbl
      .nav(&[*b"stsz"])
      .and_then(parse_sample_sizes)
      .ok_or_else(|| MediaParserError::InvalidFormat("video track missing stsz".to_string()))?;
   let stsc = stbl
      .nav(&[*b"stsc"])
      .and_then(parse_stsc)
      .ok_or_else(|| MediaParserError::InvalidFormat("video track missing stsc".to_string()))?;
   let chunk_offsets = parse_chunk_offsets(stbl).ok_or_else(|| {
      MediaParserError::InvalidFormat("video track missing stco/co64".to_string())
   })?;
   let sync_samples = stbl
      .nav(&[*b"stss"])
      .map(|stss| {
         parse_stss(stss)
            .filter(|samples| !samples.is_empty())
            .ok_or_else(|| MediaParserError::InvalidFormat("invalid video stss".to_string()))
      })
      .transpose()?;
   let composition_offsets = stbl
      .nav(&[*b"ctts"])
      .map(|ctts| {
         parse_ctts(ctts)
            .ok_or_else(|| MediaParserError::InvalidFormat("invalid video ctts".to_string()))
      })
      .transpose()?;
   let avc_configs = stbl
      .nav(&[*b"stsd"])
      .and_then(parse_avc_descriptions)
      .ok_or_else(|| MediaParserError::InvalidFormat("video track missing stsd".to_string()))?;
   validate_sample_tables(
      stts,
      composition_offsets.as_deref(),
      &sizes,
      &stsc,
      &chunk_offsets,
      sync_samples.as_deref(),
      avc_configs.len(),
   )
   .ok_or_else(|| {
      MediaParserError::InvalidFormat("inconsistent video sample tables".to_string())
   })?;

   Ok(VideoSampleTables {
      stts: stts.to_vec(),
      composition_offsets,
      sizes,
      stsc,
      chunk_offsets,
      sync_samples,
      avc_configs,
   })
}

fn exact_target(
   track: &VideoTrack,
   tables: &VideoSampleTables,
   timeline: &PresentationTimeline,
   timestamp: Duration,
) -> Result<ExactTarget> {
   let target_tick = duration_to_ticks(timestamp, track.timescale);
   let selection = timeline.select(target_tick).ok_or_else(|| {
      MediaParserError::InvalidFormat("could not select video sample".to_string())
   })?;
   let sync_sample = nearest_sync_sample(selection.sample_index, tables.sync_samples.as_deref());
   let end_sample = next_sync_sample(
      sync_sample,
      tables.sync_samples.as_deref(),
      tables.sizes.sample_count,
   )
   .and_then(|sample| sample.checked_sub(1))
   .unwrap_or(tables.sizes.sample_count);
   Ok(ExactTarget {
      gop: Gop {
         start_sample: sync_sample,
         end_sample,
      },
      sample_index: selection.sample_index,
      presentation_tick: selection.presentation_tick,
      // Assigned by plan_gop_job once the GOP is truncated to its targets.
      output_index: 0,
   })
}

/// Truncates a GOP at its last targeted sample and assigns each target its
/// presentation-order position within the truncated range.
///
/// Decoding is causal in decode order, so samples past the last target are
/// not needed to reconstruct any target. The presentation positions must be
/// computed over the truncated range: trailing B-frames can present before
/// earlier samples, so positions differ from the full-GOP order.
fn plan_gop_job(
   timeline: &PresentationTimeline,
   gop: Gop,
   target_indices: &[usize],
   targets: &mut [ExactTarget],
) -> Result<JobPlan> {
   let truncated = truncated_gop(gop, target_indices, targets)?;
   if gop_sample_count(truncated)? > MAX_SAMPLES_PER_THUMBNAIL_BATCH {
      return Err(too_many_thumbnail_samples());
   }
   let mut presentation_order = timeline
      .ticks_for_range(truncated.start_sample, truncated.end_sample)
      .ok_or_else(|| MediaParserError::InvalidFormat("invalid video timing tables".to_string()))?;
   presentation_order.sort_unstable_by_key(|(sample_index, tick)| (*tick, *sample_index));

   let mut output_indices = Vec::new();
   output_indices
      .try_reserve(target_indices.len())
      .map_err(|_| MediaParserError::InvalidFormat("too many thumbnail targets".to_string()))?;
   for index in target_indices {
      let target = &mut targets[*index];
      let output_index = presentation_order
         .iter()
         .position(|(sample_index, _)| *sample_index == target.sample_index)
         .ok_or_else(|| {
            MediaParserError::InvalidFormat("selected sample is outside its GOP".to_string())
         })?;
      target.gop = truncated;
      target.output_index = output_index;
      output_indices.push(output_index);
   }
   output_indices.sort_unstable();
   let mut unique_output_indices = Vec::new();
   let mut output_counts = Vec::new();
   for output_index in output_indices {
      if unique_output_indices.last() == Some(&output_index) {
         *output_counts
            .last_mut()
            .expect("an output index has a count") += 1;
      } else {
         unique_output_indices.push(output_index);
         output_counts.push(1);
      }
   }
   Ok(JobPlan {
      gop: truncated,
      output_indices: unique_output_indices,
      output_counts,
   })
}

fn truncated_gop(gop: Gop, target_indices: &[usize], targets: &[ExactTarget]) -> Result<Gop> {
   let mut end_sample = None;
   for index in target_indices {
      let sample_index = targets
         .get(*index)
         .ok_or_else(|| MediaParserError::InvalidFormat("invalid thumbnail target".to_string()))?
         .sample_index;
      end_sample = Some(end_sample.map_or(sample_index, |end: u32| end.max(sample_index)));
   }
   let end_sample = end_sample
      .ok_or_else(|| MediaParserError::InvalidFormat("thumbnail GOP has no targets".to_string()))?;
   Ok(Gop {
      start_sample: gop.start_sample,
      end_sample,
   })
}

fn gop_sample_count(gop: Gop) -> Result<usize> {
   gop.end_sample
      .checked_sub(gop.start_sample)
      .and_then(|count| count.checked_add(1))
      .and_then(|count| usize::try_from(count).ok())
      .ok_or_else(too_many_thumbnail_samples)
}

fn too_many_thumbnail_samples() -> MediaParserError {
   MediaParserError::InvalidFormat("too many thumbnail samples".to_string())
}

fn validate_gop_sample_budget(
   targets_by_gop: &BTreeMap<Gop, Vec<usize>>,
   targets: &[ExactTarget],
) -> Result<()> {
   let mut total = 0usize;
   for (gop, target_indices) in targets_by_gop {
      let truncated = truncated_gop(*gop, target_indices, targets)?;
      total = total
         .checked_add(gop_sample_count(truncated)?)
         .ok_or_else(too_many_thumbnail_samples)?;
      if total > MAX_SAMPLES_PER_THUMBNAIL_BATCH {
         return Err(too_many_thumbnail_samples());
      }
   }
   Ok(())
}

fn keyframe_target(
   track: &VideoTrack,
   tables: &VideoSampleTables,
   timeline: &PresentationTimeline,
   timestamp: Duration,
) -> Result<ExactTarget> {
   let target_tick = duration_to_ticks(timestamp, track.timescale);
   let selection = timeline.select(target_tick).ok_or_else(|| {
      MediaParserError::InvalidFormat("could not select video sample".to_string())
   })?;
   let sync_sample = nearest_sync_sample(selection.sample_index, tables.sync_samples.as_deref());
   let presentation_tick = timeline
      .tick(sync_sample)
      .and_then(|tick| u64::try_from(tick).ok())
      .ok_or_else(|| MediaParserError::InvalidFormat("invalid video timing tables".to_string()))?;
   Ok(ExactTarget {
      gop: Gop {
         start_sample: sync_sample,
         end_sample: sync_sample,
      },
      sample_index: sync_sample,
      presentation_tick,
      output_index: 0,
   })
}

fn frame_from_image(track: &VideoTrack, presentation_tick: u64, image: DecodedImage) -> Frame {
   Frame {
      track_id: track.id,
      width: image.width,
      height: image.height,
      timestamp: ticks_to_duration(presentation_tick, track.timescale),
      format: PixelFormat::Jpeg,
      data: image.data,
      strides: None,
   }
}

fn samples_for_gops(gops: impl Iterator<Item = Gop>) -> Result<Vec<u32>> {
   let gops = gops.collect::<Vec<_>>();
   let sample_count = gops.iter().try_fold(0usize, |total, gop| {
      let count = gop
         .end_sample
         .checked_sub(gop.start_sample)?
         .checked_add(1)?;
      total.checked_add(usize::try_from(count).ok()?)
   });
   let sample_count = sample_count
      .filter(|count| *count <= MAX_SAMPLES_PER_THUMBNAIL_BATCH)
      .ok_or_else(|| MediaParserError::InvalidFormat("too many thumbnail samples".to_string()))?;
   let mut samples = Vec::new();
   samples
      .try_reserve(sample_count)
      .map_err(|_| MediaParserError::InvalidFormat("too many thumbnail samples".to_string()))?;
   for gop in gops {
      samples.extend(gop.start_sample..=gop.end_sample);
   }
   Ok(samples)
}

fn avc_config_for_range(
   tables: &VideoSampleTables,
   start_sample: u32,
   end_sample: u32,
) -> Result<&AvcConfig> {
   let description_index = sample_description_index(
      start_sample,
      &tables.sizes,
      &tables.stsc,
      &tables.chunk_offsets,
   )
   .ok_or_else(|| {
      MediaParserError::InvalidFormat("could not resolve sample description".to_string())
   })?;
   // Stsc runs tile the sample space contiguously, so the whole range shares
   // the description index iff the runs overlapping it do — one O(stsc) walk
   // instead of one sample lookup per sample in the range.
   if !range_uses_description_index(
      start_sample,
      end_sample,
      description_index,
      &tables.stsc,
      &tables.chunk_offsets,
   ) {
      return Err(MediaParserError::UnsupportedCodec(
         "a thumbnail GOP uses multiple sample descriptions".to_string(),
      ));
   }
   usize::try_from(description_index)
      .ok()
      .and_then(|index| index.checked_sub(1))
      .and_then(|index| tables.avc_configs.get(index))
      .and_then(Option::as_ref)
      .ok_or_else(|| MediaParserError::UnsupportedCodec("video track is not H.264/AVC".to_string()))
}

fn parse_avc_descriptions(stsd: &[u8]) -> Option<Vec<Option<AvcConfig>>> {
   // Each sample description is a box, so an 8-byte minimum header bounds the count.
   let entry_count = table_entries(stsd, 8)?;
   if entry_count == 0 {
      return None;
   }
   let entries = stsd.get(8..)?;
   let mut descriptions = Vec::new();
   descriptions.try_reserve(entry_count).ok()?;
   for (fourcc, payload) in iter_boxes(entries).take(entry_count) {
      descriptions.push(
         (&fourcc == b"avc1" || &fourcc == b"avc3")
            .then(|| parse_avc_config(payload))
            .flatten(),
      );
   }
   (descriptions.len() == entry_count).then_some(descriptions)
}

fn parse_elst_media_time(elst: &[u8]) -> Option<i64> {
   let version = *elst.first()?;
   let entry_size = match version {
      0 => 12usize,
      1 => 20usize,
      _ => return None,
   };
   if table_entries(elst, entry_size)? != 1 {
      return None;
   }

   let offset = 8;
   let (segment_duration, media_time, rate_offset) = if version == 0 {
      (
         u64::from(read_u32_be(elst, offset)?),
         i64::from(i32::from_be_bytes(
            read_u32_be(elst, offset + 4)?.to_be_bytes(),
         )),
         offset + 8,
      )
   } else {
      (
         read_u64_be(elst, offset)?,
         i64::from_be_bytes(read_u64_be(elst, offset + 8)?.to_be_bytes()),
         offset + 16,
      )
   };
   let media_rate = read_u32_be(elst, rate_offset)?;
   if segment_duration == 0 || media_rate != 0x0001_0000 || media_time < -1 {
      return None;
   }
   // An empty edit (media_time -1) only delays presentation; it does not shift
   // media timestamps, so it maps to no presentation offset.
   Some(media_time.max(0))
}

#[cfg(test)]
mod tests {
   use super::*;

   fn test_track() -> VideoTrack {
      VideoTrack {
         id: 1,
         timescale: 1_000,
         duration: 10_000,
         presentation_offset: 0,
      }
   }

   fn test_image(byte: u8) -> DecodedImage {
      DecodedImage {
         width: 2,
         height: 2,
         data: vec![byte; 4],
      }
   }

   fn test_target(gop_start: u32, output_index: usize, presentation_tick: u64) -> ExactTarget {
      ExactTarget {
         gop: Gop {
            start_sample: gop_start,
            end_sample: gop_start,
         },
         sample_index: gop_start,
         presentation_tick,
         output_index,
      }
   }

   fn test_timeline(sample_count: u32) -> PresentationTimeline {
      let mut stts = vec![0; 8];
      stts[4..8].copy_from_slice(&1u32.to_be_bytes());
      stts.extend_from_slice(&sample_count.to_be_bytes());
      stts.extend_from_slice(&1u32.to_be_bytes());
      PresentationTimeline::new(&stts, None, 0, sample_count).unwrap()
   }

   #[test]
   fn rejects_oversized_gop_before_materializing_its_timeline_range() {
      let timeline = test_timeline(1);
      let oversized_end = u32::try_from(MAX_SAMPLES_PER_THUMBNAIL_BATCH).unwrap() + 1;
      let mut targets = vec![ExactTarget {
         gop: Gop {
            start_sample: 1,
            end_sample: oversized_end,
         },
         sample_index: oversized_end,
         presentation_tick: 0,
         output_index: 0,
      }];

      let error = plan_gop_job(&timeline, targets[0].gop, &[0], &mut targets)
         .expect_err("an oversized GOP must be rejected before reading the timeline range");

      assert_eq!(
         error.to_string(),
         "Invalid MP4 format: too many thumbnail samples"
      );
   }

   #[test]
   fn rejects_combined_gop_ranges_before_planning_jobs() {
      let first_end = u32::try_from(MAX_SAMPLES_PER_THUMBNAIL_BATCH / 2).unwrap();
      let second_start = first_end + 1;
      let second_end = u32::try_from(MAX_SAMPLES_PER_THUMBNAIL_BATCH).unwrap() + 1;
      let targets = vec![
         ExactTarget {
            gop: Gop {
               start_sample: 1,
               end_sample: first_end,
            },
            sample_index: first_end,
            presentation_tick: 0,
            output_index: 0,
         },
         ExactTarget {
            gop: Gop {
               start_sample: second_start,
               end_sample: second_end,
            },
            sample_index: second_end,
            presentation_tick: 0,
            output_index: 0,
         },
      ];
      let targets_by_gop = BTreeMap::from([(targets[0].gop, vec![0]), (targets[1].gop, vec![1])]);

      let error = validate_gop_sample_budget(&targets_by_gop, &targets)
         .expect_err("the combined sample budget must be checked before planning jobs");

      assert_eq!(
         error.to_string(),
         "Invalid MP4 format: too many thumbnail samples"
      );
   }

   #[test]
   fn accepts_the_exact_gop_sample_budget() {
      let end_sample = u32::try_from(MAX_SAMPLES_PER_THUMBNAIL_BATCH).unwrap();
      let targets = vec![ExactTarget {
         gop: Gop {
            start_sample: 1,
            end_sample,
         },
         sample_index: end_sample,
         presentation_tick: 0,
         output_index: 0,
      }];
      let targets_by_gop = BTreeMap::from([(targets[0].gop, vec![0])]);

      validate_gop_sample_budget(&targets_by_gop, &targets).unwrap();
   }

   #[test]
   fn assembles_one_frame_per_target_in_request_order() {
      let targets = vec![test_target(5, 0, 200), test_target(0, 1, 100)];
      let images = HashMap::from([
         ((targets[0].gop, 0), test_image(0xaa)),
         ((targets[1].gop, 1), test_image(0xbb)),
      ]);

      let frames = assemble_frames(&test_track(), targets, images).unwrap();

      assert_eq!(frames.len(), 2);
      assert_eq!(frames[0].data, vec![0xaa; 4]);
      assert_eq!(frames[0].timestamp, Duration::from_millis(200));
      assert_eq!(frames[1].data, vec![0xbb; 4]);
      assert_eq!(frames[1].timestamp, Duration::from_millis(100));
   }

   #[test]
   fn shares_one_image_across_targets_that_resolve_to_the_same_frame() {
      // Distinct timestamps can land on the same decoded frame: every target
      // still gets its own payload, and only the last use moves the image.
      let targets = vec![
         test_target(0, 0, 100),
         test_target(0, 0, 140),
         test_target(0, 0, 180),
      ];
      let images = HashMap::from([((targets[0].gop, 0), test_image(0xcc))]);

      let frames = assemble_frames(&test_track(), targets, images).unwrap();

      assert_eq!(frames.len(), 3);
      assert!(frames.iter().all(|frame| frame.data == vec![0xcc; 4]));
      assert_eq!(
         frames
            .iter()
            .map(|frame| frame.timestamp)
            .collect::<Vec<_>>(),
         vec![
            Duration::from_millis(100),
            Duration::from_millis(140),
            Duration::from_millis(180),
         ]
      );
   }

   #[test]
   fn errors_when_a_target_has_no_decoded_image() {
      let targets = vec![test_target(7, 0, 100)];

      let error = assemble_frames(&test_track(), targets, HashMap::new())
         .expect_err("a target without a decoded image must not be silently dropped");

      assert!(matches!(error, MediaParserError::InvalidFormat(_)));
   }

   #[test]
   fn uses_the_stsc_selected_sample_description() {
      let stts = [0; 16];
      let tables = VideoSampleTables {
         stts: stts.to_vec(),
         composition_offsets: None,
         sizes: SampleSizes::fixed(1, 1).unwrap(),
         stsc: vec![StscEntry {
            first_chunk: 1,
            samples_per_chunk: 1,
            sample_description_index: 2,
         }],
         chunk_offsets: vec![0],
         sync_samples: None,
         avc_configs: vec![
            Some(AvcConfig {
               length_size: 4,
               sps: vec![vec![1]],
               pps: vec![vec![2]],
               color: Default::default(),
            }),
            None,
         ],
      };

      let error = avc_config_for_range(&tables, 1, 1)
         .expect_err("description 2 is not AVC and must not reuse description 1");

      assert!(matches!(error, MediaParserError::UnsupportedCodec(_)));
   }

   #[test]
   fn accepts_empty_edit_and_rejects_multi_segment_edit_lists() {
      let mut empty_edit = vec![0, 0, 0, 0];
      empty_edit.extend_from_slice(&1u32.to_be_bytes());
      empty_edit.extend_from_slice(&1_000u32.to_be_bytes());
      empty_edit.extend_from_slice(&(-1i32).to_be_bytes());
      empty_edit.extend_from_slice(&0x0001_0000u32.to_be_bytes());

      let mut multiple_edits = vec![0, 0, 0, 0];
      multiple_edits.extend_from_slice(&2u32.to_be_bytes());
      for media_time in [0i32, 1_000] {
         multiple_edits.extend_from_slice(&1_000u32.to_be_bytes());
         multiple_edits.extend_from_slice(&media_time.to_be_bytes());
         multiple_edits.extend_from_slice(&0x0001_0000u32.to_be_bytes());
      }

      assert_eq!(parse_elst_media_time(&empty_edit), Some(0));
      assert_eq!(parse_elst_media_time(&multiple_edits), None);
   }
}

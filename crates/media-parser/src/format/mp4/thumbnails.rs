//! MP4 thumbnail extraction from H.264 video tracks.

use super::atoms::{
   CompositionOffset, Mp4Nav, SampleSizes, StscEntry, duration_to_ticks, find_and_read_moov_box,
   iter_boxes, nearest_sync_sample, next_sync_sample, parse_avc_config, parse_chunk_offsets,
   parse_ctts, parse_hdlr, parse_mdhd, parse_moov_payload, parse_sample_sizes, parse_stsc,
   parse_stss, parse_tkhd, presentation_ticks_for_range, sample_description_index,
   select_sample_by_time, ticks_to_duration, validate_sample_tables,
};
use super::thumbnail_io::{MAX_SAMPLES_PER_THUMBNAIL_BATCH, read_samples_coalesced};
use crate::decoders::h264::{AvcConfig, DecodedImage, decode_frames_to_jpeg};
use crate::errors::{MediaParserError, Result};
use crate::helpers::{read_u32_be, read_u64_be};
use crate::stream::StreamReader;
use crate::types::{Frame, PixelFormat};
use futures::stream::{self, StreamExt, TryStreamExt};
use std::collections::{BTreeMap, HashMap};
use std::time::Duration;

const MAX_THUMBNAIL_OUTPUTS: usize = 4_096;
const MAX_CONCURRENT_DECODES: usize = 4;

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

#[derive(Debug, Clone, Copy)]
struct KeyframeTarget {
   sample_index: u32,
   presentation_tick: u64,
}

impl ThumbnailIndex {
   /// Reads and parses the selected video track's sample index.
   pub async fn read(reader: &dyn StreamReader, track_id: u32) -> Result<Self> {
      let moov = find_and_read_moov_box(reader).await?;
      let moov_payload = parse_moov_payload(&moov)?;
      let (track, tables) = find_video_track(moov_payload, track_id)?.ok_or(
         MediaParserError::TrackNotFound(if track_id == 0 { 1 } else { track_id }),
      )?;
      Ok(Self { track, tables })
   }

   /// Extracts exact frames while reusing the parsed index.
   pub async fn frames(
      &self,
      reader: &dyn StreamReader,
      timestamps: &[Duration],
   ) -> Result<Vec<Frame>> {
      self.validate_timestamps(timestamps)?;
      let targets = timestamps
         .iter()
         .copied()
         .map(|timestamp| exact_target(&self.track, &self.tables, timestamp))
         .collect::<Result<Vec<_>>>()?;
      let mut targets_by_gop = BTreeMap::<Gop, Vec<ExactTarget>>::new();
      for target in targets.iter().copied() {
         targets_by_gop.entry(target.gop).or_default().push(target);
      }

      let wanted_samples = samples_for_gops(targets_by_gop.keys().copied())?;
      let mut samples = read_samples_coalesced(
         reader,
         &wanted_samples,
         &self.tables.sizes,
         &self.tables.stsc,
         &self.tables.chunk_offsets,
      )
      .await?;

      let mut decode_jobs = Vec::new();
      decode_jobs
         .try_reserve(targets_by_gop.len())
         .map_err(|_| MediaParserError::InvalidFormat("too many thumbnail GOPs".to_string()))?;
      for (gop, gop_targets) in &targets_by_gop {
         let avc_config =
            avc_config_for_range(&self.tables, gop.start_sample, gop.end_sample)?.clone();
         let mut gop_samples = Vec::new();
         gop_samples
            .try_reserve(usize::try_from(gop.end_sample - gop.start_sample + 1).unwrap_or(0))
            .map_err(|_| {
               MediaParserError::InvalidFormat("thumbnail GOP is too large".to_string())
            })?;
         for sample_index in gop.start_sample..=gop.end_sample {
            gop_samples.push(samples.remove(&sample_index).ok_or_else(|| {
               MediaParserError::InvalidFormat(format!("missing thumbnail sample {sample_index}"))
            })?);
         }
         let mut output_indices = gop_targets
            .iter()
            .map(|target| target.output_index)
            .collect::<Vec<_>>();
         output_indices.sort_unstable();
         output_indices.dedup();
         decode_jobs.push((*gop, avc_config, gop_samples, output_indices));
      }

      let decoded = stream::iter(decode_jobs.into_iter().map(
         |(gop, avc_config, samples, output_indices)| async move {
            let decoded = tokio::task::spawn_blocking(move || {
               decode_frames_to_jpeg(&avc_config, &samples, &output_indices)
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
         },
      ))
      .buffer_unordered(MAX_CONCURRENT_DECODES)
      .try_collect::<Vec<_>>()
      .await?;

      let mut images = HashMap::<(Gop, usize), DecodedImage>::new();
      for (gop, (output_indices, decoded_images)) in decoded {
         for (output_index, image) in output_indices.into_iter().zip(decoded_images) {
            images.insert((gop, output_index), image);
         }
      }

      let mut frames = Vec::new();
      frames.try_reserve(targets.len()).map_err(|_| {
         MediaParserError::InvalidFormat("too many thumbnail timestamps".to_string())
      })?;
      for target in targets {
         let image = images
            .get(&(target.gop, target.output_index))
            .ok_or_else(|| {
               MediaParserError::InvalidFormat(format!(
                  "missing decoded thumbnail sample {}",
                  target.sample_index
               ))
            })?;
         frames.push(frame_from_image(
            &self.track,
            target.presentation_tick,
            image.clone(),
         ));
      }
      Ok(frames)
   }

   /// Extracts the nearest preceding keyframe for each timestamp.
   pub async fn keyframes(
      &self,
      reader: &dyn StreamReader,
      timestamps: &[Duration],
   ) -> Result<Vec<Frame>> {
      self.validate_timestamps(timestamps)?;
      let targets = timestamps
         .iter()
         .copied()
         .map(|timestamp| keyframe_target(&self.track, &self.tables, timestamp))
         .collect::<Result<Vec<_>>>()?;
      let mut unique_samples = targets
         .iter()
         .map(|target| target.sample_index)
         .collect::<Vec<_>>();
      unique_samples.sort_unstable();
      unique_samples.dedup();
      let mut samples = read_samples_coalesced(
         reader,
         &unique_samples,
         &self.tables.sizes,
         &self.tables.stsc,
         &self.tables.chunk_offsets,
      )
      .await?;

      let mut decode_jobs = Vec::new();
      decode_jobs.try_reserve(unique_samples.len()).map_err(|_| {
         MediaParserError::InvalidFormat("too many thumbnail keyframes".to_string())
      })?;
      for sample_index in unique_samples {
         let avc_config = avc_config_for_range(&self.tables, sample_index, sample_index)?.clone();
         let sample = samples.remove(&sample_index).ok_or_else(|| {
            MediaParserError::InvalidFormat(format!("missing thumbnail sample {sample_index}"))
         })?;
         decode_jobs.push((sample_index, avc_config, sample));
      }

      let decoded = stream::iter(decode_jobs.into_iter().map(
         |(sample_index, avc_config, sample)| async move {
            let image = tokio::task::spawn_blocking(move || {
               decode_frames_to_jpeg(&avc_config, &[sample], &[0])
                  .and_then(|mut images| images.pop().ok_or_else(|| "no decoded frame".to_string()))
            })
            .await
            .map_err(|error| {
               MediaParserError::BlockingTask(format!("thumbnail decode task failed: {error}"))
            })?
            .map_err(|error| {
               MediaParserError::UnsupportedCodec(format!("H.264 decode failed: {error}"))
            })?;
            Ok::<_, MediaParserError>((sample_index, image))
         },
      ))
      .buffer_unordered(MAX_CONCURRENT_DECODES)
      .try_collect::<HashMap<_, _>>()
      .await?;

      let mut frames = Vec::new();
      frames.try_reserve(targets.len()).map_err(|_| {
         MediaParserError::InvalidFormat("too many thumbnail timestamps".to_string())
      })?;
      for target in targets {
         let image = decoded.get(&target.sample_index).ok_or_else(|| {
            MediaParserError::InvalidFormat(format!(
               "missing decoded thumbnail sample {}",
               target.sample_index
            ))
         })?;
         frames.push(frame_from_image(
            &self.track,
            target.presentation_tick,
            image.clone(),
         ));
      }
      Ok(frames)
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

pub async fn read_frame(
   reader: &dyn StreamReader,
   track_id: u32,
   timestamp: Duration,
) -> Result<Frame> {
   read_frames(reader, track_id, &[timestamp])
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
) -> Result<Vec<Frame>> {
   if timestamps.is_empty() {
      return Ok(Vec::new());
   }

   ThumbnailIndex::read(reader, track_id)
      .await?
      .frames(reader, timestamps)
      .await
}

/// Extracts nearest preceding keyframes while parsing the MP4 index once.
pub async fn read_keyframes(
   reader: &dyn StreamReader,
   track_id: u32,
   timestamps: &[Duration],
) -> Result<Vec<Frame>> {
   if timestamps.is_empty() {
      return Ok(Vec::new());
   }
   ThumbnailIndex::read(reader, track_id)
      .await?
      .keyframes(reader, timestamps)
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
      return Ok(Some((
         VideoTrack {
            id: tkhd.id,
            timescale: mdhd.timescale,
            duration: mdhd.duration,
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
   timestamp: Duration,
) -> Result<ExactTarget> {
   let target_tick = duration_to_ticks(timestamp, track.timescale);
   let selection = select_sample_by_time(
      &tables.stts,
      tables.composition_offsets.as_deref(),
      track.presentation_offset,
      target_tick,
   )
   .ok_or_else(|| MediaParserError::InvalidFormat("could not select video sample".to_string()))?;
   let sync_sample = nearest_sync_sample(selection.sample_index, tables.sync_samples.as_deref());
   let end_sample = next_sync_sample(
      sync_sample,
      tables.sync_samples.as_deref(),
      tables.sizes.sample_count,
   )
   .and_then(|sample| sample.checked_sub(1))
   .unwrap_or(tables.sizes.sample_count);
   let presentation_order = presentation_ticks_for_range(
      &tables.stts,
      tables.composition_offsets.as_deref(),
      track.presentation_offset,
      sync_sample,
      end_sample,
   )
   .ok_or_else(|| MediaParserError::InvalidFormat("invalid video timing tables".to_string()))?;
   let mut presentation_order = presentation_order;
   presentation_order.sort_unstable_by_key(|(sample_index, tick)| (*tick, *sample_index));
   let output_index = presentation_order
      .iter()
      .position(|(sample_index, _)| *sample_index == selection.sample_index)
      .ok_or_else(|| {
         MediaParserError::InvalidFormat("selected sample is outside its GOP".to_string())
      })?;
   Ok(ExactTarget {
      gop: Gop {
         start_sample: sync_sample,
         end_sample,
      },
      sample_index: selection.sample_index,
      presentation_tick: selection.presentation_tick,
      output_index,
   })
}

fn keyframe_target(
   track: &VideoTrack,
   tables: &VideoSampleTables,
   timestamp: Duration,
) -> Result<KeyframeTarget> {
   let target_tick = duration_to_ticks(timestamp, track.timescale);
   let selection = select_sample_by_time(
      &tables.stts,
      tables.composition_offsets.as_deref(),
      track.presentation_offset,
      target_tick,
   )
   .ok_or_else(|| MediaParserError::InvalidFormat("could not select video sample".to_string()))?;
   let sync_sample = nearest_sync_sample(selection.sample_index, tables.sync_samples.as_deref());
   let presentation_tick = presentation_ticks_for_range(
      &tables.stts,
      tables.composition_offsets.as_deref(),
      track.presentation_offset,
      sync_sample,
      sync_sample,
   )
   .and_then(|timings| timings.into_iter().next())
   .map(|(_, tick)| u64::try_from(tick).unwrap_or(0))
   .ok_or_else(|| MediaParserError::InvalidFormat("invalid video timing tables".to_string()))?;
   Ok(KeyframeTarget {
      sample_index: sync_sample,
      presentation_tick,
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
   for sample_index in start_sample..=end_sample {
      if sample_description_index(
         sample_index,
         &tables.sizes,
         &tables.stsc,
         &tables.chunk_offsets,
      ) != Some(description_index)
      {
         return Err(MediaParserError::UnsupportedCodec(
            "a thumbnail GOP uses multiple sample descriptions".to_string(),
         ));
      }
   }
   usize::try_from(description_index)
      .ok()
      .and_then(|index| index.checked_sub(1))
      .and_then(|index| tables.avc_configs.get(index))
      .and_then(Option::as_ref)
      .ok_or_else(|| MediaParserError::UnsupportedCodec("video track is not H.264/AVC".to_string()))
}

fn parse_avc_descriptions(stsd: &[u8]) -> Option<Vec<Option<AvcConfig>>> {
   let entry_count = usize::try_from(read_u32_be(stsd, 4)?).ok()?;
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
   let entry_count = usize::try_from(read_u32_be(elst, 4)?).ok()?;
   let entry_size = match version {
      0 => 12usize,
      1 => 20usize,
      _ => return None,
   };
   if entry_count != 1 || entry_count > elst.len().checked_sub(8)? / entry_size {
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
   (segment_duration != 0 && media_time >= 0 && media_rate == 0x0001_0000).then_some(media_time)
}

#[cfg(test)]
mod tests {
   use super::*;

   #[test]
   fn uses_the_stsc_selected_sample_description() {
      let stts = [0; 16];
      let tables = VideoSampleTables {
         stts: stts.to_vec(),
         composition_offsets: None,
         sizes: SampleSizes {
            fixed_size: 1,
            sizes: Vec::new(),
            sample_count: 1,
         },
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
            }),
            None,
         ],
      };

      let error = avc_config_for_range(&tables, 1, 1)
         .expect_err("description 2 is not AVC and must not reuse description 1");

      assert!(matches!(error, MediaParserError::UnsupportedCodec(_)));
   }

   #[test]
   fn rejects_empty_and_multi_segment_edit_lists() {
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

      assert_eq!(parse_elst_media_time(&empty_edit), None);
      assert_eq!(parse_elst_media_time(&multiple_edits), None);
   }
}

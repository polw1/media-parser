//! MP4 thumbnail extraction from H.264 video tracks.

use super::atoms::{
   CompositionOffset, Mp4Nav, SampleSizes, StscEntry, duration_to_ticks, find_and_read_moov_box,
   iter_boxes, nearest_sync_sample, next_sync_sample, parse_avc_config, parse_chunk_offsets,
   parse_ctts, parse_hdlr, parse_mdhd, parse_moov_payload, parse_sample_sizes, parse_stsc,
   parse_stss, parse_tkhd, presentation_ticks_for_range, read_sample_range,
   sample_description_index, select_sample_by_time, ticks_to_duration, validate_sample_tables,
};
use crate::decoders::h264::{AvcConfig, decode_frame_to_jpeg};
use crate::errors::{MediaParserError, Result};
use crate::helpers::{read_u32_be, read_u64_be};
use crate::stream::StreamReader;
use crate::types::{Frame, PixelFormat};
use std::time::Duration;

const MAX_FRAME_BYTES: usize = 64 * 1024 * 1024;

struct VideoTrack<'a> {
   id: u32,
   timescale: u32,
   duration: u64,
   presentation_offset: i64,
   stbl: &'a [u8],
}

struct VideoSampleTables<'a> {
   stts: &'a [u8],
   composition_offsets: Option<Vec<CompositionOffset>>,
   sizes: SampleSizes,
   stsc: Vec<StscEntry>,
   chunk_offsets: Vec<u64>,
   sync_samples: Option<Vec<u32>>,
   avc_configs: Vec<Option<AvcConfig>>,
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

   let moov = find_and_read_moov_box(reader).await?;
   let moov_payload = parse_moov_payload(&moov)?;
   let track = find_video_track(moov_payload, track_id)?.ok_or(MediaParserError::TrackNotFound(
      if track_id == 0 { 1 } else { track_id },
   ))?;
   let tables = parse_video_sample_tables(track.stbl)?;

   for timestamp in timestamps {
      if track.duration != 0 && duration_to_ticks(*timestamp, track.timescale) >= track.duration {
         return Err(MediaParserError::InvalidFormat(format!(
            "thumbnail timestamp {timestamp:?} is outside the video track duration"
         )));
      }
   }

   let mut frames = Vec::new();
   frames
      .try_reserve(timestamps.len())
      .map_err(|_| MediaParserError::InvalidFormat("too many thumbnail timestamps".to_string()))?;
   for timestamp in timestamps.iter().copied() {
      frames.push(extract_frame(reader, &track, &tables, timestamp).await?);
   }
   Ok(frames)
}

fn find_video_track(
   moov_payload: &[u8],
   requested_track_id: u32,
) -> Result<Option<VideoTrack<'_>>> {
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

      return Ok(Some(VideoTrack {
         id: tkhd.id,
         timescale: mdhd.timescale,
         duration: mdhd.duration,
         presentation_offset,
         stbl,
      }));
   }
   Ok(None)
}

fn parse_video_sample_tables(stbl: &[u8]) -> Result<VideoSampleTables<'_>> {
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
      stts,
      composition_offsets,
      sizes,
      stsc,
      chunk_offsets,
      sync_samples,
      avc_configs,
   })
}

async fn extract_frame(
   reader: &dyn StreamReader,
   track: &VideoTrack<'_>,
   tables: &VideoSampleTables<'_>,
   timestamp: Duration,
) -> Result<Frame> {
   let target_tick = duration_to_ticks(timestamp, track.timescale);
   let selection = select_sample_by_time(
      tables.stts,
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
      tables.stts,
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

   let avc_config = avc_config_for_range(tables, sync_sample, end_sample)?;
   let samples = read_sample_range(
      reader,
      sync_sample,
      end_sample,
      &tables.sizes,
      &tables.stsc,
      &tables.chunk_offsets,
      MAX_FRAME_BYTES,
   )
   .await?;
   let avc_config = avc_config.clone();
   let decoded = tokio::task::spawn_blocking(move || {
      decode_frame_to_jpeg(&avc_config, &samples, output_index)
   })
   .await
   .map_err(|error| {
      MediaParserError::BlockingTask(format!("thumbnail decode task failed: {error}"))
   })?
   .map_err(|error| MediaParserError::UnsupportedCodec(format!("H.264 decode failed: {error}")))?;

   Ok(Frame {
      track_id: track.id,
      width: decoded.width,
      height: decoded.height,
      timestamp: ticks_to_duration(selection.presentation_tick, track.timescale),
      format: PixelFormat::Jpeg,
      data: decoded.data,
      strides: None,
   })
}

fn avc_config_for_range<'a>(
   tables: &'a VideoSampleTables<'_>,
   start_sample: u32,
   end_sample: u32,
) -> Result<&'a AvcConfig> {
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
         stts: &stts,
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

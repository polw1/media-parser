//! MP4 sample-table parsing and sample reads.

use super::{Mp4Nav, iter_boxes};
use crate::decoders::h264::AvcConfig;
use crate::errors::{MediaParserError, Result};
use crate::helpers::{read_u16_be, read_u32_be, read_u64_be};
use crate::stream::StreamReader;
use std::time::Duration;

#[derive(Debug, Clone, Copy)]
pub struct StscEntry {
   pub first_chunk: u32,
   pub samples_per_chunk: u32,
   pub sample_description_index: u32,
}

#[derive(Debug, Clone)]
pub struct SampleSizes {
   pub fixed_size: u32,
   pub sizes: Vec<u32>,
   pub sample_count: u32,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CompositionOffset {
   pub sample_count: u32,
   pub sample_offset: i64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SampleSelection {
   pub sample_index: u32,
   pub presentation_tick: u64,
}

const MAX_SAMPLES_PER_RANGE: usize = 16_384;

pub fn parse_avc_config(sample_entry_payload: &[u8]) -> Option<AvcConfig> {
   let children = sample_entry_payload.get(78..)?;
   let avcc =
      iter_boxes(children).find_map(|(fourcc, payload)| (&fourcc == b"avcC").then_some(payload))?;
   if avcc.len() < 7 || avcc[0] != 1 {
      return None;
   }

   let length_size = (avcc[4] & 0x03) as usize + 1;
   let sps_count = avcc[5] & 0x1f;
   let mut offset = 6usize;
   let mut sps = Vec::new();
   sps.try_reserve(sps_count as usize).ok()?;
   for _ in 0..sps_count {
      let length = read_u16_be(avcc, offset)? as usize;
      offset = offset.checked_add(2)?;
      let end = offset.checked_add(length)?;
      sps.push(avcc.get(offset..end)?.to_vec());
      offset = end;
   }

   let pps_count = *avcc.get(offset)?;
   offset = offset.checked_add(1)?;
   let mut pps = Vec::new();
   pps.try_reserve(pps_count as usize).ok()?;
   for _ in 0..pps_count {
      let length = read_u16_be(avcc, offset)? as usize;
      offset = offset.checked_add(2)?;
      let end = offset.checked_add(length)?;
      pps.push(avcc.get(offset..end)?.to_vec());
      offset = end;
   }

   Some(AvcConfig {
      length_size,
      sps,
      pps,
   })
}

pub fn parse_sample_sizes(stsz: &[u8]) -> Option<SampleSizes> {
   let fixed_size = read_u32_be(stsz, 4)?;
   let sample_count = read_u32_be(stsz, 8)?;
   let mut sizes = Vec::new();
   if fixed_size == 0 {
      let available = stsz.len().checked_sub(12)? / 4;
      let count = usize::try_from(sample_count).ok()?;
      if count > available {
         return None;
      }
      sizes.try_reserve(count).ok()?;
      for index in 0..count {
         sizes.push(read_u32_be(stsz, 12 + index * 4)?);
      }
   }

   Some(SampleSizes {
      fixed_size,
      sizes,
      sample_count,
   })
}

pub fn parse_stsc(stsc: &[u8]) -> Option<Vec<StscEntry>> {
   let entry_count = usize::try_from(read_u32_be(stsc, 4)?).ok()?;
   if entry_count > stsc.len().checked_sub(8)? / 12 {
      return None;
   }

   let mut entries = Vec::new();
   entries.try_reserve(entry_count).ok()?;
   for index in 0..entry_count {
      let offset = 8 + index * 12;
      let entry = StscEntry {
         first_chunk: read_u32_be(stsc, offset)?,
         samples_per_chunk: read_u32_be(stsc, offset + 4)?,
         sample_description_index: read_u32_be(stsc, offset + 8)?,
      };
      if entry.first_chunk == 0
         || entry.samples_per_chunk == 0
         || entry.sample_description_index == 0
      {
         return None;
      }
      if entries
         .last()
         .is_some_and(|previous: &StscEntry| previous.first_chunk >= entry.first_chunk)
      {
         return None;
      }
      entries.push(entry);
   }
   Some(entries)
}

pub fn parse_chunk_offsets(stbl: &[u8]) -> Option<Vec<u64>> {
   if let Some(stco) = stbl.nav(&[*b"stco"]) {
      let entry_count = usize::try_from(read_u32_be(stco, 4)?).ok()?;
      if entry_count > stco.len().checked_sub(8)? / 4 {
         return None;
      }
      let mut offsets = Vec::new();
      offsets.try_reserve(entry_count).ok()?;
      for index in 0..entry_count {
         offsets.push(u64::from(read_u32_be(stco, 8 + index * 4)?));
      }
      return Some(offsets);
   }

   let co64 = stbl.nav(&[*b"co64"])?;
   let entry_count = usize::try_from(read_u32_be(co64, 4)?).ok()?;
   if entry_count > co64.len().checked_sub(8)? / 8 {
      return None;
   }
   let mut offsets = Vec::new();
   offsets.try_reserve(entry_count).ok()?;
   for index in 0..entry_count {
      offsets.push(read_u64_be(co64, 8 + index * 8)?);
   }
   Some(offsets)
}

pub fn parse_stss(stss: &[u8]) -> Option<Vec<u32>> {
   let entry_count = usize::try_from(read_u32_be(stss, 4)?).ok()?;
   if entry_count > stss.len().checked_sub(8)? / 4 {
      return None;
   }
   let mut samples = Vec::new();
   samples.try_reserve(entry_count).ok()?;
   for index in 0..entry_count {
      samples.push(read_u32_be(stss, 8 + index * 4)?);
   }
   samples
      .windows(2)
      .all(|pair| pair[0] < pair[1])
      .then_some(samples)
}

pub fn parse_ctts(ctts: &[u8]) -> Option<Vec<CompositionOffset>> {
   let version = *ctts.first()?;
   if version > 1 {
      return None;
   }
   let entry_count = usize::try_from(read_u32_be(ctts, 4)?).ok()?;
   if entry_count > ctts.len().checked_sub(8)? / 8 {
      return None;
   }

   let mut offsets = Vec::new();
   offsets.try_reserve(entry_count).ok()?;
   for index in 0..entry_count {
      let offset = 8 + index * 8;
      let sample_count = read_u32_be(ctts, offset)?;
      if sample_count == 0 {
         return None;
      }
      let raw_offset = read_u32_be(ctts, offset + 4)?;
      offsets.push(CompositionOffset {
         sample_count,
         sample_offset: if version == 0 {
            i64::from(raw_offset)
         } else {
            i64::from(i32::from_be_bytes(raw_offset.to_be_bytes()))
         },
      });
   }
   Some(offsets)
}

pub fn select_sample_by_time(
   stts: &[u8],
   composition_offsets: Option<&[CompositionOffset]>,
   presentation_offset: i64,
   target_tick: u64,
) -> Option<SampleSelection> {
   let entry_count = usize::try_from(read_u32_be(stts, 4)?).ok()?;
   if entry_count > stts.len().checked_sub(8)? / 8 {
      return None;
   }

   let mut sample_index = 1u32;
   let mut decode_tick = 0u64;
   let mut stts_index = 0usize;
   let mut stts_remaining = 0u32;
   let mut sample_delta = 0u64;
   let mut ctts_index = 0usize;
   let mut ctts_remaining = 0u32;
   let mut composition_offset = 0i64;
   let mut before: Option<SampleSelection> = None;
   let mut after: Option<SampleSelection> = None;

   loop {
      if stts_remaining == 0 {
         if stts_index == entry_count {
            break;
         }
         let offset = 8 + stts_index * 8;
         stts_remaining = read_u32_be(stts, offset)?;
         sample_delta = u64::from(read_u32_be(stts, offset + 4)?);
         if stts_remaining == 0 || sample_delta == 0 {
            return None;
         }
         stts_index += 1;
      }

      if let Some(offsets) = composition_offsets {
         if ctts_remaining == 0 {
            let entry = offsets.get(ctts_index)?;
            ctts_remaining = entry.sample_count;
            composition_offset = entry.sample_offset;
            ctts_index += 1;
         }
      } else {
         ctts_remaining = stts_remaining;
         composition_offset = 0;
      }

      let segment_count = stts_remaining.min(ctts_remaining);
      let first_presentation_tick = i128::from(decode_tick)
         .checked_add(i128::from(composition_offset))?
         .checked_sub(i128::from(presentation_offset))?;
      consider_presentation_segment(
         &mut before,
         &mut after,
         sample_index,
         first_presentation_tick,
         segment_count,
         sample_delta,
         target_tick,
      )?;

      let segment_duration = u64::from(segment_count).checked_mul(sample_delta)?;
      decode_tick = decode_tick.checked_add(segment_duration)?;
      sample_index = sample_index.checked_add(segment_count)?;
      stts_remaining -= segment_count;
      ctts_remaining -= segment_count;
   }

   if composition_offsets.is_some()
      && (ctts_remaining != 0
         || composition_offsets.is_some_and(|offsets| ctts_index != offsets.len()))
   {
      return None;
   }

   before.or(after)
}

fn consider_presentation_segment(
   before: &mut Option<SampleSelection>,
   after: &mut Option<SampleSelection>,
   first_sample: u32,
   first_tick: i128,
   sample_count: u32,
   sample_delta: u64,
   target_tick: u64,
) -> Option<()> {
   let delta = i128::from(sample_delta);
   let count = i128::from(sample_count);
   let first_nonnegative = if first_tick < 0 {
      (-first_tick).checked_add(delta.checked_sub(1)?)? / delta
   } else {
      0
   };
   if first_nonnegative >= count {
      return Some(());
   }

   let first_valid_tick = first_tick.checked_add(first_nonnegative.checked_mul(delta)?)?;
   let target = i128::from(target_tick);
   if first_valid_tick <= target {
      let position = ((target - first_tick) / delta).min(count - 1);
      let presentation_tick = first_tick.checked_add(position.checked_mul(delta)?)?;
      let candidate = SampleSelection {
         sample_index: first_sample.checked_add(u32::try_from(position).ok()?)?,
         presentation_tick: u64::try_from(presentation_tick).ok()?,
      };
      if before.is_none_or(|current| {
         (candidate.presentation_tick, candidate.sample_index)
            > (current.presentation_tick, current.sample_index)
      }) {
         *before = Some(candidate);
      }
   } else {
      let candidate = SampleSelection {
         sample_index: first_sample.checked_add(u32::try_from(first_nonnegative).ok()?)?,
         presentation_tick: u64::try_from(first_valid_tick).ok()?,
      };
      if after.is_none_or(|current| {
         (candidate.presentation_tick, candidate.sample_index)
            < (current.presentation_tick, current.sample_index)
      }) {
         *after = Some(candidate);
      }
   }
   Some(())
}

pub fn nearest_sync_sample(sample_index: u32, sync_samples: Option<&[u32]>) -> u32 {
   let Some(sync_samples) = sync_samples else {
      return sample_index;
   };
   let partition = sync_samples.partition_point(|sample| *sample <= sample_index);
   partition
      .checked_sub(1)
      .and_then(|index| sync_samples.get(index))
      .copied()
      .unwrap_or(1)
}

pub fn next_sync_sample(
   sample_index: u32,
   sync_samples: Option<&[u32]>,
   sample_count: u32,
) -> Option<u32> {
   match sync_samples {
      Some(sync_samples) => {
         let partition = sync_samples.partition_point(|sample| *sample <= sample_index);
         sync_samples
            .get(partition)
            .copied()
            .filter(|sample| *sample <= sample_count)
      }
      None => sample_index
         .checked_add(1)
         .filter(|sample| *sample <= sample_count),
   }
}

pub fn sample_file_offset(
   sample_index: u32,
   sizes: &SampleSizes,
   stsc: &[StscEntry],
   chunk_offsets: &[u64],
) -> Option<u64> {
   sample_location(sample_index, sizes, stsc, chunk_offsets).map(|location| location.file_offset)
}

pub fn sample_description_index(
   sample_index: u32,
   sizes: &SampleSizes,
   stsc: &[StscEntry],
   chunk_offsets: &[u64],
) -> Option<u32> {
   sample_location(sample_index, sizes, stsc, chunk_offsets)
      .map(|location| location.sample_description_index)
}

#[derive(Debug, Clone, Copy)]
struct SampleLocation {
   file_offset: u64,
   sample_description_index: u32,
}

fn sample_location(
   sample_index: u32,
   sizes: &SampleSizes,
   stsc: &[StscEntry],
   chunk_offsets: &[u64],
) -> Option<SampleLocation> {
   if sample_index == 0
      || sample_index > sizes.sample_count
      || stsc.first()?.first_chunk != 1
      || chunk_offsets.is_empty()
   {
      return None;
   }

   let target = sample_index - 1;
   let mut first_sample_in_run = 0u32;
   let final_chunk = u32::try_from(chunk_offsets.len()).ok()?.checked_add(1)?;
   for (entry_index, entry) in stsc.iter().enumerate() {
      let next_chunk = stsc
         .get(entry_index + 1)
         .map(|next| next.first_chunk)
         .unwrap_or(final_chunk);
      if entry.first_chunk >= next_chunk || next_chunk > final_chunk {
         return None;
      }

      let chunk_count = next_chunk.checked_sub(entry.first_chunk)?;
      let samples_in_run = chunk_count.checked_mul(entry.samples_per_chunk)?;
      let run_end = first_sample_in_run.checked_add(samples_in_run)?;
      if target < run_end {
         let within_run = target.checked_sub(first_sample_in_run)?;
         let chunk_in_run = within_run / entry.samples_per_chunk;
         let within_chunk = within_run % entry.samples_per_chunk;
         let chunk_number = entry.first_chunk.checked_add(chunk_in_run)?;
         let first_sample_in_chunk =
            first_sample_in_run.checked_add(chunk_in_run.checked_mul(entry.samples_per_chunk)?)?;
         let prior_bytes = sum_sample_sizes(first_sample_in_chunk, within_chunk, sizes)?;
         let file_offset = chunk_offsets
            .get(usize::try_from(chunk_number.checked_sub(1)?).ok()?)?
            .checked_add(prior_bytes)?;
         return Some(SampleLocation {
            file_offset,
            sample_description_index: entry.sample_description_index,
         });
      }
      first_sample_in_run = run_end;
   }

   None
}

pub fn sample_size(sample_index: u32, sizes: &SampleSizes) -> Option<u32> {
   if sample_index == 0 || sample_index > sizes.sample_count {
      return None;
   }
   if sizes.fixed_size != 0 {
      Some(sizes.fixed_size)
   } else {
      sizes
         .sizes
         .get(usize::try_from(sample_index - 1).ok()?)
         .copied()
   }
}

pub async fn read_sample_data(
   reader: &dyn StreamReader,
   sample_index: u32,
   sizes: &SampleSizes,
   stsc: &[StscEntry],
   chunk_offsets: &[u64],
   max_sample_bytes: usize,
) -> Result<Vec<u8>> {
   let offset = sample_file_offset(sample_index, sizes, stsc, chunk_offsets).ok_or_else(|| {
      MediaParserError::InvalidFormat(format!("could not locate sample {sample_index}"))
   })?;
   let size = sample_size(sample_index, sizes).ok_or_else(|| {
      MediaParserError::InvalidFormat(format!("could not read sample {sample_index} size"))
   })?;
   let size = usize::try_from(size)
      .map_err(|_| MediaParserError::InvalidFormat("sample too large".to_string()))?;
   if size > max_sample_bytes {
      return Err(MediaParserError::InvalidFormat(format!(
         "sample too large: {size} bytes"
      )));
   }

   let mut data = Vec::new();
   data
      .try_reserve_exact(size)
      .map_err(|_| MediaParserError::InvalidFormat("sample too large".to_string()))?;
   data.resize(size, 0);
   let read = reader.read_at(offset, &mut data).await?;
   if read != size {
      return Err(MediaParserError::InvalidFormat(format!(
         "truncated sample {sample_index}: expected {size} bytes, read {read}"
      )));
   }
   Ok(data)
}

pub async fn read_sample_range(
   reader: &dyn StreamReader,
   start_sample: u32,
   end_sample: u32,
   sizes: &SampleSizes,
   stsc: &[StscEntry],
   chunk_offsets: &[u64],
   max_total_bytes: usize,
) -> Result<Vec<Vec<u8>>> {
   let capacity = validate_sample_range(start_sample, end_sample, max_total_bytes)?;
   if end_sample > sizes.sample_count {
      return Err(MediaParserError::InvalidFormat(format!(
         "sample range exceeds sample count: {end_sample} > {}",
         sizes.sample_count
      )));
   }

   let mut total_bytes = 0usize;
   for sample_index in start_sample..=end_sample {
      let size = usize::try_from(sample_size(sample_index, sizes).ok_or_else(|| {
         MediaParserError::InvalidFormat(format!("could not read sample {sample_index} size"))
      })?)
      .map_err(|_| MediaParserError::InvalidFormat("sample too large".to_string()))?;
      total_bytes = total_bytes
         .checked_add(size)
         .ok_or_else(|| MediaParserError::InvalidFormat("sample range too large".to_string()))?;
      if total_bytes > max_total_bytes {
         return Err(MediaParserError::InvalidFormat(format!(
            "sample range too large: {total_bytes} bytes"
         )));
      }
   }

   let allocation_bytes = capacity
      .checked_mul(std::mem::size_of::<Vec<u8>>())
      .and_then(|overhead| overhead.checked_add(total_bytes))
      .ok_or_else(|| MediaParserError::InvalidFormat("sample range too large".to_string()))?;
   if allocation_bytes > max_total_bytes {
      return Err(MediaParserError::InvalidFormat(format!(
         "sample range allocation too large: {allocation_bytes} bytes"
      )));
   }

   let mut samples = Vec::new();
   samples
      .try_reserve(capacity)
      .map_err(|_| MediaParserError::InvalidFormat("sample range too large".to_string()))?;
   for sample_index in start_sample..=end_sample {
      samples.push(
         read_sample_data(
            reader,
            sample_index,
            sizes,
            stsc,
            chunk_offsets,
            max_total_bytes,
         )
         .await?,
      );
   }
   Ok(samples)
}

pub fn validate_sample_range(
   start_sample: u32,
   end_sample: u32,
   max_total_bytes: usize,
) -> Result<usize> {
   if start_sample == 0 || end_sample < start_sample {
      return Err(MediaParserError::InvalidFormat(format!(
         "invalid sample range: {start_sample}..={end_sample}"
      )));
   }

   let capacity = usize::try_from(
      end_sample
         .checked_sub(start_sample)
         .and_then(|count| count.checked_add(1))
         .ok_or_else(|| MediaParserError::InvalidFormat("sample range too large".to_string()))?,
   )
   .map_err(|_| MediaParserError::InvalidFormat("sample range too large".to_string()))?;
   let max_by_overhead = max_total_bytes / std::mem::size_of::<Vec<u8>>();
   if capacity > MAX_SAMPLES_PER_RANGE || capacity > max_by_overhead {
      return Err(MediaParserError::InvalidFormat(format!(
         "sample range contains too many samples: {capacity}"
      )));
   }
   Ok(capacity)
}

pub fn validate_sample_tables(
   stts: &[u8],
   composition_offsets: Option<&[CompositionOffset]>,
   sizes: &SampleSizes,
   stsc: &[StscEntry],
   chunk_offsets: &[u64],
   sync_samples: Option<&[u32]>,
   sample_description_count: usize,
) -> Option<()> {
   if sizes.sample_count == 0
      || stsc.first()?.first_chunk != 1
      || chunk_offsets.is_empty()
      || sample_description_count == 0
      || stts_sample_count(stts)? != sizes.sample_count
   {
      return None;
   }

   if let Some(offsets) = composition_offsets
      && offsets
         .iter()
         .try_fold(0u32, |total, entry| total.checked_add(entry.sample_count))?
         != sizes.sample_count
   {
      return None;
   }

   let final_chunk = u32::try_from(chunk_offsets.len()).ok()?.checked_add(1)?;
   let mut described_samples = 0u32;
   for (index, entry) in stsc.iter().enumerate() {
      let next_chunk = stsc
         .get(index + 1)
         .map(|next| next.first_chunk)
         .unwrap_or(final_chunk);
      if entry.first_chunk >= next_chunk
         || next_chunk > final_chunk
         || usize::try_from(entry.sample_description_index).ok()? > sample_description_count
      {
         return None;
      }
      described_samples = described_samples.checked_add(
         next_chunk
            .checked_sub(entry.first_chunk)?
            .checked_mul(entry.samples_per_chunk)?,
      )?;
   }
   if described_samples != sizes.sample_count {
      return None;
   }

   if sync_samples.is_some_and(|samples| {
      samples.first() != Some(&1)
         || samples
            .iter()
            .any(|sample| *sample == 0 || *sample > sizes.sample_count)
   }) {
      return None;
   }
   Some(())
}

pub fn presentation_ticks_for_range(
   stts: &[u8],
   composition_offsets: Option<&[CompositionOffset]>,
   presentation_offset: i64,
   start_sample: u32,
   end_sample: u32,
) -> Option<Vec<(u32, i128)>> {
   validate_sample_range(start_sample, end_sample, usize::MAX).ok()?;
   let entry_count = usize::try_from(read_u32_be(stts, 4)?).ok()?;
   if entry_count > stts.len().checked_sub(8)? / 8 {
      return None;
   }

   let mut result = Vec::new();
   result
      .try_reserve(usize::try_from(end_sample - start_sample + 1).ok()?)
      .ok()?;
   let mut sample_index = 1u32;
   let mut decode_tick = 0u64;
   let mut stts_index = 0usize;
   let mut stts_remaining = 0u32;
   let mut sample_delta = 0u64;
   let mut ctts_index = 0usize;
   let mut ctts_remaining = 0u32;
   let mut composition_offset = 0i64;

   while sample_index <= end_sample {
      if stts_remaining == 0 {
         if stts_index == entry_count {
            return None;
         }
         let offset = 8 + stts_index * 8;
         stts_remaining = read_u32_be(stts, offset)?;
         sample_delta = u64::from(read_u32_be(stts, offset + 4)?);
         if stts_remaining == 0 || sample_delta == 0 {
            return None;
         }
         stts_index += 1;
      }
      if let Some(offsets) = composition_offsets {
         if ctts_remaining == 0 {
            let entry = offsets.get(ctts_index)?;
            ctts_remaining = entry.sample_count;
            composition_offset = entry.sample_offset;
            ctts_index += 1;
         }
      } else {
         ctts_remaining = stts_remaining;
         composition_offset = 0;
      }

      let count = stts_remaining.min(ctts_remaining);
      let segment_end = sample_index.checked_add(count)?;
      let wanted_start = start_sample.saturating_sub(sample_index).min(count);
      let wanted_end = end_sample
         .checked_add(1)?
         .saturating_sub(sample_index)
         .min(count);
      for position in wanted_start..wanted_end {
         let sample_decode_tick =
            decode_tick.checked_add(u64::from(position).checked_mul(sample_delta)?)?;
         let presentation_tick = i128::from(sample_decode_tick)
            .checked_add(i128::from(composition_offset))?
            .checked_sub(i128::from(presentation_offset))?;
         result.push((sample_index.checked_add(position)?, presentation_tick));
      }
      decode_tick = decode_tick.checked_add(u64::from(count).checked_mul(sample_delta)?)?;
      sample_index = segment_end;
      stts_remaining -= count;
      ctts_remaining -= count;
   }

   (result.len() == usize::try_from(end_sample - start_sample + 1).ok()?).then_some(result)
}

pub fn duration_to_ticks(duration: Duration, timescale: u32) -> u64 {
   let ticks = duration.as_nanos().saturating_mul(u128::from(timescale)) / 1_000_000_000;
   u64::try_from(ticks).unwrap_or(u64::MAX)
}

fn stts_sample_count(stts: &[u8]) -> Option<u32> {
   let entry_count = usize::try_from(read_u32_be(stts, 4)?).ok()?;
   if entry_count > stts.len().checked_sub(8)? / 8 {
      return None;
   }
   (0..entry_count).try_fold(0u32, |total, index| {
      total.checked_add(read_u32_be(stts, 8 + index * 8)?)
   })
}

pub fn ticks_to_duration(ticks: u64, timescale: u32) -> Duration {
   if timescale == 0 {
      return Duration::ZERO;
   }
   let nanos = u128::from(ticks).saturating_mul(1_000_000_000) / u128::from(timescale);
   Duration::from_nanos(u64::try_from(nanos).unwrap_or(u64::MAX))
}

fn sum_sample_sizes(start_sample: u32, count: u32, sizes: &SampleSizes) -> Option<u64> {
   if count == 0 {
      return Some(0);
   }
   if sizes.fixed_size != 0 {
      return u64::from(sizes.fixed_size).checked_mul(u64::from(count));
   }

   let start = usize::try_from(start_sample).ok()?;
   let end = start.checked_add(usize::try_from(count).ok()?)?;
   sizes
      .sizes
      .get(start..end)?
      .iter()
      .try_fold(0u64, |total, size| total.checked_add(u64::from(*size)))
}

#[cfg(test)]
mod tests {
   use super::*;

   #[test]
   fn selects_sample_without_expanding_stts() {
      let mut stts = vec![0; 8];
      stts[4..8].copy_from_slice(&1u32.to_be_bytes());
      stts.extend_from_slice(&4u32.to_be_bytes());
      stts.extend_from_slice(&1_000u32.to_be_bytes());

      let selection = select_sample_by_time(&stts, None, 0, 2_500).unwrap();

      assert_eq!(selection.sample_index, 3);
      assert_eq!(selection.presentation_tick, 2_000);
   }

   #[test]
   fn selects_sample_by_ctts_presentation_time() {
      let mut stts = vec![0; 8];
      stts[4..8].copy_from_slice(&1u32.to_be_bytes());
      stts.extend_from_slice(&3u32.to_be_bytes());
      stts.extend_from_slice(&1_000u32.to_be_bytes());

      let mut ctts = vec![0; 8];
      ctts[4..8].copy_from_slice(&3u32.to_be_bytes());
      for offset in [2_000u32, 3_000, 1_000] {
         ctts.extend_from_slice(&1u32.to_be_bytes());
         ctts.extend_from_slice(&offset.to_be_bytes());
      }
      let composition_offsets = parse_ctts(&ctts).unwrap();

      let selection =
         select_sample_by_time(&stts, Some(&composition_offsets), 2_000, 1_000).unwrap();

      assert_eq!(selection.sample_index, 3);
      assert_eq!(selection.presentation_tick, 1_000);
   }

   #[test]
   fn parses_signed_ctts_v1_offsets() {
      let mut ctts = vec![1, 0, 0, 0, 0, 0, 0, 1];
      ctts.extend_from_slice(&2u32.to_be_bytes());
      ctts.extend_from_slice(&(-500i32).to_be_bytes());

      assert_eq!(
         parse_ctts(&ctts).unwrap(),
         vec![CompositionOffset {
            sample_count: 2,
            sample_offset: -500,
         }]
      );
   }

   #[test]
   fn preserves_stsc_sample_description_index() {
      let mut stsc = vec![0; 8];
      stsc[4..8].copy_from_slice(&1u32.to_be_bytes());
      stsc.extend_from_slice(&1u32.to_be_bytes());
      stsc.extend_from_slice(&2u32.to_be_bytes());
      stsc.extend_from_slice(&3u32.to_be_bytes());

      let entries = parse_stsc(&stsc).unwrap();

      assert_eq!(entries[0].sample_description_index, 3);
   }

   #[test]
   fn rejects_hostile_sample_range_before_allocating() {
      assert!(validate_sample_range(1, u32::MAX, 64 * 1024 * 1024).is_err());
   }

   #[test]
   fn locates_samples_without_iterating_every_prior_chunk() {
      let sizes = SampleSizes {
         fixed_size: 4,
         sizes: Vec::new(),
         sample_count: 1_000_000_000,
      };
      let stsc = [StscEntry {
         first_chunk: 1,
         samples_per_chunk: 1_000_000_000,
         sample_description_index: 1,
      }];
      let chunk_offsets = vec![0];

      assert_eq!(
         sample_file_offset(1_000, &sizes, &stsc, &chunk_offsets),
         Some(3_996)
      );
      assert_eq!(
         sample_file_offset(1_000_000_000, &sizes, &stsc, &chunk_offsets),
         Some(3_999_999_996)
      );
   }

   #[test]
   fn rejects_stsc_entries_outside_the_chunk_table() {
      let mut stts = vec![0; 8];
      stts[4..8].copy_from_slice(&1u32.to_be_bytes());
      stts.extend_from_slice(&1u32.to_be_bytes());
      stts.extend_from_slice(&1u32.to_be_bytes());
      let sizes = SampleSizes {
         fixed_size: 1,
         sizes: Vec::new(),
         sample_count: 1,
      };
      let stsc = [
         StscEntry {
            first_chunk: 1,
            samples_per_chunk: 1,
            sample_description_index: 1,
         },
         StscEntry {
            first_chunk: u32::MAX,
            samples_per_chunk: 1,
            sample_description_index: 1,
         },
      ];

      assert!(validate_sample_tables(&stts, None, &sizes, &stsc, &[0], None, 1).is_none());
   }
}

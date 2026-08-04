//! MP4 sample-table parsing and sample reads.

use super::{Mp4Nav, iter_boxes};
use crate::decoders::h264::AvcConfig;
use crate::errors::{MediaParserError, Result};
use crate::helpers::{read_u16_be, read_u32_be, read_u64_be};
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

/// Reads the entry count of a full-box table (8-byte header of version/flags
/// plus entry count) and validates that `entry_size`-byte entries fit in the
/// box payload.
pub fn table_entries(buf: &[u8], entry_size: usize) -> Option<usize> {
   let entry_count = usize::try_from(read_u32_be(buf, 4)?).ok()?;
   (entry_count <= buf.len().checked_sub(8)? / entry_size).then_some(entry_count)
}

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
   let entry_count = table_entries(stsc, 12)?;

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
      let entry_count = table_entries(stco, 4)?;
      let mut offsets = Vec::new();
      offsets.try_reserve(entry_count).ok()?;
      for index in 0..entry_count {
         offsets.push(u64::from(read_u32_be(stco, 8 + index * 4)?));
      }
      return Some(offsets);
   }

   let co64 = stbl.nav(&[*b"co64"])?;
   let entry_count = table_entries(co64, 8)?;
   let mut offsets = Vec::new();
   offsets.try_reserve(entry_count).ok()?;
   for index in 0..entry_count {
      offsets.push(read_u64_be(co64, 8 + index * 8)?);
   }
   Some(offsets)
}

pub fn parse_stss(stss: &[u8]) -> Option<Vec<u32>> {
   let entry_count = table_entries(stss, 4)?;
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
   let entry_count = table_entries(ctts, 8)?;

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

/// One stts/ctts segment in decode order: `sample_count` samples starting at
/// `first_sample` that share one sample delta and one composition offset.
#[derive(Debug, Clone, Copy)]
struct TimingSegment {
   first_sample: u32,
   decode_tick: u64,
   sample_count: u32,
   sample_delta: u64,
   composition_offset: i64,
}

/// Result of advancing a [`TimingWalker`].
enum TimingStep {
   Segment(TimingSegment),
   /// All stts entries were consumed.
   Exhausted,
   /// The timing tables are malformed (zero count/delta, missing ctts entry,
   /// or tick overflow).
   Invalid,
}

/// Walks the stts/ctts sample timing tables segment by segment in decode
/// order, keeping the running sample index and decode tick.
struct TimingWalker<'a> {
   stts: &'a [u8],
   composition_offsets: Option<&'a [CompositionOffset]>,
   entry_count: usize,
   sample_index: u32,
   decode_tick: u64,
   stts_index: usize,
   stts_remaining: u32,
   sample_delta: u64,
   ctts_index: usize,
   ctts_remaining: u32,
   composition_offset: i64,
}

impl<'a> TimingWalker<'a> {
   fn new(stts: &'a [u8], composition_offsets: Option<&'a [CompositionOffset]>) -> Option<Self> {
      let entry_count = table_entries(stts, 8)?;
      Some(Self {
         stts,
         composition_offsets,
         entry_count,
         sample_index: 1,
         decode_tick: 0,
         stts_index: 0,
         stts_remaining: 0,
         sample_delta: 0,
         ctts_index: 0,
         ctts_remaining: 0,
         composition_offset: 0,
      })
   }

   fn next_segment(&mut self) -> TimingStep {
      if self.stts_remaining == 0 {
         if self.stts_index == self.entry_count {
            return TimingStep::Exhausted;
         }
         let offset = 8 + self.stts_index * 8;
         let (Some(remaining), Some(delta)) = (
            read_u32_be(self.stts, offset),
            read_u32_be(self.stts, offset + 4),
         ) else {
            return TimingStep::Invalid;
         };
         if remaining == 0 || delta == 0 {
            return TimingStep::Invalid;
         }
         self.stts_remaining = remaining;
         self.sample_delta = u64::from(delta);
         self.stts_index += 1;
      }

      if let Some(offsets) = self.composition_offsets {
         if self.ctts_remaining == 0 {
            let Some(entry) = offsets.get(self.ctts_index) else {
               return TimingStep::Invalid;
            };
            self.ctts_remaining = entry.sample_count;
            self.composition_offset = entry.sample_offset;
            self.ctts_index += 1;
         }
      } else {
         self.ctts_remaining = self.stts_remaining;
         self.composition_offset = 0;
      }

      let segment_count = self.stts_remaining.min(self.ctts_remaining);
      let segment = TimingSegment {
         first_sample: self.sample_index,
         decode_tick: self.decode_tick,
         sample_count: segment_count,
         sample_delta: self.sample_delta,
         composition_offset: self.composition_offset,
      };
      let advance = u64::from(segment_count)
         .checked_mul(self.sample_delta)
         .and_then(|duration| self.decode_tick.checked_add(duration))
         .and_then(|decode_tick| {
            self
               .sample_index
               .checked_add(segment_count)
               .map(|sample_index| (decode_tick, sample_index))
         });
      let Some((decode_tick, next_sample_index)) = advance else {
         return TimingStep::Invalid;
      };
      self.decode_tick = decode_tick;
      self.sample_index = next_sample_index;
      self.stts_remaining -= segment_count;
      self.ctts_remaining -= segment_count;
      TimingStep::Segment(segment)
   }

   /// 1-based index of the next sample the walker will describe.
   fn sample_index(&self) -> u32 {
      self.sample_index
   }

   /// Whether any ctts-described samples remain after the stts table ended.
   fn has_unconsumed_ctts(&self) -> bool {
      self.ctts_remaining != 0
         || self
            .composition_offsets
            .is_some_and(|offsets| self.ctts_index != offsets.len())
   }
}

pub fn select_sample_by_time(
   stts: &[u8],
   composition_offsets: Option<&[CompositionOffset]>,
   presentation_offset: i64,
   target_tick: u64,
) -> Option<SampleSelection> {
   let mut walker = TimingWalker::new(stts, composition_offsets)?;
   let mut before: Option<SampleSelection> = None;
   let mut after: Option<SampleSelection> = None;

   loop {
      match walker.next_segment() {
         TimingStep::Segment(segment) => {
            let first_presentation_tick = i128::from(segment.decode_tick)
               .checked_add(i128::from(segment.composition_offset))?
               .checked_sub(i128::from(presentation_offset))?;
            consider_presentation_segment(
               &mut before,
               &mut after,
               segment.first_sample,
               first_presentation_tick,
               segment.sample_count,
               segment.sample_delta,
               target_tick,
            )?;
         }
         TimingStep::Exhausted => break,
         TimingStep::Invalid => return None,
      }
   }

   if walker.has_unconsumed_ctts() {
      return None;
   }

   before.or(after)
}

/// Considers one presentation segment for sample selection.
///
/// The caller must guarantee `sample_delta` is non-zero (the stts/ctts walker
/// rejects zero deltas while producing segments).
fn consider_presentation_segment(
   before: &mut Option<SampleSelection>,
   after: &mut Option<SampleSelection>,
   first_sample: u32,
   first_tick: i128,
   sample_count: u32,
   sample_delta: u64,
   target_tick: u64,
) -> Option<()> {
   debug_assert!(sample_delta > 0);
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

#[cfg(test)]
fn sample_file_offset(
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
   #[cfg(test)]
   file_offset: u64,
   sample_description_index: u32,
}

/// One stsc run: `chunk_count` consecutive chunks of `samples_per_chunk`
/// samples each, starting at 0-based `first_sample_index`.
#[derive(Debug, Clone, Copy)]
struct StscRun {
   first_chunk: u32,
   samples_per_chunk: u32,
   sample_description_index: u32,
   first_sample_index: u32,
   sample_count: u32,
}

/// Iterates the chunk runs described by the stsc table. Once a run fails
/// validation the iterator stays exhausted and [`StscRuns::failed`] reports
/// that the table was malformed.
struct StscRuns<'a> {
   stsc: &'a [StscEntry],
   final_chunk: u32,
   next_entry: usize,
   first_sample_index: u32,
   failed: bool,
}

impl<'a> StscRuns<'a> {
   fn new(stsc: &'a [StscEntry], chunk_count: usize) -> Option<Self> {
      let final_chunk = u32::try_from(chunk_count).ok()?.checked_add(1)?;
      Some(Self {
         stsc,
         final_chunk,
         next_entry: 0,
         first_sample_index: 0,
         failed: false,
      })
   }

   fn failed(&self) -> bool {
      self.failed
   }
}

impl Iterator for StscRuns<'_> {
   type Item = StscRun;

   fn next(&mut self) -> Option<StscRun> {
      if self.failed {
         return None;
      }
      let entry = self.stsc.get(self.next_entry)?;
      let next_chunk = self
         .stsc
         .get(self.next_entry + 1)
         .map(|next| next.first_chunk)
         .unwrap_or(self.final_chunk);
      let run = if entry.first_chunk < next_chunk && next_chunk <= self.final_chunk {
         next_chunk
            .checked_sub(entry.first_chunk)
            .and_then(|chunk_count| chunk_count.checked_mul(entry.samples_per_chunk))
            .and_then(|sample_count| {
               self
                  .first_sample_index
                  .checked_add(sample_count)
                  .map(|run_end| {
                     (
                        StscRun {
                           first_chunk: entry.first_chunk,
                           samples_per_chunk: entry.samples_per_chunk,
                           sample_description_index: entry.sample_description_index,
                           first_sample_index: self.first_sample_index,
                           sample_count,
                        },
                        run_end,
                     )
                  })
            })
      } else {
         None
      };
      let Some((run, run_end)) = run else {
         self.failed = true;
         return None;
      };
      self.next_entry += 1;
      self.first_sample_index = run_end;
      Some(run)
   }
}

/// Checks whether every sample in `start_sample..=end_sample` (1-based) uses
/// `description_index`, walking the stsc runs once — O(stsc entries) instead
/// of one `sample_location` walk per sample.
pub fn range_uses_description_index(
   start_sample: u32,
   end_sample: u32,
   description_index: u32,
   stsc: &[StscEntry],
   chunk_offsets: &[u64],
) -> bool {
   if start_sample == 0 || end_sample < start_sample {
      return false;
   }
   let first = start_sample - 1;
   let last = end_sample - 1;
   let Some(mut runs) = StscRuns::new(stsc, chunk_offsets.len()) else {
      return false;
   };
   // Runs are contiguous, so the range is covered iff every run overlapping
   // [first, last] starts where the previous one ended and uses the index.
   let mut expected = first;
   for run in &mut runs {
      let run_end = run.first_sample_index + run.sample_count;
      if run_end <= first {
         continue;
      }
      if run.first_sample_index > expected {
         break;
      }
      if run.sample_description_index != description_index {
         return false;
      }
      expected = expected.max(run_end);
      if expected > last {
         return true;
      }
   }
   !runs.failed() && expected > last
}

/// Locates samples by file offset, amortizing the stsc and sample-size walks
/// across calls. Queries must be made in non-decreasing sample-index order;
/// out-of-order queries return `None`.
pub struct SampleLocator<'a> {
   sizes: &'a SampleSizes,
   chunk_offsets: &'a [u64],
   runs: StscRuns<'a>,
   run: StscRun,
   chunk_number: u32,
   next_sample: u32,
   next_offset: u64,
   samples_left_in_chunk: u32,
}

impl<'a> SampleLocator<'a> {
   pub fn new(
      sizes: &'a SampleSizes,
      stsc: &'a [StscEntry],
      chunk_offsets: &'a [u64],
   ) -> Option<Self> {
      if stsc.first()?.first_chunk != 1 || chunk_offsets.is_empty() {
         return None;
      }
      let mut runs = StscRuns::new(stsc, chunk_offsets.len())?;
      let run = runs.next()?;
      Some(Self {
         sizes,
         chunk_offsets,
         runs,
         run,
         chunk_number: run.first_chunk,
         next_sample: 1,
         next_offset: *chunk_offsets.first()?,
         samples_left_in_chunk: run.samples_per_chunk,
      })
   }

   /// File offset of `sample_index` (1-based).
   pub fn file_offset(&mut self, sample_index: u32) -> Option<u64> {
      if sample_index == 0
         || sample_index > self.sizes.sample_count
         || sample_index < self.next_sample
      {
         return None;
      }
      while self.next_sample < sample_index {
         let size = sample_size(self.next_sample, self.sizes)?;
         self.next_offset = self.next_offset.checked_add(u64::from(size))?;
         self.next_sample = self.next_sample.checked_add(1)?;
         self.samples_left_in_chunk -= 1;
         if self.samples_left_in_chunk == 0 && self.next_sample <= sample_index {
            self.move_to_next_chunk()?;
         }
      }
      Some(self.next_offset)
   }

   fn move_to_next_chunk(&mut self) -> Option<()> {
      self.chunk_number = self.chunk_number.checked_add(1)?;
      let run_chunks = self.run.sample_count / self.run.samples_per_chunk;
      if self.chunk_number >= self.run.first_chunk.checked_add(run_chunks)? {
         self.run = self.runs.next()?;
         self.chunk_number = self.run.first_chunk;
      }
      self.next_offset = *self
         .chunk_offsets
         .get(usize::try_from(self.chunk_number.checked_sub(1)?).ok()?)?;
      self.samples_left_in_chunk = self.run.samples_per_chunk;
      Some(())
   }
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
   for run in StscRuns::new(stsc, chunk_offsets.len())? {
      let run_end = run.first_sample_index.checked_add(run.sample_count)?;
      if target < run_end {
         let within_run = target.checked_sub(run.first_sample_index)?;
         let chunk_in_run = within_run / run.samples_per_chunk;
         let within_chunk = within_run % run.samples_per_chunk;
         let chunk_number = run.first_chunk.checked_add(chunk_in_run)?;
         let first_sample_in_chunk = run
            .first_sample_index
            .checked_add(chunk_in_run.checked_mul(run.samples_per_chunk)?)?;
         let prior_bytes = sum_sample_sizes(first_sample_in_chunk, within_chunk, sizes)?;
         let _file_offset = chunk_offsets
            .get(usize::try_from(chunk_number.checked_sub(1)?).ok()?)?
            .checked_add(prior_bytes)?;
         return Some(SampleLocation {
            #[cfg(test)]
            file_offset: _file_offset,
            sample_description_index: run.sample_description_index,
         });
      }
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

   let mut described_samples = 0u32;
   let mut runs = StscRuns::new(stsc, chunk_offsets.len())?;
   for run in &mut runs {
      if usize::try_from(run.sample_description_index).ok()? > sample_description_count {
         return None;
      }
      described_samples = described_samples.checked_add(run.sample_count)?;
   }
   if runs.failed() || described_samples != sizes.sample_count {
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
   let range_len = usize::try_from(
      end_sample
         .checked_sub(start_sample)
         .and_then(|count| count.checked_add(1))?,
   )
   .ok()?;
   let mut walker = TimingWalker::new(stts, composition_offsets)?;

   let mut result = Vec::new();
   result.try_reserve(range_len).ok()?;
   while walker.sample_index() <= end_sample {
      let TimingStep::Segment(segment) = walker.next_segment() else {
         return None;
      };
      let count = segment.sample_count;
      let wanted_start = start_sample.saturating_sub(segment.first_sample).min(count);
      let wanted_end = end_sample
         .checked_add(1)?
         .saturating_sub(segment.first_sample)
         .min(count);
      for position in wanted_start..wanted_end {
         let sample_decode_tick = segment
            .decode_tick
            .checked_add(u64::from(position).checked_mul(segment.sample_delta)?)?;
         let presentation_tick = i128::from(sample_decode_tick)
            .checked_add(i128::from(segment.composition_offset))?
            .checked_sub(i128::from(presentation_offset))?;
         result.push((
            segment.first_sample.checked_add(position)?,
            presentation_tick,
         ));
      }
   }

   (result.len() == range_len).then_some(result)
}

pub fn duration_to_ticks(duration: Duration, timescale: u32) -> u64 {
   let ticks = duration.as_nanos().saturating_mul(u128::from(timescale)) / 1_000_000_000;
   u64::try_from(ticks).unwrap_or(u64::MAX)
}

fn stts_sample_count(stts: &[u8]) -> Option<u32> {
   let entry_count = table_entries(stts, 8)?;
   (0..entry_count).try_fold(0u32, |total, index| {
      total.checked_add(read_u32_be(stts, 8 + index * 8)?)
   })
}

/// Total duration in media ticks described by the stts table.
pub fn stts_duration_ticks(stts: &[u8]) -> Option<u64> {
   let entry_count = table_entries(stts, 8)?;
   (0..entry_count).try_fold(0u64, |total, index| {
      let count = u64::from(read_u32_be(stts, 8 + index * 8)?);
      let delta = u64::from(read_u32_be(stts, 8 + index * 8 + 4)?);
      total.checked_add(count.checked_mul(delta)?)
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
   fn rejects_zero_sample_delta_while_selecting() {
      let mut stts = vec![0; 8];
      stts[4..8].copy_from_slice(&1u32.to_be_bytes());
      stts.extend_from_slice(&2u32.to_be_bytes());
      stts.extend_from_slice(&0u32.to_be_bytes());

      assert!(select_sample_by_time(&stts, None, 0, 1_000).is_none());
   }

   fn two_run_tables() -> (SampleSizes, Vec<StscEntry>, Vec<u64>) {
      // chunks 1-2 use description 1 (2 samples each), chunks 3-4 use
      // description 2 (1 sample each); sample sizes vary per sample.
      let sizes = SampleSizes {
         fixed_size: 0,
         sizes: vec![10, 20, 30, 40, 50, 60],
         sample_count: 6,
      };
      let stsc = vec![
         StscEntry {
            first_chunk: 1,
            samples_per_chunk: 2,
            sample_description_index: 1,
         },
         StscEntry {
            first_chunk: 3,
            samples_per_chunk: 1,
            sample_description_index: 2,
         },
      ];
      let chunk_offsets = vec![100, 200, 300, 400];
      (sizes, stsc, chunk_offsets)
   }

   #[test]
   fn range_description_index_covers_whole_range_in_one_walk() {
      let (sizes, stsc, chunk_offsets) = two_run_tables();

      assert!(range_uses_description_index(1, 4, 1, &stsc, &chunk_offsets));
      assert!(range_uses_description_index(5, 6, 2, &stsc, &chunk_offsets));
      assert!(!range_uses_description_index(
         1,
         6,
         1,
         &stsc,
         &chunk_offsets
      ));
      assert!(!range_uses_description_index(
         4,
         5,
         1,
         &stsc,
         &chunk_offsets
      ));
      assert!(!range_uses_description_index(
         6,
         7,
         2,
         &stsc,
         &chunk_offsets
      ));
      assert!(!range_uses_description_index(
         0,
         4,
         1,
         &stsc,
         &chunk_offsets
      ));
      let _ = sizes;
   }

   #[test]
   fn sample_locator_matches_sample_file_offset_in_order() {
      let (sizes, stsc, chunk_offsets) = two_run_tables();
      let mut locator = SampleLocator::new(&sizes, &stsc, &chunk_offsets).unwrap();

      for sample_index in 1..=6 {
         assert_eq!(
            locator.file_offset(sample_index),
            sample_file_offset(sample_index, &sizes, &stsc, &chunk_offsets),
            "sample {sample_index}"
         );
      }
   }

   #[test]
   fn sample_locator_rejects_out_of_order_queries() {
      let (sizes, stsc, chunk_offsets) = two_run_tables();
      let mut locator = SampleLocator::new(&sizes, &stsc, &chunk_offsets).unwrap();

      assert_eq!(locator.file_offset(3), Some(200));
      assert_eq!(locator.file_offset(2), None);
   }

   #[test]
   fn sums_stts_duration_with_checked_arithmetic() {
      let mut stts = vec![0; 8];
      stts[4..8].copy_from_slice(&2u32.to_be_bytes());
      stts.extend_from_slice(&3u32.to_be_bytes());
      stts.extend_from_slice(&1_000u32.to_be_bytes());
      stts.extend_from_slice(&2u32.to_be_bytes());
      stts.extend_from_slice(&500u32.to_be_bytes());

      assert_eq!(stts_duration_ticks(&stts), Some(4_000));

      let mut large = vec![0; 8];
      large[4..8].copy_from_slice(&1u32.to_be_bytes());
      large.extend_from_slice(&u32::MAX.to_be_bytes());
      large.extend_from_slice(&u32::MAX.to_be_bytes());

      assert_eq!(
         stts_duration_ticks(&large),
         Some(u64::from(u32::MAX) * u64::from(u32::MAX))
      );
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

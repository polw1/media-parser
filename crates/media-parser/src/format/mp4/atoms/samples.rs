//! MP4 sample-table parsing and sample reads.

use super::sample_timing::{CompositionOffset, stts_sample_count};
use super::{Mp4Nav, iter_boxes};
use crate::decoders::h264::AvcConfig;
use crate::helpers::{read_u16_be, read_u32_be, read_u64_be};

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

pub fn sample_description_index(
   sample_index: u32,
   sizes: &SampleSizes,
   stsc: &[StscEntry],
   chunk_offsets: &[u64],
) -> Option<u32> {
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
         return Some(run.sample_description_index);
      }
   }
   None
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

#[cfg(test)]
mod tests {
   use super::*;

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
   fn sample_locator_matches_expected_offsets_in_order() {
      let (sizes, stsc, chunk_offsets) = two_run_tables();
      let mut locator = SampleLocator::new(&sizes, &stsc, &chunk_offsets).unwrap();

      for (sample_index, expected_offset) in [100, 110, 200, 230, 300, 400].into_iter().enumerate()
      {
         assert_eq!(
            locator.file_offset(u32::try_from(sample_index + 1).unwrap()),
            Some(expected_offset),
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
         sample_description_index(1_000, &sizes, &stsc, &chunk_offsets),
         Some(1)
      );
      assert_eq!(
         sample_description_index(1_000_000_000, &sizes, &stsc, &chunk_offsets),
         Some(1)
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

//! MP4 sample-table parsing and sample reads.

use super::budget::{RetainedBudget, TableParseError, TableResult, budgeted_vec};
use super::sample_timing::{CompositionOffset, stts_sample_count};
use super::{Mp4Nav, iter_boxes};
use crate::decoders::h264::{AvcColorMetadata, AvcConfig};
use crate::helpers::{read_u16_be, read_u32_be, read_u64_be};

const SAMPLE_SIZE_PREFIX_INTERVAL: usize = 256;

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
   size_prefixes: Vec<u64>,
}

impl SampleSizes {
   #[cfg(test)]
   pub(crate) fn fixed(sample_count: u32, fixed_size: u32) -> Option<Self> {
      (fixed_size != 0).then_some(Self {
         fixed_size,
         sizes: Vec::new(),
         sample_count,
         size_prefixes: Vec::new(),
      })
   }

   #[cfg(test)]
   pub(crate) fn variable(sizes: Vec<u32>) -> Option<Self> {
      let sample_count = u32::try_from(sizes.len()).ok()?;
      Self::from_parts(0, sizes, sample_count)
   }

   #[cfg(test)]
   pub(in crate::format::mp4) fn size_prefixes_capacity(&self) -> usize {
      self.size_prefixes.capacity()
   }

   fn from_parts(fixed_size: u32, sizes: Vec<u32>, sample_count: u32) -> Option<Self> {
      if fixed_size != 0 {
         return sizes.is_empty().then_some(Self {
            fixed_size,
            sizes,
            sample_count,
            size_prefixes: Vec::new(),
         });
      }
      if usize::try_from(sample_count).ok()? != sizes.len() {
         return None;
      }

      let mut size_prefixes = Vec::new();
      size_prefixes
         .try_reserve(sizes.len().div_ceil(SAMPLE_SIZE_PREFIX_INTERVAL))
         .ok()?;
      let mut total = 0u64;
      for (index, size) in sizes.iter().copied().enumerate() {
         if index % SAMPLE_SIZE_PREFIX_INTERVAL == 0 {
            size_prefixes.push(total);
         }
         total = total.checked_add(u64::from(size))?;
      }
      Some(Self {
         fixed_size,
         sizes,
         sample_count,
         size_prefixes,
      })
   }

   fn from_parts_bounded(
      fixed_size: u32,
      sizes: Vec<u32>,
      sample_count: u32,
      budget: &mut RetainedBudget,
   ) -> TableResult<Self> {
      if fixed_size != 0 {
         return sizes
            .is_empty()
            .then_some(Self {
               fixed_size,
               sizes,
               sample_count,
               size_prefixes: Vec::new(),
            })
            .ok_or(TableParseError::Invalid("fixed stsz has size entries"));
      }
      if usize::try_from(sample_count).map_err(|_| TableParseError::BudgetExceeded)? != sizes.len()
      {
         return Err(TableParseError::Invalid("stsz sample count mismatch"));
      }

      let prefix_count = sizes.len().div_ceil(SAMPLE_SIZE_PREFIX_INTERVAL);
      let mut size_prefixes = budgeted_vec(prefix_count, budget)?;
      let mut total = 0u64;
      for (index, size) in sizes.iter().copied().enumerate() {
         if index % SAMPLE_SIZE_PREFIX_INTERVAL == 0 {
            size_prefixes.push(total);
         }
         total = total
            .checked_add(u64::from(size))
            .ok_or(TableParseError::Invalid("stsz byte total overflow"))?;
      }
      Ok(Self {
         fixed_size,
         sizes,
         sample_count,
         size_prefixes,
      })
   }

   /// Bytes occupied by samples preceding `sample_index` (1-based), plus the
   /// number of raw variable-size entries examined after the nearest prefix.
   fn byte_offset_before(&self, sample_index: u32) -> Option<(u64, usize)> {
      if sample_index == 0 || sample_index > self.sample_count {
         return None;
      }
      let preceding = usize::try_from(sample_index - 1).ok()?;
      if self.fixed_size != 0 {
         return Some((
            u64::from(self.fixed_size).checked_mul(u64::try_from(preceding).ok()?)?,
            0,
         ));
      }

      let block = preceding / SAMPLE_SIZE_PREFIX_INTERVAL;
      let block_start = block.checked_mul(SAMPLE_SIZE_PREFIX_INTERVAL)?;
      let mut offset = *self.size_prefixes.get(block)?;
      let remainder = self.sizes.get(block_start..preceding)?;
      for size in remainder {
         offset = offset.checked_add(u64::from(*size))?;
      }
      Some((offset, remainder.len()))
   }
}

/// Reads the entry count of a full-box table (8-byte header of version/flags
/// plus entry count) and validates that `entry_size`-byte entries fit in the
/// box payload.
pub fn table_entries(buf: &[u8], entry_size: usize) -> Option<usize> {
   let entry_count = usize::try_from(read_u32_be(buf, 4)?).ok()?;
   (entry_count <= buf.len().checked_sub(8)? / entry_size).then_some(entry_count)
}

/// Reads the matrix and range hints from an `nclx`/`nclc` colour box.
///
/// Returns `false` for other parameter types and for bodies too short to carry
/// a matrix, so the caller keeps looking at later `colr` boxes. Colour is a
/// presentation hint: an unreadable one must never invalidate the `avcC` it
/// sits next to.
fn parse_colr(payload: &[u8], color: &mut AvcColorMetadata) -> bool {
   let Some(parameter_type) = payload.get(..4) else {
      return false;
   };
   if parameter_type != b"nclx" && parameter_type != b"nclc" {
      return false;
   }
   let Some(matrix_coefficients) = read_u16_be(payload, 8) else {
      return false;
   };
   color.matrix_coefficients = (matrix_coefficients != 2).then_some(matrix_coefficients);
   if parameter_type == b"nclx" {
      // Some muxers emit an `nclx` body without its trailing flags byte; the
      // matrix is still usable on its own.
      color.full_range = payload.get(10).map(|flags| flags & 0x80 != 0);
   }
   true
}

pub fn parse_avc_config(sample_entry_payload: &[u8]) -> Option<AvcConfig> {
   let children = sample_entry_payload.get(78..)?;
   let mut avcc = None;
   let mut color = AvcColorMetadata::default();
   let mut color_found = false;
   for (fourcc, payload) in iter_boxes(children) {
      match &fourcc {
         b"avcC" if avcc.is_none() => avcc = Some(payload),
         b"colr" if !color_found => color_found = parse_colr(payload, &mut color),
         _ => {}
      }
   }
   let avcc = avcc?;
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
      color,
   })
}

pub fn parse_sample_sizes(stsz: &[u8]) -> Option<SampleSizes> {
   let (fixed_size, sample_count) = sample_size_header(stsz)?;
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

   SampleSizes::from_parts(fixed_size, sizes, sample_count)
}

pub(in crate::format::mp4) fn parse_sample_sizes_bounded(
   stsz: &[u8],
   budget: &mut RetainedBudget,
) -> TableResult<SampleSizes> {
   let (fixed_size, sample_count) =
      sample_size_header(stsz).ok_or(TableParseError::Invalid("malformed stsz table"))?;
   let expected_len = if fixed_size == 0 {
      usize::try_from(sample_count)
         .map_err(|_| TableParseError::Invalid("malformed stsz entry count"))?
         .checked_mul(4)
         .and_then(|bytes| bytes.checked_add(12))
         .ok_or(TableParseError::Invalid("malformed stsz entry count"))?
   } else {
      12
   };
   if stsz.len() != expected_len {
      return Err(TableParseError::Invalid("malformed stsz entry count"));
   }

   let count = usize::try_from(sample_count).map_err(|_| TableParseError::BudgetExceeded)?;
   let mut sizes = if fixed_size == 0 {
      budgeted_vec(count, budget)?
   } else {
      Vec::new()
   };
   if fixed_size == 0 {
      for index in 0..count {
         sizes.push(
            read_u32_be(stsz, 12 + index * 4)
               .ok_or(TableParseError::Invalid("malformed stsz entry"))?,
         );
      }
   }
   SampleSizes::from_parts_bounded(fixed_size, sizes, sample_count, budget)
}

fn sample_size_header(stsz: &[u8]) -> Option<(u32, u32)> {
   Some((read_u32_be(stsz, 4)?, read_u32_be(stsz, 8)?))
}

pub fn parse_stsc(stsc: &[u8]) -> Option<Vec<StscEntry>> {
   let entry_count = table_entries(stsc, 12)?;

   let mut entries = Vec::new();
   entries.try_reserve(entry_count).ok()?;
   for index in 0..entry_count {
      let entry = parse_stsc_entry(stsc, index)?;
      if !valid_stsc_entry(entries.last(), entry) {
         return None;
      }
      entries.push(entry);
   }
   Some(entries)
}

pub(in crate::format::mp4) fn parse_stsc_bounded(
   stsc: &[u8],
   budget: &mut RetainedBudget,
) -> TableResult<Vec<StscEntry>> {
   let entry_count = raw_table_entries(stsc, "malformed stsc table")?;
   validate_exact_table_len(stsc, entry_count, 12, "malformed stsc table")?;
   let mut entries = budgeted_vec(entry_count, budget)?;
   for index in 0..entry_count {
      let entry =
         parse_stsc_entry(stsc, index).ok_or(TableParseError::Invalid("malformed stsc entry"))?;
      if !valid_stsc_entry(entries.last(), entry) {
         return Err(TableParseError::Invalid("invalid stsc entry"));
      }
      entries.push(entry);
   }
   Ok(entries)
}

fn parse_stsc_entry(stsc: &[u8], index: usize) -> Option<StscEntry> {
   let offset = 8usize.checked_add(index.checked_mul(12)?)?;
   Some(StscEntry {
      first_chunk: read_u32_be(stsc, offset)?,
      samples_per_chunk: read_u32_be(stsc, offset + 4)?,
      sample_description_index: read_u32_be(stsc, offset + 8)?,
   })
}

fn valid_stsc_entry(previous: Option<&StscEntry>, entry: StscEntry) -> bool {
   entry.first_chunk != 0
      && entry.samples_per_chunk != 0
      && entry.sample_description_index != 0
      && previous.is_none_or(|previous| previous.first_chunk < entry.first_chunk)
}

pub fn parse_chunk_offsets(stbl: &[u8]) -> Option<Vec<u64>> {
   let (table, entry_size, is_64) = chunk_offset_table(stbl)?;
   let entry_count = table_entries(table, entry_size)?;
   let mut offsets = Vec::new();
   offsets.try_reserve(entry_count).ok()?;
   for index in 0..entry_count {
      offsets.push(read_chunk_offset(table, index, entry_size, is_64)?);
   }
   Some(offsets)
}

pub(in crate::format::mp4) fn parse_chunk_offsets_bounded(
   stbl: &[u8],
   budget: &mut RetainedBudget,
) -> TableResult<Vec<u64>> {
   let (table, entry_size, is_64) =
      chunk_offset_table(stbl).ok_or(TableParseError::Invalid("missing stco/co64 table"))?;
   let entry_count = raw_table_entries(table, "malformed chunk offset table")?;
   validate_exact_table_len(
      table,
      entry_count,
      entry_size,
      "malformed chunk offset table",
   )?;
   let mut offsets = budgeted_vec(entry_count, budget)?;
   for index in 0..entry_count {
      offsets.push(
         read_chunk_offset(table, index, entry_size, is_64)
            .ok_or(TableParseError::Invalid("malformed chunk offset entry"))?,
      );
   }
   Ok(offsets)
}

fn chunk_offset_table(stbl: &[u8]) -> Option<(&[u8], usize, bool)> {
   stbl
      .nav(&[*b"stco"])
      .map(|stco| (stco, 4, false))
      .or_else(|| stbl.nav(&[*b"co64"]).map(|co64| (co64, 8, true)))
}

fn read_chunk_offset(table: &[u8], index: usize, entry_size: usize, is_64: bool) -> Option<u64> {
   let offset = 8usize.checked_add(index.checked_mul(entry_size)?)?;
   if is_64 {
      read_u64_be(table, offset)
   } else {
      read_u32_be(table, offset).map(u64::from)
   }
}

fn raw_table_entries(table: &[u8], message: &'static str) -> TableResult<usize> {
   usize::try_from(read_u32_be(table, 4).ok_or(TableParseError::Invalid(message))?)
      .map_err(|_| TableParseError::BudgetExceeded)
}

fn validate_exact_table_len(
   table: &[u8],
   entry_count: usize,
   entry_size: usize,
   message: &'static str,
) -> TableResult<()> {
   let expected_len = entry_count
      .checked_mul(entry_size)
      .and_then(|bytes| bytes.checked_add(8))
      .ok_or(TableParseError::Invalid(message))?;
   if expected_len != table.len() {
      return Err(TableParseError::Invalid(message));
   }
   Ok(())
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
   next_sample: u32,
   #[cfg(test)]
   size_entries_examined: usize,
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
         next_sample: 1,
         #[cfg(test)]
         size_entries_examined: 0,
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
      let target = sample_index - 1;
      while target
         >= self
            .run
            .first_sample_index
            .checked_add(self.run.sample_count)?
      {
         self.run = self.runs.next()?;
      }
      let sample_in_run = target.checked_sub(self.run.first_sample_index)?;
      let chunk_in_run = sample_in_run / self.run.samples_per_chunk;
      let chunk_number = self.run.first_chunk.checked_add(chunk_in_run)?;
      let chunk_offset = *self
         .chunk_offsets
         .get(usize::try_from(chunk_number.checked_sub(1)?).ok()?)?;
      let chunk_first_sample = self
         .run
         .first_sample_index
         .checked_add(chunk_in_run.checked_mul(self.run.samples_per_chunk)?)?
         .checked_add(1)?;
      let (sample_prefix, sample_steps) = self.sizes.byte_offset_before(sample_index)?;
      let (chunk_prefix, chunk_steps) = self.sizes.byte_offset_before(chunk_first_sample)?;
      #[cfg(test)]
      {
         self.size_entries_examined = self
            .size_entries_examined
            .checked_add(sample_steps.checked_add(chunk_steps)?)?;
      }
      #[cfg(not(test))]
      let _ = (sample_steps, chunk_steps);
      self.next_sample = sample_index;
      chunk_offset.checked_add(sample_prefix.checked_sub(chunk_prefix)?)
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

   fn append_box(target: &mut Vec<u8>, fourcc: &[u8; 4], payload: &[u8]) {
      let size = u32::try_from(payload.len() + 8).expect("test box fits u32");
      target.extend_from_slice(&size.to_be_bytes());
      target.extend_from_slice(fourcc);
      target.extend_from_slice(payload);
   }

   #[test]
   fn bounded_stsz_validates_framing_before_charging_declared_entries() {
      let mut stsz = vec![0u8; 12];
      stsz[8..12].copy_from_slice(&u32::MAX.to_be_bytes());
      let mut budget = RetainedBudget::new(8);

      assert_eq!(
         parse_sample_sizes_bounded(&stsz, &mut budget).unwrap_err(),
         TableParseError::Invalid("malformed stsz entry count")
      );
   }

   #[test]
   fn bounded_stsc_and_offsets_reject_trailing_bytes() {
      let mut stsc = vec![0u8; 8];
      stsc[4..8].copy_from_slice(&1u32.to_be_bytes());
      stsc.extend_from_slice(&1u32.to_be_bytes());
      stsc.extend_from_slice(&1u32.to_be_bytes());
      stsc.extend_from_slice(&1u32.to_be_bytes());
      stsc.push(0);
      // A budget too small for a single entry separates the two orders: were
      // the charge to precede the framing check, this would be the fatal
      // BudgetExceeded instead of the track-local Invalid.
      let mut retained = RetainedBudget::new(0);
      assert!(matches!(
         parse_stsc_bounded(&stsc, &mut retained),
         Err(TableParseError::Invalid("malformed stsc table"))
      ));

      let mut stco = vec![0u8; 8];
      stco[4..8].copy_from_slice(&1u32.to_be_bytes());
      stco.extend_from_slice(&100u32.to_be_bytes());
      stco.push(0);
      let mut stbl = Vec::new();
      append_box(&mut stbl, b"stco", &stco);
      let mut retained = RetainedBudget::new(0);
      assert!(matches!(
         parse_chunk_offsets_bounded(&stbl, &mut retained),
         Err(TableParseError::Invalid("malformed chunk offset table"))
      ));
   }

   #[test]
   fn exact_table_length_overflow_is_invalid() {
      assert_eq!(
         validate_exact_table_len(&[], usize::MAX, 2, "overflowed table"),
         Err(TableParseError::Invalid("overflowed table"))
      );
   }

   #[test]
   fn bounded_stsc_rejects_a_false_small_count_without_charging_budget() {
      let mut stsc = vec![0u8; 8];
      stsc[4..8].copy_from_slice(&1u32.to_be_bytes());
      let mut budget = RetainedBudget::new(1024);

      assert!(matches!(
         parse_stsc_bounded(&stsc, &mut budget),
         Err(TableParseError::Invalid("malformed stsc table"))
      ));
      assert_eq!(budget.used_bytes(), 0);
   }

   #[test]
   fn parses_nclx_matrix_and_range_with_avc_config() {
      let mut sample_entry = vec![0; 78];
      append_box(&mut sample_entry, b"avcC", &[1, 66, 0, 30, 0xff, 0xe0, 0]);
      let mut colr = Vec::from(&b"nclx"[..]);
      colr.extend_from_slice(&1u16.to_be_bytes());
      colr.extend_from_slice(&1u16.to_be_bytes());
      colr.extend_from_slice(&1u16.to_be_bytes());
      colr.push(0x80);
      append_box(&mut sample_entry, b"colr", &colr);

      let config = parse_avc_config(&sample_entry).expect("valid avc3 description parses");

      assert_eq!(config.color.matrix_coefficients, Some(1));
      assert_eq!(config.color.full_range, Some(true));
   }

   #[test]
   fn parses_nclc_matrix_without_inventing_a_range() {
      let mut sample_entry = vec![0; 78];
      append_box(&mut sample_entry, b"avcC", &[1, 66, 0, 30, 0xff, 0xe0, 0]);
      let mut colr = Vec::from(&b"nclc"[..]);
      colr.extend_from_slice(&6u16.to_be_bytes());
      colr.extend_from_slice(&6u16.to_be_bytes());
      colr.extend_from_slice(&6u16.to_be_bytes());
      append_box(&mut sample_entry, b"colr", &colr);

      let config = parse_avc_config(&sample_entry).expect("valid nclc description parses");

      assert_eq!(config.color.matrix_coefficients, Some(6));
      assert_eq!(config.color.full_range, None);
   }

   #[test]
   fn keeps_the_avc_config_when_the_colr_box_is_truncated() {
      let mut sample_entry = vec![0; 78];
      append_box(&mut sample_entry, b"avcC", &[1, 66, 0, 30, 0xff, 0xe0, 0]);
      // An `nclx` body without its trailing flags byte, and a `colr` too short
      // to even name its parameter type.
      let mut short_nclx = Vec::from(&b"nclx"[..]);
      short_nclx.extend_from_slice(&1u16.to_be_bytes());
      short_nclx.extend_from_slice(&1u16.to_be_bytes());
      short_nclx.extend_from_slice(&1u16.to_be_bytes());
      append_box(&mut sample_entry, b"colr", &short_nclx);
      append_box(&mut sample_entry, b"colr", b"ncl");

      let config = parse_avc_config(&sample_entry).expect("a bad colr cannot void the avcC");

      assert_eq!(config.length_size, 4);
      assert_eq!(config.color.matrix_coefficients, Some(1));
      assert_eq!(config.color.full_range, None);
   }

   #[test]
   fn finds_nclx_after_an_unsupported_colr_box() {
      let mut sample_entry = vec![0; 78];
      append_box(&mut sample_entry, b"avcC", &[1, 66, 0, 30, 0xff, 0xe0, 0]);
      append_box(&mut sample_entry, b"colr", b"profignored");
      let mut nclx = Vec::from(&b"nclx"[..]);
      nclx.extend_from_slice(&1u16.to_be_bytes());
      nclx.extend_from_slice(&1u16.to_be_bytes());
      nclx.extend_from_slice(&1u16.to_be_bytes());
      nclx.push(0);
      append_box(&mut sample_entry, b"colr", &nclx);

      let config = parse_avc_config(&sample_entry).expect("valid later nclx is found");

      assert_eq!(config.color.matrix_coefficients, Some(1));
      assert_eq!(config.color.full_range, Some(false));
   }

   fn two_run_tables() -> (SampleSizes, Vec<StscEntry>, Vec<u64>) {
      // chunks 1-2 use description 1 (2 samples each), chunks 3-4 use
      // description 2 (1 sample each); sample sizes vary per sample.
      let sizes = SampleSizes::variable(vec![10, 20, 30, 40, 50, 60]).unwrap();
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
   fn sample_locator_does_not_scan_every_size_before_a_sparse_query() {
      let sample_count = 10_000;
      let sizes = SampleSizes::variable(vec![1; sample_count as usize]).unwrap();
      let stsc = [StscEntry {
         first_chunk: 1,
         samples_per_chunk: sample_count,
         sample_description_index: 1,
      }];
      let chunk_offsets = [100];
      let mut locator = SampleLocator::new(&sizes, &stsc, &chunk_offsets).unwrap();

      assert_eq!(locator.file_offset(sample_count), Some(10_099));
      assert!(
         locator.size_entries_examined <= 512,
         "sparse lookup examined {} preceding sizes",
         locator.size_entries_examined
      );
   }

   #[test]
   fn sample_locator_bounds_both_sparse_prefix_tails() {
      let sizes = SampleSizes::variable(vec![1; 768]).unwrap();
      let stsc = [
         StscEntry {
            first_chunk: 1,
            samples_per_chunk: 255,
            sample_description_index: 1,
         },
         StscEntry {
            first_chunk: 2,
            samples_per_chunk: 513,
            sample_description_index: 1,
         },
      ];
      let chunk_offsets = [100, 1_000];
      let mut locator = SampleLocator::new(&sizes, &stsc, &chunk_offsets).unwrap();

      assert_eq!(locator.file_offset(768), Some(1_512));
      assert_eq!(locator.size_entries_examined, 510);
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
      let sizes = SampleSizes::fixed(1_000_000_000, 4).unwrap();
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
      let sizes = SampleSizes::fixed(1, 1).unwrap();
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

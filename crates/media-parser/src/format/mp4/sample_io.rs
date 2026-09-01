//! Bounded, coalesced reads for indexed MP4 media samples.
//!
//! Planning is transactional: ordering, locations, sizes, overlap, coalescing,
//! and every configured limit are validated before the shared budget changes.
//! A successful plan commits its aggregate sample, logical-byte,
//! physical-byte, and region charges. Later read or result-allocation failures
//! do not refund those charges. Limit and validation errors use terminology
//! shared by every caller.
//!
//! Each [`SampleData`] is a checked view into a shared physical read region.
//! The region remains alive through consumer clones, including decode jobs,
//! and is released after the last sample view is dropped.

use super::atoms::{SampleLocator, SampleSizes, StscEntry, sample_size};
use crate::errors::{MediaParserError, Result};
use crate::stream::StreamReader;
use futures::stream::{self, StreamExt};
use std::collections::HashMap;
use std::ops::Range;
use std::sync::Arc;

const MAX_CONCURRENT_READS: usize = 4;

/// Stateless limits applied to one sample-read plan plus aggregate ceilings
/// applied against its shared [`SampleReadBudget`].
#[derive(Clone, Copy, Debug)]
pub(super) struct SampleReadLimits {
   pub(super) max_samples: usize,
   pub(super) max_sample_bytes: usize,
   pub(super) max_logical_bytes: usize,
   pub(super) max_physical_bytes: usize,
   pub(super) max_regions: usize,
   pub(super) max_region_bytes: usize,
   pub(super) max_coalesce_gap_bytes: usize,
}

/// Aggregate charges committed by successful plans.
///
/// Planning failures leave this value unchanged. Once a plan succeeds, later
/// read or result-allocation failures do not refund its charges.
#[derive(Debug, Default)]
pub(super) struct SampleReadBudget {
   samples: usize,
   logical_bytes: usize,
   physical_bytes: usize,
   regions: usize,
}

/// A checked sample range backed by a shared physical read region.
///
/// Clones retain the region without copying its payload. The allocation is
/// released only after every consumer of every view into the region is gone.
#[derive(Clone, Debug)]
pub(super) struct SampleData {
   bytes: Arc<Vec<u8>>,
   range: Range<usize>,
}

impl SampleData {
   pub(super) fn as_slice(&self) -> &[u8] {
      &self.bytes[self.range.clone()]
   }
}

impl AsRef<[u8]> for SampleData {
   fn as_ref(&self) -> &[u8] {
      self.as_slice()
   }
}

fn shared_region(bytes: Vec<u8>) -> Arc<Vec<u8>> {
   Arc::new(bytes)
}

#[derive(Debug)]
struct SampleSlice {
   sample_index: u32,
   offset: usize,
   size: usize,
}

#[derive(Debug)]
struct LocatedSample {
   sample_index: u32,
   offset: u64,
   size: usize,
}

#[derive(Debug)]
struct ReadBatch {
   offset: u64,
   size: usize,
   samples: Vec<SampleSlice>,
}

/// Exhausting a limit or failing an allocation is a resource failure of the
/// request, not evidence of malformed input, so both surface as
/// [`MediaParserError::Other`]. [`MediaParserError::InvalidFormat`] stays
/// reserved for bytes that are actually wrong, including arithmetic derived
/// from them.
#[derive(Debug)]
pub(super) enum SampleReadError {
   Limit(String),
   Track(MediaParserError),
   Fatal(MediaParserError),
}

impl SampleReadError {
   pub(super) fn into_media_error(self) -> MediaParserError {
      match self {
         Self::Limit(reason) => MediaParserError::Other(reason),
         Self::Track(error) | Self::Fatal(error) => error,
      }
   }
}

type SampleReadResult<T> = std::result::Result<T, SampleReadError>;

fn track_error(message: impl Into<String>) -> SampleReadError {
   SampleReadError::Track(MediaParserError::InvalidFormat(message.into()))
}

fn fatal_error(message: impl Into<String>) -> SampleReadError {
   SampleReadError::Fatal(MediaParserError::InvalidFormat(message.into()))
}

fn allocation_error(message: impl Into<String>) -> SampleReadError {
   SampleReadError::Fatal(MediaParserError::Other(message.into()))
}

fn limit_error(message: impl Into<String>) -> SampleReadError {
   SampleReadError::Limit(message.into())
}

fn allocate_located_samples(capacity: usize) -> SampleReadResult<Vec<LocatedSample>> {
   let mut samples = Vec::new();
   samples
      .try_reserve_exact(capacity)
      .map_err(|_| allocation_error("sample planning allocation failed"))?;
   Ok(samples)
}

pub(super) async fn read_samples_coalesced(
   reader: &dyn StreamReader,
   sample_indices: &[u32],
   sizes: &SampleSizes,
   stsc: &[StscEntry],
   chunk_offsets: &[u64],
   limits: SampleReadLimits,
   budget: &mut SampleReadBudget,
) -> Result<HashMap<u32, SampleData>> {
   read_samples_coalesced_classified(
      reader,
      sample_indices,
      sizes,
      stsc,
      chunk_offsets,
      limits,
      budget,
   )
   .await
   .map_err(SampleReadError::into_media_error)
}

pub(super) async fn read_samples_coalesced_classified(
   reader: &dyn StreamReader,
   sample_indices: &[u32],
   sizes: &SampleSizes,
   stsc: &[StscEntry],
   chunk_offsets: &[u64],
   limits: SampleReadLimits,
   budget: &mut SampleReadBudget,
) -> SampleReadResult<HashMap<u32, SampleData>> {
   let batches = plan_read_batches(sample_indices, sizes, stsc, chunk_offsets, limits, budget)?;
   let batch_count = batches.len();
   let mut pending = stream::iter(batches.into_iter().map(|batch| async move {
      let data = reader
         .read_vec(batch.offset, batch.size)
         .await
         .map_err(SampleReadError::Fatal)?;
      if data.len() != batch.size {
         return Err(track_error(format!(
            "truncated sample batch at {}: expected {} bytes, read {}",
            batch.offset,
            batch.size,
            data.len()
         )));
      }
      let data = shared_region(data);

      let mut samples = Vec::new();
      samples
         .try_reserve(batch.samples.len())
         .map_err(|_| allocation_error("sample result allocation failed"))?;
      for sample in batch.samples {
         let end = sample
            .offset
            .checked_add(sample.size)
            .ok_or_else(|| fatal_error("sample slice overflow"))?;
         data
            .get(sample.offset..end)
            .ok_or_else(|| track_error("sample is outside its read batch"))?;
         samples.push((
            sample.sample_index,
            SampleData {
               bytes: Arc::clone(&data),
               range: sample.offset..end,
            },
         ));
      }
      Ok::<_, SampleReadError>(samples)
   }))
   .buffer_unordered(MAX_CONCURRENT_READS);
   let mut batch_results = Vec::new();
   batch_results
      .try_reserve_exact(batch_count)
      .map_err(|_| allocation_error("sample read result allocation failed"))?;
   while let Some(batch) = pending.next().await {
      batch_results.push(batch?);
   }

   let sample_count = batch_results.iter().try_fold(0usize, |count, batch| {
      count
         .checked_add(batch.len())
         .ok_or_else(|| fatal_error("sample result count overflow"))
   })?;
   let mut samples = HashMap::new();
   samples
      .try_reserve(sample_count)
      .map_err(|_| allocation_error("sample result map allocation failed"))?;
   for batch in batch_results {
      for (sample_index, data) in batch {
         if samples.insert(sample_index, data).is_some() {
            return Err(track_error("duplicate sample in coalesced read result"));
         }
      }
   }
   Ok(samples)
}

fn plan_read_batches(
   sample_indices: &[u32],
   sizes: &SampleSizes,
   stsc: &[StscEntry],
   chunk_offsets: &[u64],
   limits: SampleReadLimits,
   budget: &mut SampleReadBudget,
) -> SampleReadResult<Vec<ReadBatch>> {
   if sample_indices
      .windows(2)
      .any(|indices| indices[0] >= indices[1])
   {
      return Err(track_error("sample indices must be strictly increasing"));
   }
   let total_samples = budget
      .samples
      .checked_add(sample_indices.len())
      .ok_or_else(|| fatal_error("sample count overflow"))?;
   if total_samples > limits.max_samples {
      return Err(limit_error("too many samples"));
   }
   if sample_indices.is_empty() {
      return Ok(Vec::new());
   }

   let coalesce_gap = u64::try_from(limits.max_coalesce_gap_bytes)
      .map_err(|_| fatal_error("sample coalescing gap is too large"))?;

   // Phase 1: validate every requested sample and the cumulative logical
   // charge without allocating metadata proportional to the request.
   let mut locator = SampleLocator::new(sizes, stsc, chunk_offsets)
      .ok_or_else(|| track_error("could not locate samples"))?;
   let mut logical_bytes = 0usize;
   for sample_index in sample_indices.iter().copied() {
      let offset = locator
         .file_offset(sample_index)
         .ok_or_else(|| track_error(format!("could not locate sample {sample_index}")))?;
      let size = checked_sample_size(sample_index, sizes, limits)?;
      checked_sample_end(offset, size)?;
      logical_bytes = logical_bytes
         .checked_add(size)
         .ok_or_else(|| fatal_error("sample batch byte count overflow"))?;
      let total = budget
         .logical_bytes
         .checked_add(logical_bytes)
         .ok_or_else(|| fatal_error("sample batch byte count overflow"))?;
      if total > limits.max_logical_bytes {
         return Err(limit_error(format!(
            "sample batch is too large: {total} bytes"
         )));
      }
   }
   let total_logical = budget
      .logical_bytes
      .checked_add(logical_bytes)
      .ok_or_else(|| fatal_error("sample batch byte count overflow"))?;

   // Phase 2: one compact descriptor per already-counted sample is the
   // bounded exception needed to restore physical order without an O(n²)
   // locator walk. Physical planning below performs no further proportional
   // allocation.
   let mut located_samples = allocate_located_samples(sample_indices.len())?;
   let mut locator = SampleLocator::new(sizes, stsc, chunk_offsets)
      .ok_or_else(|| track_error("could not locate samples"))?;
   for sample_index in sample_indices.iter().copied() {
      let offset = locator
         .file_offset(sample_index)
         .ok_or_else(|| track_error(format!("could not locate sample {sample_index}")))?;
      let size = checked_sample_size(sample_index, sizes, limits)?;
      checked_sample_end(offset, size)?;
      located_samples.push(LocatedSample {
         sample_index,
         offset,
         size,
      });
   }
   located_samples.sort_unstable_by_key(|sample| (sample.offset, sample.sample_index));
   let (total_physical, total_regions, region_count) =
      validate_physical_plan(&located_samples, coalesce_gap, limits, budget)?;

   // Phase 3: all seven limits are known to pass. Batch and slice metadata can
   // now be allocated fallibly; the shared counters remain unchanged until
   // construction is complete.
   let mut reads: Vec<ReadBatch> = Vec::new();
   reads
      .try_reserve_exact(region_count)
      .map_err(|_| allocation_error("sample read batch allocation failed"))?;
   for sample in located_samples {
      if let Some(batch) = reads.last_mut()
         && let Some(merged_size) =
            merged_region_size(batch.offset, batch.size, &sample, coalesce_gap, limits)?
      {
         batch
            .samples
            .try_reserve(1)
            .map_err(|_| allocation_error("sample slice allocation failed"))?;
         batch.samples.push(SampleSlice {
            sample_index: sample.sample_index,
            offset: sample
               .offset
               .checked_sub(batch.offset)
               .and_then(|offset| usize::try_from(offset).ok())
               .ok_or_else(|| fatal_error("sample offset is too large"))?,
            size: sample.size,
         });
         batch.size = merged_size;
         continue;
      }
      let mut samples = Vec::new();
      samples
         .try_reserve_exact(1)
         .map_err(|_| allocation_error("sample slice allocation failed"))?;
      samples.push(SampleSlice {
         sample_index: sample.sample_index,
         offset: 0,
         size: sample.size,
      });
      reads.push(ReadBatch {
         offset: sample.offset,
         size: sample.size,
         samples,
      });
   }

   budget.samples = total_samples;
   budget.logical_bytes = total_logical;
   budget.physical_bytes = total_physical;
   budget.regions = total_regions;
   Ok(reads)
}

fn validate_physical_plan(
   samples: &[LocatedSample],
   coalesce_gap: u64,
   limits: SampleReadLimits,
   budget: &SampleReadBudget,
) -> SampleReadResult<(usize, usize, usize)> {
   let mut physical_bytes = 0usize;
   let mut regions = 0usize;
   let mut current_region: Option<(u64, usize)> = None;
   // Phase 1 already rejected any sample above max_sample_bytes, and every
   // caller asserts max_sample_bytes <= max_region_bytes, so a lone sample
   // always fits its region and only coalescing can reach the region ceiling.
   for sample in samples {
      if let Some((region_offset, region_size)) = current_region {
         if let Some(merged_size) =
            merged_region_size(region_offset, region_size, sample, coalesce_gap, limits)?
         {
            current_region = Some((region_offset, merged_size));
            continue;
         }
         validate_region_charge(
            region_size,
            budget,
            limits,
            &mut physical_bytes,
            &mut regions,
         )?;
      }
      current_region = Some((sample.offset, sample.size));
   }
   if let Some((_, region_size)) = current_region {
      validate_region_charge(
         region_size,
         budget,
         limits,
         &mut physical_bytes,
         &mut regions,
      )?;
   }

   let total_physical = budget
      .physical_bytes
      .checked_add(physical_bytes)
      .ok_or_else(|| fatal_error("coalesced sample reads are too large"))?;
   let total_regions = budget
      .regions
      .checked_add(regions)
      .ok_or_else(|| fatal_error("too many sample read batches"))?;
   Ok((total_physical, total_regions, regions))
}

fn merged_region_size(
   region_offset: u64,
   region_size: usize,
   sample: &LocatedSample,
   coalesce_gap: u64,
   limits: SampleReadLimits,
) -> SampleReadResult<Option<usize>> {
   let region_end = checked_region_end(region_offset, region_size)?;
   if sample.offset < region_end {
      return Err(track_error("overlapping samples"));
   }
   let sample_end = checked_sample_end(sample.offset, sample.size)?;
   let merged_size = sample_end
      .checked_sub(region_offset)
      .and_then(|size| usize::try_from(size).ok())
      .ok_or_else(|| fatal_error("sample batch size overflow"))?;
   let gap = sample
      .offset
      .checked_sub(region_end)
      .ok_or_else(|| track_error("overlapping samples"))?;
   Ok((gap <= coalesce_gap && merged_size <= limits.max_region_bytes).then_some(merged_size))
}

fn validate_region_charge(
   region_size: usize,
   budget: &SampleReadBudget,
   limits: SampleReadLimits,
   physical_bytes: &mut usize,
   regions: &mut usize,
) -> SampleReadResult<()> {
   let next_physical = physical_bytes
      .checked_add(region_size)
      .ok_or_else(|| fatal_error("coalesced sample reads are too large"))?;
   let total_physical = budget
      .physical_bytes
      .checked_add(next_physical)
      .ok_or_else(|| fatal_error("coalesced sample reads are too large"))?;
   if total_physical > limits.max_physical_bytes {
      return Err(limit_error("coalesced sample reads are too large"));
   }
   let next_regions = regions
      .checked_add(1)
      .ok_or_else(|| fatal_error("too many sample read batches"))?;
   let total_regions = budget
      .regions
      .checked_add(next_regions)
      .ok_or_else(|| fatal_error("too many sample read batches"))?;
   if total_regions > limits.max_regions {
      return Err(limit_error("too many sample read batches"));
   }
   *physical_bytes = next_physical;
   *regions = next_regions;
   Ok(())
}

fn checked_sample_size(
   sample_index: u32,
   sizes: &SampleSizes,
   limits: SampleReadLimits,
) -> SampleReadResult<usize> {
   let size = usize::try_from(
      sample_size(sample_index, sizes)
         .ok_or_else(|| track_error(format!("could not read sample {sample_index} size")))?,
   )
   .map_err(|_| fatal_error("sample size exceeds usize"))?;
   if size == 0 {
      return Err(track_error("invalid sample size: 0 bytes"));
   }
   if size > limits.max_sample_bytes {
      return Err(track_error(format!("invalid sample size: {size} bytes")));
   }
   Ok(size)
}

fn checked_sample_end(offset: u64, size: usize) -> SampleReadResult<u64> {
   offset
      .checked_add(u64::try_from(size).map_err(|_| fatal_error("sample size exceeds u64"))?)
      .ok_or_else(|| fatal_error("sample offset overflow"))
}

fn checked_region_end(offset: u64, size: usize) -> SampleReadResult<u64> {
   offset
      .checked_add(u64::try_from(size).map_err(|_| fatal_error("sample batch size exceeds u64"))?)
      .ok_or_else(|| fatal_error("sample batch offset overflow"))
}

#[cfg(test)]
mod tests {
   use super::*;
   use async_trait::async_trait;
   use std::sync::Mutex;
   use std::sync::atomic::{AtomicUsize, Ordering};

   const TEST_LIMITS: SampleReadLimits = SampleReadLimits {
      max_samples: 16,
      max_sample_bytes: 1_024,
      max_logical_bytes: 4_096,
      max_physical_bytes: 8_192,
      max_regions: 4,
      max_region_bytes: 4_096,
      max_coalesce_gap_bytes: 64,
   };

   fn fixed_samples(sample_count: u32, fixed_size: u32) -> SampleSizes {
      SampleSizes::fixed(sample_count, fixed_size).expect("test sample size must be non-zero")
   }

   fn one_sample_per_chunk() -> [StscEntry; 1] {
      [StscEntry {
         first_chunk: 1,
         samples_per_chunk: 1,
         sample_description_index: 1,
      }]
   }

   fn offsets(count: u32, stride: u64) -> Vec<u64> {
      (0..count).map(|index| u64::from(index) * stride).collect()
   }

   fn plan(
      sample_indices: &[u32],
      sizes: &SampleSizes,
      chunk_offsets: &[u64],
      limits: SampleReadLimits,
      budget: &mut SampleReadBudget,
   ) -> Result<Vec<ReadBatch>> {
      plan_read_batches(
         sample_indices,
         sizes,
         &one_sample_per_chunk(),
         chunk_offsets,
         limits,
         budget,
      )
      .map_err(SampleReadError::into_media_error)
   }

   fn assert_limit(error: SampleReadError, expected_reason: &str) {
      assert!(
         matches!(&error, SampleReadError::Limit(reason) if reason == expected_reason),
         "expected limit error {expected_reason:?}, got {error:?}"
      );
      assert!(matches!(
         error.into_media_error(),
         MediaParserError::Other(reason) if reason == expected_reason
      ));
   }

   #[test]
   fn max_samples_failure_is_a_limit_error() {
      let mut budget = SampleReadBudget::default();
      let error = plan_read_batches(
         &[1],
         &fixed_samples(1, 1),
         &one_sample_per_chunk(),
         &[0],
         SampleReadLimits {
            max_samples: 0,
            ..TEST_LIMITS
         },
         &mut budget,
      )
      .expect_err("sample count above its configured limit must fail");

      assert_limit(error, "too many samples");
   }

   #[test]
   fn max_sample_bytes_failure_is_a_track_error() {
      let mut budget = SampleReadBudget::default();
      let error = plan_read_batches(
         &[1],
         &fixed_samples(1, 4),
         &one_sample_per_chunk(),
         &[0],
         SampleReadLimits {
            max_sample_bytes: 3,
            ..TEST_LIMITS
         },
         &mut budget,
      )
      .expect_err("sample size above its configured limit must fail");

      assert!(matches!(
         error,
         SampleReadError::Track(MediaParserError::InvalidFormat(reason))
            if reason == "invalid sample size: 4 bytes"
      ));
   }

   #[test]
   fn max_logical_bytes_failure_is_a_limit_error() {
      let mut budget = SampleReadBudget::default();
      let error = plan_read_batches(
         &[1],
         &fixed_samples(1, 4),
         &one_sample_per_chunk(),
         &[0],
         SampleReadLimits {
            max_logical_bytes: 3,
            ..TEST_LIMITS
         },
         &mut budget,
      )
      .expect_err("logical bytes above their configured limit must fail");

      assert_limit(error, "sample batch is too large: 4 bytes");
   }

   #[test]
   fn max_physical_bytes_failure_is_a_limit_error() {
      let mut budget = SampleReadBudget::default();
      let error = plan_read_batches(
         &[1],
         &fixed_samples(1, 4),
         &one_sample_per_chunk(),
         &[0],
         SampleReadLimits {
            max_physical_bytes: 3,
            ..TEST_LIMITS
         },
         &mut budget,
      )
      .expect_err("physical bytes above their configured limit must fail");

      assert_limit(error, "coalesced sample reads are too large");
   }

   #[test]
   fn max_region_bytes_partitions_contiguous_samples() {
      let mut budget = SampleReadBudget::default();
      let batches = plan(
         &[1, 2],
         &fixed_samples(2, 1_024),
         &[0, 1_024],
         SampleReadLimits {
            max_region_bytes: 1_024,
            ..TEST_LIMITS
         },
         &mut budget,
      )
      .expect("samples within the region ceiling must plan as separate regions");

      assert_eq!(
         batches.iter().map(|batch| batch.size).collect::<Vec<_>>(),
         vec![1_024, 1_024]
      );
      assert_eq!(budget.regions, 2);
   }

   #[test]
   fn max_regions_failure_is_a_limit_error() {
      let mut budget = SampleReadBudget::default();
      let error = plan_read_batches(
         &[1],
         &fixed_samples(1, 4),
         &one_sample_per_chunk(),
         &[0],
         SampleReadLimits {
            max_regions: 0,
            ..TEST_LIMITS
         },
         &mut budget,
      )
      .expect_err("region count above its configured limit must fail");

      assert_limit(error, "too many sample read batches");
   }

   #[test]
   fn classified_invalid_sample_planning_is_track_local() {
      let mut budget = SampleReadBudget::default();
      let error = plan_read_batches(
         &[1],
         &fixed_samples(1, 1),
         &[],
         &[0],
         TEST_LIMITS,
         &mut budget,
      )
      .expect_err("invalid tables are local to the selected track");

      assert!(matches!(error, SampleReadError::Track(_)));
   }

   #[test]
   fn sample_end_arithmetic_overflow_is_fatal() {
      let error = checked_sample_end(u64::MAX, 1).expect_err("sample end must not wrap");

      assert!(matches!(
         error,
         SampleReadError::Fatal(MediaParserError::InvalidFormat(reason))
            if reason == "sample offset overflow"
      ));
   }

   #[test]
   fn allocation_failure_is_a_fatal_resource_error() {
      let error = allocation_error("sample planning allocation failed");

      assert!(matches!(
         error,
         SampleReadError::Fatal(MediaParserError::Other(reason))
            if reason == "sample planning allocation failed"
      ));
   }

   #[test]
   fn accepts_the_exact_sample_count_limit() {
      let sizes = fixed_samples(16, 1);
      let batches = plan(
         &(1..=16).collect::<Vec<_>>(),
         &sizes,
         &offsets(16, 1),
         TEST_LIMITS,
         &mut SampleReadBudget::default(),
      )
      .unwrap();

      assert_eq!(
         batches
            .iter()
            .map(|batch| batch.samples.len())
            .sum::<usize>(),
         16
      );
   }

   #[test]
   fn rejects_one_over_the_sample_count_limit() {
      let sizes = fixed_samples(17, 1);
      let error = plan(
         &(1..=17).collect::<Vec<_>>(),
         &sizes,
         &offsets(17, 1),
         TEST_LIMITS,
         &mut SampleReadBudget::default(),
      )
      .unwrap_err();

      assert!(error.to_string().contains("too many samples"));
   }

   #[test]
   fn rejects_one_over_the_individual_sample_byte_limit() {
      let error = plan(
         &[1],
         &fixed_samples(1, 1_025),
         &[0],
         TEST_LIMITS,
         &mut SampleReadBudget::default(),
      )
      .unwrap_err();

      assert!(error.to_string().contains("invalid sample size"));
   }

   /// Four contiguous 1 KiB samples sit exactly on three ceilings at once:
   /// `max_sample_bytes` per sample, and `max_logical_bytes` and
   /// `max_region_bytes` for the coalesced batch. One accepting plan covers
   /// all three; the rejecting cases below separate them by raising whichever
   /// ceiling is not under test.
   #[test]
   fn accepts_the_exact_aggregate_logical_byte_limit() {
      let batches = plan(
         &[1, 2, 3, 4],
         &fixed_samples(4, 1_024),
         &[0, 1_024, 2_048, 3_072],
         TEST_LIMITS,
         &mut SampleReadBudget::default(),
      )
      .unwrap();

      assert_eq!(batches[0].size, 4_096);
   }

   #[test]
   fn rejects_one_over_the_aggregate_logical_byte_limit() {
      let sizes = SampleSizes::variable(vec![1_024, 1_024, 1_024, 1_024, 1]).unwrap();
      let error = plan(
         &[1, 2, 3, 4, 5],
         &sizes,
         &[0, 1_024, 2_048, 3_072, 4_096],
         SampleReadLimits {
            max_region_bytes: 4_097,
            ..TEST_LIMITS
         },
         &mut SampleReadBudget::default(),
      )
      .unwrap_err();

      assert!(error.to_string().contains("sample batch is too large"));
   }

   #[test]
   fn accepts_the_exact_aggregate_physical_byte_limit_including_gaps() {
      let batches = plan(
         &[1, 2],
         &fixed_samples(2, 1_024),
         &[0, 7_168],
         SampleReadLimits {
            max_region_bytes: 8_192,
            max_coalesce_gap_bytes: 6_144,
            ..TEST_LIMITS
         },
         &mut SampleReadBudget::default(),
      )
      .unwrap();

      assert_eq!(batches[0].size, 8_192);
   }

   #[test]
   fn rejects_one_over_the_aggregate_physical_byte_limit_including_gaps() {
      let error = plan(
         &[1, 2],
         &fixed_samples(2, 1_024),
         &[0, 7_169],
         SampleReadLimits {
            max_region_bytes: 8_193,
            max_coalesce_gap_bytes: 6_145,
            ..TEST_LIMITS
         },
         &mut SampleReadBudget::default(),
      )
      .unwrap_err();

      assert!(
         error
            .to_string()
            .contains("coalesced sample reads are too large")
      );
   }

   #[test]
   fn accepts_the_exact_region_count_limit() {
      let batches = plan(
         &[1, 2, 3, 4],
         &fixed_samples(4, 1),
         &[0, 100, 200, 300],
         TEST_LIMITS,
         &mut SampleReadBudget::default(),
      )
      .unwrap();

      assert_eq!(batches.len(), 4);
   }

   #[test]
   fn rejects_one_over_the_region_count_limit() {
      let error = plan(
         &[1, 2, 3, 4, 5],
         &fixed_samples(5, 1),
         &[0, 100, 200, 300, 400],
         TEST_LIMITS,
         &mut SampleReadBudget::default(),
      )
      .unwrap_err();

      assert!(error.to_string().contains("too many sample read batches"));
   }

   #[test]
   fn splits_at_one_over_the_individual_region_byte_limit() {
      let sizes = SampleSizes::variable(vec![1_024, 1_024, 1_024, 1_024, 1]).unwrap();
      let batches = plan(
         &[1, 2, 3, 4, 5],
         &sizes,
         &[0, 1_024, 2_048, 3_072, 4_096],
         SampleReadLimits {
            max_logical_bytes: 4_097,
            ..TEST_LIMITS
         },
         &mut SampleReadBudget::default(),
      )
      .unwrap();

      assert_eq!(
         batches.iter().map(|batch| batch.size).collect::<Vec<_>>(),
         vec![4_096, 1]
      );
   }

   #[test]
   fn coalesces_at_the_exact_gap_limit() {
      let sizes = fixed_samples(2, 4);
      let second_offset = 100 + 4 + u64::try_from(TEST_LIMITS.max_coalesce_gap_bytes).unwrap();
      let batches = plan(
         &[1, 2],
         &sizes,
         &[100, second_offset],
         TEST_LIMITS,
         &mut SampleReadBudget::default(),
      )
      .expect("plan should coalesce a sample at the gap limit");

      assert_eq!(batches.len(), 1);
      assert_eq!(batches[0].offset, 100);
      assert_eq!(batches[0].size, 8 + TEST_LIMITS.max_coalesce_gap_bytes);
      assert_eq!(batches[0].samples.len(), 2);
      assert_eq!(
         batches[0].samples[1].offset,
         4 + TEST_LIMITS.max_coalesce_gap_bytes
      );
   }

   #[test]
   fn splits_at_one_over_the_gap_limit() {
      let sizes = fixed_samples(2, 4);
      let second_offset = 4 + u64::try_from(TEST_LIMITS.max_coalesce_gap_bytes).unwrap() + 1;
      let batches = plan(
         &[1, 2],
         &sizes,
         &[0, second_offset],
         TEST_LIMITS,
         &mut SampleReadBudget::default(),
      )
      .expect("plan should split distant samples");

      assert_eq!(batches.len(), 2);
      assert_eq!(batches[0].size, 4);
      assert_eq!(batches[1].offset, second_offset);
   }

   #[test]
   fn rejects_overlapping_samples() {
      let sizes = fixed_samples(2, 4);
      let error = plan(
         &[1, 2],
         &sizes,
         &[0, 3],
         TEST_LIMITS,
         &mut SampleReadBudget::default(),
      )
      .expect_err("overlapping samples must be rejected");

      assert!(matches!(
         error,
         MediaParserError::InvalidFormat(message) if message == "overlapping samples"
      ));
   }

   #[test]
   fn rejects_sample_indices_that_are_not_strictly_increasing() {
      for sample_indices in [[1, 1], [2, 1]] {
         let error = plan(
            &sample_indices,
            &fixed_samples(2, 4),
            &[0, 4],
            TEST_LIMITS,
            &mut SampleReadBudget::default(),
         )
         .unwrap_err();

         assert!(
            error.to_string().contains("strictly increasing"),
            "{sample_indices:?} must be rejected"
         );
      }
   }

   #[test]
   fn rejects_zero_size_samples_that_require_payloads() {
      let error = plan(
         &[1],
         &SampleSizes::variable(vec![0]).unwrap(),
         &[0],
         TEST_LIMITS,
         &mut SampleReadBudget::default(),
      )
      .unwrap_err();

      assert!(error.to_string().contains("invalid sample size: 0 bytes"));
   }

   #[test]
   fn successful_charges_remain_in_the_shared_budget_across_calls() {
      for limits in [
         SampleReadLimits {
            max_samples: 1,
            ..TEST_LIMITS
         },
         SampleReadLimits {
            max_logical_bytes: 4,
            ..TEST_LIMITS
         },
         SampleReadLimits {
            max_physical_bytes: 4,
            ..TEST_LIMITS
         },
         SampleReadLimits {
            max_regions: 1,
            ..TEST_LIMITS
         },
      ] {
         let mut budget = SampleReadBudget::default();
         let first = plan(&[1], &fixed_samples(1, 4), &[0], limits, &mut budget).unwrap();
         drop(first);
         let charged = (
            budget.samples,
            budget.logical_bytes,
            budget.physical_bytes,
            budget.regions,
         );

         plan(&[1], &fixed_samples(1, 4), &[0], limits, &mut budget)
            .expect_err("the second call must observe the first call's charges");

         assert_eq!(
            (
               budget.samples,
               budget.logical_bytes,
               budget.physical_bytes,
               budget.regions
            ),
            charged,
            "a rejected call must not refund prior charges"
         );
      }
   }

   #[test]
   fn representative_multitrack_subtitle_fixture_reaches_the_aggregate_region_ceiling() {
      const SAMPLES_PER_TRACK: u32 = 2_050;
      const COALESCE_GAP: usize = 64 * 1_024;
      let limits = SampleReadLimits {
         max_samples: 4_100,
         max_sample_bytes: 1,
         max_logical_bytes: 4_100,
         max_physical_bytes: 4_100,
         max_regions: 4_096,
         max_region_bytes: 1,
         max_coalesce_gap_bytes: COALESCE_GAP,
      };
      let sample_indices = (1..=SAMPLES_PER_TRACK).collect::<Vec<_>>();
      // Each one-byte subtitle sample is separated by more than the 64 KiB
      // coalescing gap, representing intervening audio/video payload in an
      // interleaved mux while keeping logical and physical subtitle bytes low.
      let chunk_offsets = offsets(SAMPLES_PER_TRACK, COALESCE_GAP as u64 + 2);
      let sizes = fixed_samples(SAMPLES_PER_TRACK, 1);
      let mut request_budget = SampleReadBudget::default();

      let first_track = plan(
         &sample_indices,
         &sizes,
         &chunk_offsets,
         limits,
         &mut request_budget,
      )
      .expect("the first track is below every aggregate request ceiling");
      assert_eq!(first_track.len(), SAMPLES_PER_TRACK as usize);
      drop(first_track);

      let error = plan(
         &sample_indices,
         &sizes,
         &chunk_offsets,
         limits,
         &mut request_budget,
      )
      .expect_err("the second track takes the shared request over 4,096 regions");

      assert!(error.to_string().contains("too many sample read batches"));
      assert_eq!(request_budget.regions, SAMPLES_PER_TRACK as usize);
   }

   struct RecordingReader {
      bytes: Vec<u8>,
      reads: AtomicUsize,
      regions: Mutex<Vec<(u64, usize)>>,
   }

   #[async_trait]
   impl StreamReader for RecordingReader {
      async fn read_at(&self, offset: u64, buf: &mut [u8]) -> Result<usize> {
         self.reads.fetch_add(1, Ordering::Relaxed);
         self.regions.lock().unwrap().push((offset, buf.len()));
         let start = usize::try_from(offset)
            .unwrap_or(usize::MAX)
            .min(self.bytes.len());
         let read = buf.len().min(self.bytes.len() - start);
         buf[..read].copy_from_slice(&self.bytes[start..start + read]);
         Ok(read)
      }

      async fn size(&self) -> Result<u64> {
         Ok(self.bytes.len() as u64)
      }
   }

   #[tokio::test]
   async fn physical_and_region_failures_do_not_read_or_charge_the_budget() {
      let cases = [
         (
            vec![0; 12],
            vec![0, 8],
            SampleReadLimits {
               max_physical_bytes: 11,
               ..TEST_LIMITS
            },
         ),
         (
            vec![0; 104],
            vec![0, 100],
            SampleReadLimits {
               max_regions: 1,
               ..TEST_LIMITS
            },
         ),
      ];

      for (bytes, offsets, limits) in cases {
         let reader = RecordingReader {
            bytes,
            reads: AtomicUsize::new(0),
            regions: Mutex::new(Vec::new()),
         };
         let mut budget = SampleReadBudget::default();

         read_samples_coalesced(
            &reader,
            &[1, 2],
            &fixed_samples(2, 4),
            &one_sample_per_chunk(),
            &offsets,
            limits,
            &mut budget,
         )
         .await
         .expect_err("planning must reject the physical or region limit");

         assert_eq!(reader.reads.load(Ordering::Relaxed), 0);
         assert!(reader.regions.lock().unwrap().is_empty());
         assert_eq!(
            (
               budget.samples,
               budget.logical_bytes,
               budget.physical_bytes,
               budget.regions
            ),
            (0, 0, 0, 0)
         );
      }
   }

   #[tokio::test]
   async fn merged_samples_share_the_same_region_storage() {
      let reader = RecordingReader {
         bytes: (0..16).collect(),
         reads: AtomicUsize::new(0),
         regions: Mutex::new(Vec::new()),
      };
      let mut budget = SampleReadBudget::default();
      let samples = read_samples_coalesced(
         &reader,
         &[1, 2],
         &fixed_samples(2, 4),
         &one_sample_per_chunk(),
         &[0, 8],
         TEST_LIMITS,
         &mut budget,
      )
      .await
      .unwrap();

      assert!(Arc::ptr_eq(&samples[&1].bytes, &samples[&2].bytes));
      assert_eq!(samples[&1].as_slice(), &[0, 1, 2, 3]);
      assert_eq!(samples[&2].as_slice(), &[8, 9, 10, 11]);
      assert_eq!(reader.reads.load(Ordering::Relaxed), 1);
   }

   #[tokio::test]
   async fn reads_disjoint_samples_whose_physical_offsets_decrease() {
      let mut bytes = vec![0; 104];
      bytes[0..4].copy_from_slice(&[2, 2, 2, 2]);
      bytes[100..104].copy_from_slice(&[1, 1, 1, 1]);
      let reader = RecordingReader {
         bytes,
         reads: AtomicUsize::new(0),
         regions: Mutex::new(Vec::new()),
      };
      let mut budget = SampleReadBudget::default();

      let samples = read_samples_coalesced(
         &reader,
         &[1, 2],
         &fixed_samples(2, 4),
         &one_sample_per_chunk(),
         &[100, 0],
         TEST_LIMITS,
         &mut budget,
      )
      .await
      .expect("disjoint physical regions may appear in decreasing sample-index order");

      assert_eq!(samples[&1].as_slice(), &[1, 1, 1, 1]);
      assert_eq!(samples[&2].as_slice(), &[2, 2, 2, 2]);
      let mut regions = reader.regions.lock().unwrap().clone();
      regions.sort_unstable();
      assert_eq!(regions, vec![(0, 4), (100, 4)]);
   }

   #[tokio::test]
   async fn rejects_a_truncated_coalesced_read() {
      let reader = RecordingReader {
         bytes: vec![1, 2, 3],
         reads: AtomicUsize::new(0),
         regions: Mutex::new(Vec::new()),
      };
      let sizes = fixed_samples(1, 4);
      let mut budget = SampleReadBudget::default();
      let error = read_samples_coalesced(
         &reader,
         &[1],
         &sizes,
         &one_sample_per_chunk(),
         &[0],
         TEST_LIMITS,
         &mut budget,
      )
      .await
      .expect_err("short reads must not produce partial samples");

      assert!(matches!(
         error,
         MediaParserError::InvalidFormat(message) if message.contains("truncated sample batch")
      ));
      assert_eq!(
         (
            budget.samples,
            budget.logical_bytes,
            budget.physical_bytes,
            budget.regions
         ),
         (1, 4, 4, 1),
         "I/O failure must not refund a successful plan"
      );
   }
}

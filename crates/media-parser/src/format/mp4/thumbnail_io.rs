//! Bounded, coalesced sample reads for MP4 thumbnail extraction.

use super::atoms::{SampleLocator, SampleSizes, StscEntry, sample_size};
use crate::errors::{MediaParserError, Result};
use crate::stream::StreamReader;
use futures::stream::{self, StreamExt, TryStreamExt};
use std::collections::HashMap;

const MAX_FRAME_BYTES: usize = 64 * 1024 * 1024;
const MAX_THUMBNAIL_BATCH_BYTES: usize = 128 * 1024 * 1024;
const MAX_COALESCED_READ_BYTES: usize = 64 * 1024 * 1024;
pub(super) const MAX_SAMPLES_PER_THUMBNAIL_BATCH: usize = 16_384;
const READ_COALESCE_GAP_BYTES: u64 = 32 * 1024;
const MAX_CONCURRENT_READS: usize = 4;

#[derive(Debug)]
struct SampleSlice {
   sample_index: u32,
   offset: usize,
   size: usize,
}

#[derive(Debug)]
struct ReadBatch {
   offset: u64,
   size: usize,
   samples: Vec<SampleSlice>,
}

pub(super) async fn read_samples_coalesced(
   reader: &dyn StreamReader,
   sample_indices: &[u32],
   sizes: &SampleSizes,
   stsc: &[StscEntry],
   chunk_offsets: &[u64],
) -> Result<HashMap<u32, Vec<u8>>> {
   let batches = plan_read_batches(sample_indices, sizes, stsc, chunk_offsets)?;
   let batch_results = stream::iter(batches.into_iter().map(|batch| async move {
      let data = reader.read_vec(batch.offset, batch.size).await?;
      if data.len() != batch.size {
         return Err(MediaParserError::InvalidFormat(format!(
            "truncated sample batch at {}: expected {} bytes, read {}",
            batch.offset,
            batch.size,
            data.len()
         )));
      }

      let mut samples = Vec::new();
      samples
         .try_reserve(batch.samples.len())
         .map_err(|_| MediaParserError::InvalidFormat("sample batch is too large".to_string()))?;
      for sample in batch.samples {
         let end = sample
            .offset
            .checked_add(sample.size)
            .ok_or_else(|| MediaParserError::InvalidFormat("sample slice overflow".to_string()))?;
         let source = data.get(sample.offset..end).ok_or_else(|| {
            MediaParserError::InvalidFormat("sample is outside its read batch".to_string())
         })?;
         let mut bytes = Vec::new();
         bytes
            .try_reserve_exact(source.len())
            .map_err(|_| MediaParserError::InvalidFormat("sample is too large".to_string()))?;
         bytes.extend_from_slice(source);
         samples.push((sample.sample_index, bytes));
      }
      Ok::<_, MediaParserError>(samples)
   }))
   .buffer_unordered(MAX_CONCURRENT_READS)
   .try_collect::<Vec<_>>()
   .await?;

   let sample_count = batch_results.iter().map(Vec::len).sum();
   let mut samples = HashMap::new();
   samples
      .try_reserve(sample_count)
      .map_err(|_| MediaParserError::InvalidFormat("too many thumbnail samples".to_string()))?;
   for batch in batch_results {
      for (sample_index, data) in batch {
         samples.insert(sample_index, data);
      }
   }
   Ok(samples)
}

fn plan_read_batches(
   sample_indices: &[u32],
   sizes: &SampleSizes,
   stsc: &[StscEntry],
   chunk_offsets: &[u64],
) -> Result<Vec<ReadBatch>> {
   if sample_indices.len() > MAX_SAMPLES_PER_THUMBNAIL_BATCH {
      return Err(MediaParserError::InvalidFormat(
         "too many thumbnail samples".to_string(),
      ));
   }
   let mut unique_samples = Vec::new();
   unique_samples
      .try_reserve(sample_indices.len())
      .map_err(|_| MediaParserError::InvalidFormat("too many thumbnail samples".to_string()))?;
   unique_samples.extend_from_slice(sample_indices);
   unique_samples.sort_unstable();
   unique_samples.dedup();

   let mut reads = Vec::new();
   reads
      .try_reserve(unique_samples.len())
      .map_err(|_| MediaParserError::InvalidFormat("too many thumbnail samples".to_string()))?;
   // unique_samples is sorted, so a single forward cursor locates every
   // sample in O(samples + stsc entries) instead of one stsc walk per sample.
   let mut locator = SampleLocator::new(sizes, stsc, chunk_offsets)
      .ok_or_else(|| MediaParserError::InvalidFormat("could not locate samples".to_string()))?;
   let mut total_sample_bytes = 0usize;
   for sample_index in unique_samples {
      let offset = locator.file_offset(sample_index).ok_or_else(|| {
         MediaParserError::InvalidFormat(format!("could not locate sample {sample_index}"))
      })?;
      let size = usize::try_from(sample_size(sample_index, sizes).ok_or_else(|| {
         MediaParserError::InvalidFormat(format!("could not read sample {sample_index} size"))
      })?)
      .map_err(|_| MediaParserError::InvalidFormat("sample is too large".to_string()))?;
      if size == 0 || size > MAX_FRAME_BYTES {
         return Err(MediaParserError::InvalidFormat(format!(
            "invalid thumbnail sample size: {size} bytes"
         )));
      }
      total_sample_bytes = total_sample_bytes.checked_add(size).ok_or_else(|| {
         MediaParserError::InvalidFormat("thumbnail sample batch is too large".to_string())
      })?;
      if total_sample_bytes > MAX_THUMBNAIL_BATCH_BYTES {
         return Err(MediaParserError::InvalidFormat(format!(
            "thumbnail sample batch is too large: {total_sample_bytes} bytes"
         )));
      }
      reads.push((offset, size, sample_index));
   }
   reads.sort_unstable_by_key(|(offset, _, sample_index)| (*offset, *sample_index));

   let mut batches: Vec<ReadBatch> = Vec::new();
   batches.try_reserve(reads.len()).map_err(|_| {
      MediaParserError::InvalidFormat("too many thumbnail read batches".to_string())
   })?;
   for (offset, size, sample_index) in reads {
      let sample_end = offset
         .checked_add(
            u64::try_from(size)
               .map_err(|_| MediaParserError::InvalidFormat("sample is too large".to_string()))?,
         )
         .ok_or_else(|| MediaParserError::InvalidFormat("sample offset overflow".to_string()))?;
      if let Some(batch) = batches.last_mut() {
         let batch_end = batch
            .offset
            .checked_add(u64::try_from(batch.size).map_err(|_| {
               MediaParserError::InvalidFormat("sample batch is too large".to_string())
            })?)
            .ok_or_else(|| {
               MediaParserError::InvalidFormat("sample batch offset overflow".to_string())
            })?;
         if offset < batch_end {
            return Err(MediaParserError::InvalidFormat(
               "overlapping video samples".to_string(),
            ));
         }
         let merged_size = usize::try_from(sample_end - batch.offset).map_err(|_| {
            MediaParserError::InvalidFormat("sample batch is too large".to_string())
         })?;
         if offset - batch_end <= READ_COALESCE_GAP_BYTES && merged_size <= MAX_COALESCED_READ_BYTES
         {
            batch.samples.push(SampleSlice {
               sample_index,
               offset: usize::try_from(offset - batch.offset).map_err(|_| {
                  MediaParserError::InvalidFormat("sample offset is too large".to_string())
               })?,
               size,
            });
            batch.size = merged_size;
            continue;
         }
      }
      batches.push(ReadBatch {
         offset,
         size,
         samples: vec![SampleSlice {
            sample_index,
            offset: 0,
            size,
         }],
      });
   }
   let total_read_bytes = batches
      .iter()
      .try_fold(0usize, |total, batch| total.checked_add(batch.size));
   if total_read_bytes.is_none_or(|total| total > MAX_THUMBNAIL_BATCH_BYTES) {
      return Err(MediaParserError::InvalidFormat(
         "coalesced thumbnail reads are too large".to_string(),
      ));
   }
   Ok(batches)
}

#[cfg(test)]
mod tests {
   use super::*;
   use async_trait::async_trait;

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

   #[test]
   fn coalesces_samples_within_the_gap_limit() {
      let sizes = fixed_samples(2, 4);
      let second_offset = 100 + 4 + READ_COALESCE_GAP_BYTES;
      let batches = plan_read_batches(
         &[1, 2],
         &sizes,
         &one_sample_per_chunk(),
         &[100, second_offset],
      )
      .expect("plan should coalesce a sample at the gap limit");

      assert_eq!(batches.len(), 1);
      assert_eq!(batches[0].offset, 100);
      assert_eq!(batches[0].size, 8 + READ_COALESCE_GAP_BYTES as usize);
      assert_eq!(batches[0].samples.len(), 2);
      assert_eq!(
         batches[0].samples[1].offset,
         4 + READ_COALESCE_GAP_BYTES as usize
      );
   }

   #[test]
   fn splits_samples_beyond_the_gap_limit() {
      let sizes = fixed_samples(2, 4);
      let second_offset = 4 + READ_COALESCE_GAP_BYTES + 1;
      let batches = plan_read_batches(
         &[1, 2],
         &sizes,
         &one_sample_per_chunk(),
         &[0, second_offset],
      )
      .expect("plan should split distant samples");

      assert_eq!(batches.len(), 2);
      assert_eq!(batches[0].size, 4);
      assert_eq!(batches[1].offset, second_offset);
   }

   #[test]
   fn rejects_overlapping_samples() {
      let sizes = fixed_samples(2, 4);
      let error = plan_read_batches(&[1, 2], &sizes, &one_sample_per_chunk(), &[0, 3])
         .expect_err("overlapping samples must be rejected");

      assert!(matches!(
         error,
         MediaParserError::InvalidFormat(message) if message == "overlapping video samples"
      ));
   }

   #[test]
   fn rejects_a_sample_larger_than_the_frame_limit() {
      let sizes = fixed_samples(1, u32::try_from(MAX_FRAME_BYTES + 1).unwrap());
      let error = plan_read_batches(&[1], &sizes, &one_sample_per_chunk(), &[0])
         .expect_err("oversized sample must be rejected");

      assert!(matches!(
         error,
         MediaParserError::InvalidFormat(message) if message.contains("invalid thumbnail sample size")
      ));
   }

   #[test]
   fn rejects_total_sample_bytes_over_the_batch_limit() {
      let sizes = fixed_samples(3, u32::try_from(MAX_FRAME_BYTES).unwrap());
      let error = plan_read_batches(
         &[1, 2, 3],
         &sizes,
         &one_sample_per_chunk(),
         &[0, MAX_FRAME_BYTES as u64, (MAX_FRAME_BYTES * 2) as u64],
      )
      .expect_err("sample byte total must be bounded");

      assert!(matches!(
         error,
         MediaParserError::InvalidFormat(message) if message.contains("thumbnail sample batch is too large")
      ));
   }

   #[test]
   fn splits_a_nearby_read_that_would_exceed_the_coalesced_limit() {
      let sizes = SampleSizes::variable(vec![MAX_COALESCED_READ_BYTES as u32, 1]).unwrap();
      let batches = plan_read_batches(
         &[1, 2],
         &sizes,
         &one_sample_per_chunk(),
         &[0, MAX_COALESCED_READ_BYTES as u64 + 1],
      )
      .expect("the oversized merged read should split instead of failing");

      assert_eq!(batches.len(), 2);
      assert_eq!(batches[0].size, MAX_COALESCED_READ_BYTES);
      assert_eq!(batches[1].size, 1);
   }

   #[test]
   fn rejects_coalesced_reads_over_the_batch_limit() {
      let sample_count = 4_101u32;
      let group_size = 1_367u32;
      let mut offsets = Vec::with_capacity(sample_count as usize);
      let mut group_start = 0u64;
      for index in 0..sample_count {
         let position = index % group_size;
         if position == 0 && index != 0 {
            group_start = offsets.last().copied().unwrap() + READ_COALESCE_GAP_BYTES + 2;
         }
         offsets.push(group_start + u64::from(position) * (READ_COALESCE_GAP_BYTES + 1));
      }
      let sizes = fixed_samples(sample_count, 1);
      let sample_indices: Vec<u32> = (1..=sample_count).collect();
      let error = plan_read_batches(&sample_indices, &sizes, &one_sample_per_chunk(), &offsets)
         .expect_err("coalesced read total must be bounded");

      assert!(matches!(
         error,
         MediaParserError::InvalidFormat(message) if message == "coalesced thumbnail reads are too large"
      ));
   }

   struct TruncatedReader(Vec<u8>);

   #[async_trait]
   impl StreamReader for TruncatedReader {
      async fn read_at(&self, offset: u64, buf: &mut [u8]) -> Result<usize> {
         let start = usize::try_from(offset)
            .unwrap_or(usize::MAX)
            .min(self.0.len());
         let read = buf.len().min(self.0.len() - start);
         buf[..read].copy_from_slice(&self.0[start..start + read]);
         Ok(read)
      }

      async fn size(&self) -> Result<u64> {
         Ok(self.0.len() as u64)
      }
   }

   #[tokio::test]
   async fn rejects_a_truncated_coalesced_read() {
      let reader = TruncatedReader(vec![1, 2, 3]);
      let sizes = fixed_samples(1, 4);
      let error = read_samples_coalesced(&reader, &[1], &sizes, &one_sample_per_chunk(), &[0])
         .await
         .expect_err("short reads must not produce partial samples");

      assert!(matches!(
         error,
         MediaParserError::InvalidFormat(message) if message.contains("truncated sample batch")
      ));
   }
}

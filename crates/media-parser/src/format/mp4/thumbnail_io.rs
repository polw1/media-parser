//! Bounded, coalesced sample reads for MP4 thumbnail extraction.

use super::atoms::{SampleSizes, StscEntry, sample_file_offset, sample_size};
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
   let mut total_sample_bytes = 0usize;
   for sample_index in unique_samples {
      let offset =
         sample_file_offset(sample_index, sizes, stsc, chunk_offsets).ok_or_else(|| {
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

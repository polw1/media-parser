//! Functions to locate and read the `moov` box from MP4 files.
//!
//! The `moov` box contains movie-level metadata including duration, timescale,
//! and track information. This module provides functions to efficiently locate
//! the `moov` box in both local and remote files using partial reads.

use super::read::{read_box, read_box_header};
use crate::errors::{MediaParserError, Result};
use crate::stream::StreamReader;

const HEAD_SIZE: usize = 8 * 1024;
const TAIL_SIZE: usize = 512 * 1024;
/// Maximum accepted moov box size (100 MiB) to avoid unbounded allocations.
const MAX_MOOV_SIZE: u64 = 100 * 1024 * 1024;

/// Parses a complete `moov` box and returns its payload.
pub(crate) fn parse_moov_payload(data: &[u8]) -> Result<&[u8]> {
   read_box(data, 0)
      .filter(|box_read| box_read.fourcc == *b"moov")
      .map(|box_read| box_read.payload)
      .ok_or_else(|| MediaParserError::InvalidFormat("invalid moov box".to_string()))
}

/// Valid child boxes that can appear inside moov.
const MOOV_CHILD_FOURCCS: &[[u8; 4]] = &[
   *b"mvhd", // Movie Header (required, usually first)
   *b"trak", // Track (required, 1+)
   *b"udta", // User Data
   *b"meta", // Metadata
];

/// Locates and reads the entire `moov` box from an MP4 file.
///
/// Search strategy:
/// 1. First 8 KB - iterate boxes (streaming-optimized files)
/// 2. Last 512 KB - pattern search for "moov" (traditional files)
///
/// # Errors
///
/// Returns [`MediaParserError::InvalidFormat`] if the `moov` box is not found.
///
/// # Example
///
/// ```no_run
/// use media_parser::{FileStreamReader, format::mp4::atoms::find_and_read_moov_box};
///
/// # async fn example() -> media_parser::Result<()> {
/// let reader = FileStreamReader::new("video.mp4")?;
/// let moov_data = find_and_read_moov_box(&reader).await?;
/// println!("moov box: {} bytes", moov_data.len());
/// # Ok(())
/// # }
/// ```
pub async fn find_and_read_moov_box(reader: &dyn StreamReader) -> Result<Vec<u8>> {
   // Strategy 1: Head - iterate aligned boxes
   // Read before asking for size so HTTP readers can learn it from Content-Range
   // and avoid a separate HEAD request.
   let mut head_buf = vec![0u8; HEAD_SIZE];
   let head_read = reader.read_at(0, &mut head_buf).await?;
   head_buf.truncate(head_read);
   let file_size = reader.size().await?;

   if let Some((pos, size)) = find_moov_aligned(&head_buf, 0) {
      return read_moov_at(reader, pos, size, &head_buf, 0).await;
   }

   // Strategy 2: Tail - pattern search for "moov" fourcc
   let tail_len = TAIL_SIZE.min(usize::try_from(file_size).unwrap_or(usize::MAX));
   let tail_offset = file_size.saturating_sub(tail_len as u64);
   let mut tail_buf = vec![0u8; tail_len];
   let _ = reader.read_at(tail_offset, &mut tail_buf).await?;

   if let Some((pos, size)) = find_moov_pattern(&tail_buf, tail_offset, file_size) {
      return read_moov_at(reader, pos, size, &tail_buf, tail_offset).await;
   }

   Err(crate::errors::MediaParserError::InvalidFormat(
      "moov box not found".into(),
   ))
}

/// Read moov from buffer or directly from reader.
async fn read_moov_at(
   reader: &dyn StreamReader,
   pos: u64,
   size: u64,
   buf: &[u8],
   buf_offset: u64,
) -> Result<Vec<u8>> {
   if size > MAX_MOOV_SIZE {
      return Err(crate::errors::MediaParserError::InvalidFormat(format!(
         "moov box too large: {} bytes",
         size
      )));
   }

   let rel_offset = pos.checked_sub(buf_offset).ok_or_else(|| {
      crate::errors::MediaParserError::InvalidFormat("moov offset underflow".into())
   })?;
   let local_start = usize::try_from(rel_offset)
      .map_err(|_| crate::errors::MediaParserError::InvalidFormat("moov offset overflow".into()))?;
   let size_usize = usize::try_from(size)
      .map_err(|_| crate::errors::MediaParserError::InvalidFormat("moov box too large".into()))?;

   if let Some(local_end) = local_start.checked_add(size_usize)
      && local_end <= buf.len()
   {
      return Ok(buf[local_start..local_end].to_vec());
   }

   // Read directly
   let mut moov_buf = vec![0u8; size_usize];
   let read = reader.read_at(pos, &mut moov_buf).await?;
   if read != size_usize {
      return Err(crate::errors::MediaParserError::InvalidFormat(format!(
         "truncated moov box: expected {} bytes, read {}",
         size_usize, read
      )));
   }
   Ok(moov_buf)
}

/// Find moov by iterating aligned boxes (for head).
fn find_moov_aligned(buf: &[u8], base_offset: u64) -> Option<(u64, u64)> {
   let mut offset = 0usize;
   while let Some(h) = read_box_header(buf, offset) {
      if &h.fourcc == b"moov" {
         return Some((base_offset.checked_add(offset as u64)?, h.total_size as u64));
      }
      offset = offset.checked_add(h.total_size)?;
   }
   None
}

/// Check if first child box has a valid moov child fourcc.
fn has_valid_moov_child(buf: &[u8], payload_start: usize, payload_size: usize) -> bool {
   let Some(child_header) = read_box_header(buf, payload_start) else {
      return false;
   };
   child_header.total_size <= payload_size && MOOV_CHILD_FOURCCS.contains(&child_header.fourcc)
}

/// Find moov by pattern search (for tail/unaligned buffers).
fn find_moov_pattern(buf: &[u8], base_offset: u64, file_size: u64) -> Option<(u64, u64)> {
   for i in 4..buf.len().saturating_sub(4) {
      if &buf[i..i + 4] == b"moov" {
         let Some(box_offset) = i.checked_sub(4) else {
            continue;
         };
         let Some(header) = read_box_header(buf, box_offset) else {
            continue;
         };
         let Some(box_start) = base_offset.checked_add(box_offset as u64) else {
            continue;
         };
         let box_size = header.total_size as u64;
         let Some(payload_start) = box_offset.checked_add(header.header_len) else {
            continue;
         };
         let payload_size = header.total_size - header.header_len;

         // Validate: box fits in file AND has valid first child
         if box_start
            .checked_add(box_size)
            .is_some_and(|box_end| box_end <= file_size)
            && has_valid_moov_child(buf, payload_start, payload_size)
         {
            return Some((box_start, box_size));
         }
      }
   }
   None
}

#[cfg(test)]
mod tests {
   use super::*;
   use std::sync::atomic::{AtomicBool, Ordering};

   struct ReadBeforeSizeReader {
      data: Vec<u8>,
      read_started: AtomicBool,
   }

   #[async_trait::async_trait]
   impl StreamReader for ReadBeforeSizeReader {
      async fn read_at(&self, offset: u64, buf: &mut [u8]) -> Result<usize> {
         self.read_started.store(true, Ordering::Relaxed);
         let start = usize::try_from(offset).unwrap_or(usize::MAX);
         let Some(source) = self.data.get(start..) else {
            return Ok(0);
         };
         let read = buf.len().min(source.len());
         buf[..read].copy_from_slice(&source[..read]);
         Ok(read)
      }

      async fn size(&self) -> Result<u64> {
         if !self.read_started.load(Ordering::Relaxed) {
            return Err(MediaParserError::Other(
               "size was requested before the initial range read".to_string(),
            ));
         }
         Ok(self.data.len() as u64)
      }
   }

   fn make_box(fourcc: &[u8; 4], payload_size: usize) -> Vec<u8> {
      let total = 8 + payload_size;
      let mut buf = Vec::with_capacity(total);
      buf.extend_from_slice(&(total as u32).to_be_bytes());
      buf.extend_from_slice(fourcc);
      buf.extend_from_slice(&vec![0u8; payload_size]);
      buf
   }

   /// Create a moov box with a valid mvhd child inside.
   fn make_moov_with_mvhd() -> Vec<u8> {
      let mvhd = make_box(b"mvhd", 100); // mvhd with 100 bytes payload
      let total = 8 + mvhd.len();
      let mut buf = Vec::with_capacity(total);
      buf.extend_from_slice(&(total as u32).to_be_bytes());
      buf.extend_from_slice(b"moov");
      buf.extend(mvhd);
      buf
   }

   /// Create an extended-size moov box with a valid mvhd child inside.
   fn make_extended_moov_with_mvhd() -> Vec<u8> {
      let mvhd = make_box(b"mvhd", 100);
      let total = 16 + mvhd.len();
      let mut buf = Vec::with_capacity(total);
      buf.extend_from_slice(&1u32.to_be_bytes());
      buf.extend_from_slice(b"moov");
      buf.extend_from_slice(&(total as u64).to_be_bytes());
      buf.extend(mvhd);
      buf
   }

   #[tokio::test]
   async fn reads_the_head_before_requesting_stream_size() {
      let reader = ReadBeforeSizeReader {
         data: make_moov_with_mvhd(),
         read_started: AtomicBool::new(false),
      };

      let moov = find_and_read_moov_box(&reader)
         .await
         .expect("the first read should make the size available");

      assert_eq!(moov, reader.data);
   }

   #[test]
   fn test_find_moov_aligned_at_start() {
      let buf = make_box(b"moov", 16);
      let result = find_moov_aligned(&buf, 0);
      assert_eq!(result, Some((0, 24)));
   }

   #[test]
   fn test_find_moov_aligned_after_ftyp() {
      let mut buf = make_box(b"ftyp", 8);
      buf.extend(make_box(b"moov", 16));
      let result = find_moov_aligned(&buf, 0);
      assert_eq!(result, Some((16, 24)));
   }

   #[test]
   fn test_find_moov_aligned_rejects_overflowing_box_offset() {
      let mut buf = make_box(b"free", 0);
      buf.extend_from_slice(&1u32.to_be_bytes());
      buf.extend_from_slice(b"skip");
      buf.extend_from_slice(&u64::MAX.to_be_bytes());

      assert_eq!(find_moov_aligned(&buf, 0), None);
   }

   #[test]
   fn test_find_moov_pattern_unaligned() {
      // Simulate tail buffer that starts mid-file
      let mut buf = vec![0u8; 100]; // garbage prefix
      let moov = make_moov_with_mvhd();
      let moov_size = moov.len();
      buf.extend(moov);
      let file_size = 10000u64;
      let base_offset = file_size - buf.len() as u64;

      let result = find_moov_pattern(&buf, base_offset, file_size);
      assert!(result.is_some());
      let (pos, size) = result.unwrap();
      assert_eq!(size as usize, moov_size);
      assert_eq!(pos, base_offset + 100);
   }

   #[test]
   fn test_find_moov_pattern_accepts_partial_moov_with_child_header() {
      let mut buf = make_moov_with_mvhd();
      let moov_size = buf.len();
      buf.truncate(16);
      let base_offset = 10_000u64;
      let file_size = base_offset + u64::try_from(moov_size).unwrap();

      assert_eq!(
         find_moov_pattern(&buf, base_offset, file_size),
         Some((base_offset, u64::try_from(moov_size).unwrap()))
      );
   }

   #[test]
   fn test_find_extended_moov_pattern_unaligned() {
      let mut buf = vec![0u8; 100];
      let moov = make_extended_moov_with_mvhd();
      let moov_size = moov.len();
      buf.extend(moov);
      let file_size = 10_000u64;
      let base_offset = file_size - buf.len() as u64;

      let result = find_moov_pattern(&buf, base_offset, file_size);

      assert_eq!(
         result,
         Some((base_offset + 100, u64::try_from(moov_size).unwrap()))
      );
   }

   #[test]
   fn test_find_extended_moov_pattern_at_buffer_start() {
      let buf = make_extended_moov_with_mvhd();
      let base_offset = 10_000u64;
      let file_size = base_offset + u64::try_from(buf.len()).unwrap();

      let result = find_moov_pattern(&buf, base_offset, file_size);

      assert_eq!(
         result,
         Some((base_offset, u64::try_from(buf.len()).unwrap()))
      );
   }

   #[test]
   fn test_find_moov_pattern_rejects_fake_moov() {
      // moov without valid child should be rejected
      let mut buf = vec![0u8; 100];
      buf.extend(make_box(b"moov", 16)); // moov with zeros (no valid child)
      let file_size = 10000u64;
      let base_offset = file_size - buf.len() as u64;

      let result = find_moov_pattern(&buf, base_offset, file_size);
      assert!(result.is_none()); // Should reject fake moov
   }

   #[test]
   fn test_find_moov_pattern_rejects_undersized_child_box() {
      let mut buf = 16u32.to_be_bytes().to_vec();
      buf.extend_from_slice(b"moov");
      buf.extend_from_slice(&4u32.to_be_bytes());
      buf.extend_from_slice(b"mvhd");

      assert_eq!(find_moov_pattern(&buf, 0, buf.len() as u64), None);
   }

   #[test]
   fn test_find_moov_pattern_rejects_child_larger_than_moov() {
      let mut buf = 16u32.to_be_bytes().to_vec();
      buf.extend_from_slice(b"moov");
      buf.extend_from_slice(&32u32.to_be_bytes());
      buf.extend_from_slice(b"mvhd");

      assert_eq!(find_moov_pattern(&buf, 0, buf.len() as u64), None);
   }

   #[test]
   fn test_find_moov_pattern_rejects_overflowing_file_end() {
      let mut buf = Vec::new();
      buf.extend_from_slice(&8u32.to_be_bytes());
      buf.extend_from_slice(b"moov");
      buf.extend_from_slice(b"mvhd");

      assert_eq!(find_moov_pattern(&buf, u64::MAX - 1, u64::MAX), None);
   }

   #[test]
   fn test_moov_not_found() {
      let buf = make_box(b"ftyp", 8);
      let result = find_moov_aligned(&buf, 0);
      assert_eq!(result, None);
   }
}

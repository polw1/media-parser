//! Functions to locate and read the `moov` box from MP4 files.
//!
//! The `moov` box contains movie-level metadata including duration, timescale,
//! and track information. This module provides functions to efficiently locate
//! the `moov` box in both local and remote files using partial reads.

use super::read::read_box_header;
use crate::errors::Result;
use crate::helpers::{read_u32_be, read_u64_be};
use crate::stream::StreamReader;

const HEAD_SIZE: usize = 8 * 1024;
const TAIL_SIZE: usize = 512 * 1024;
/// Maximum accepted moov box size (100 MiB) to avoid unbounded allocations.
const MAX_MOOV_SIZE: u64 = 100 * 1024 * 1024;

/// Valid child boxes that can appear inside moov.
const MOOV_CHILD_FOURCCS: &[&[u8; 4]] = &[
   b"mvhd", // Movie Header (required, usually first)
   b"trak", // Track (required, 1+)
   b"udta", // User Data
   b"meta", // Metadata
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
   let file_size = reader.size().await?;

   // Strategy 1: Head - iterate aligned boxes
   let head_len = HEAD_SIZE.min(usize::try_from(file_size).unwrap_or(usize::MAX));
   let mut head_buf = vec![0u8; head_len];
   let _ = reader.read_at(0, &mut head_buf).await?;

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
fn has_valid_moov_child(buf: &[u8], payload_start: usize) -> bool {
   // Need at least 8 bytes: 4 for child size + 4 for child fourcc
   let Some(child_start) = payload_start.checked_add(4) else {
      return false;
   };
   let Some(child_end) = payload_start.checked_add(8) else {
      return false;
   };
   let Some(child_fourcc) = buf.get(child_start..child_end) else {
      return false;
   };
   MOOV_CHILD_FOURCCS
      .iter()
      .any(|&valid| valid == child_fourcc)
}

/// Find moov by pattern search (for tail/unaligned buffers).
fn find_moov_pattern(buf: &[u8], base_offset: u64, file_size: u64) -> Option<(u64, u64)> {
   for i in 4..buf.len().saturating_sub(4) {
      if &buf[i..i + 4] == b"moov" {
         let Some(size32) = read_u32_be(buf, i - 4) else {
            continue;
         };
         let size32 = size32 as u64;
         let (box_start, box_size) =
            if size32 == 1 && i >= 12 && i.checked_add(12).is_some_and(|end| end <= buf.len()) {
               // Extended size
               let ext_size = read_u64_be(buf, i.checked_add(4)?)?;
               (base_offset.checked_add(i as u64)?.checked_sub(4)?, ext_size)
            } else if size32 >= 8 {
               (base_offset.checked_add(i as u64)?.checked_sub(4)?, size32)
            } else {
               continue;
            };

         // Validate: box fits in file AND has valid first child
         if box_start
            .checked_add(box_size)
            .is_some_and(|box_end| box_end <= file_size)
            && has_valid_moov_child(buf, i.checked_add(4)?)
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

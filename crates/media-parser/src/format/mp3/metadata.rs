//! # MP3 Metadata Extraction
//!
//! Extracts metadata from MP3 files using ID3v2 tags.
//!
//! ## ID3v2 Frame IDs
//!
//! - TIT2: Title
//! - TPE1: Artist
//! - TALB: Album
//! - TYER/TDRC: Year
//! - TRCK: Track number
//! - TCON: Genre
//! - COMM: Comment
//! - TCOM: Composer

use super::duration::calculate_duration;
use super::tags::{frame_id_to_key, frame_name};
use crate::Result;
use crate::helpers::{
   decode_latin1, decode_utf8, decode_utf16_be, decode_utf16_with_bom, trim_null_and_whitespace,
};
use crate::stream::StreamReader;
use crate::types::{Meta, Metadata};

const ID3_HEADER_SIZE: usize = 10;
const ID3_FRAME_HEADER_SIZE: usize = 10;

/// Reads metadata from an MP3 file.
///
/// Extracts ID3v2 tags (if present) and calculates duration.
/// MP3 files without ID3v2 tags are valid - duration will still be calculated.
///
/// # Errors
///
/// Returns an error if reading from the stream fails.
pub async fn read_metadata(reader: &dyn StreamReader) -> Result<Metadata> {
   let header = try_read_id3_header(reader).await?;

   let (values, id3_end) = match header {
      Some(h) => {
         let frames = read_id3_frames(reader, &h).await?;
         let values = frames_to_meta(frames);
         (values, id3_end(&h))
      }
      None => (vec![], 0),
   };

   let duration = calculate_duration(reader, id3_end).await?;

   Ok(Metadata {
      format: "MP3".to_string(),
      values,
      timescale: 1000,
      duration: duration.millis,
   })
}

#[derive(Debug)]
struct Id3Header {
   version: (u8, u8),
   #[allow(dead_code)]
   flags: u8,
   tag_size: u32,
}

const fn id3_end(header: &Id3Header) -> u64 {
   ID3_HEADER_SIZE as u64 + header.tag_size as u64
}

pub(super) async fn read_id3_end(reader: &dyn StreamReader) -> Result<u64> {
   Ok(try_read_id3_header(reader)
      .await?
      .as_ref()
      .map_or(0, id3_end))
}

/// Tries to read the ID3v2 header. Returns None if not present.
async fn try_read_id3_header(reader: &dyn StreamReader) -> Result<Option<Id3Header>> {
   let mut header_bytes = [0u8; ID3_HEADER_SIZE];
   let bytes_read = reader.read_at(0, &mut header_bytes).await?;

   if bytes_read < ID3_HEADER_SIZE || &header_bytes[0..3] != b"ID3" {
      return Ok(None);
   }

   let version = (header_bytes[3], header_bytes[4]);
   let flags = header_bytes[5];

   // Syncsafe integer: each byte uses only 7 bits
   let tag_size = ((header_bytes[6] as u32 & 0x7F) << 21)
      | ((header_bytes[7] as u32 & 0x7F) << 14)
      | ((header_bytes[8] as u32 & 0x7F) << 7)
      | (header_bytes[9] as u32 & 0x7F);

   Ok(Some(Id3Header {
      version,
      flags,
      tag_size,
   }))
}

#[derive(Debug)]
struct Id3Frame {
   id: [u8; 4],
   data: Vec<u8>,
}

/// Reads all ID3v2 frames from the tag.
async fn read_id3_frames(reader: &dyn StreamReader, header: &Id3Header) -> Result<Vec<Id3Frame>> {
   let mut frames = Vec::new();
   let mut offset = ID3_HEADER_SIZE as u64;
   let end_offset = ID3_HEADER_SIZE as u64 + header.tag_size as u64;

   // Skip extended header if present (bit 6 of flags)
   if header.flags & 0x40 != 0 {
      let mut ext_size_bytes = [0u8; 4];
      if reader.read_at(offset, &mut ext_size_bytes).await? >= 4 {
         let ext_size = if header.version.0 >= 4 {
            // ID3v2.4: syncsafe integer, size includes the 4 bytes itself
            ((ext_size_bytes[0] as u32 & 0x7F) << 21)
               | ((ext_size_bytes[1] as u32 & 0x7F) << 14)
               | ((ext_size_bytes[2] as u32 & 0x7F) << 7)
               | (ext_size_bytes[3] as u32 & 0x7F)
         } else {
            // ID3v2.3: regular big-endian, size excludes the 4 bytes itself
            u32::from_be_bytes(ext_size_bytes) + 4
         };
         offset += ext_size as u64;
      }
   }

   while offset + ID3_FRAME_HEADER_SIZE as u64 <= end_offset {
      let mut frame_header = [0u8; ID3_FRAME_HEADER_SIZE];
      let bytes_read = reader.read_at(offset, &mut frame_header).await?;

      if bytes_read < ID3_FRAME_HEADER_SIZE {
         break;
      }

      let id = [
         frame_header[0],
         frame_header[1],
         frame_header[2],
         frame_header[3],
      ];

      // End of frames (padding)
      if id == [0, 0, 0, 0] {
         break;
      }

      // Frame size (ID3v2.4 uses syncsafe, ID3v2.3 uses regular)
      let size = if header.version.0 >= 4 {
         // Syncsafe integer for ID3v2.4
         ((frame_header[4] as u32 & 0x7F) << 21)
            | ((frame_header[5] as u32 & 0x7F) << 14)
            | ((frame_header[6] as u32 & 0x7F) << 7)
            | (frame_header[7] as u32 & 0x7F)
      } else {
         // Regular integer for ID3v2.3 and earlier
         ((frame_header[4] as u32) << 24)
            | ((frame_header[5] as u32) << 16)
            | ((frame_header[6] as u32) << 8)
            | (frame_header[7] as u32)
      };

      offset += ID3_FRAME_HEADER_SIZE as u64;

      if size == 0 || offset + size as u64 > end_offset {
         break;
      }

      let mut data = vec![0u8; size as usize];
      reader.read_at(offset, &mut data).await?;
      offset += size as u64;

      frames.push(Id3Frame { id, data });
   }

   Ok(frames)
}

/// Decodes ID3v2 text frame data based on encoding byte.
fn decode_id3_text(data: &[u8]) -> Option<String> {
   if data.is_empty() {
      return None;
   }

   let encoding = data[0];
   let text_data = &data[1..];

   let text = match encoding {
      0 => decode_latin1(text_data)?,         // ISO-8859-1 (Latin-1)
      1 => decode_utf16_with_bom(text_data)?, // UTF-16 with BOM
      2 => decode_utf16_be(text_data)?,       // UTF-16BE without BOM
      3 => decode_utf8(text_data)?,           // UTF-8
      _ => return None,
   };

   trim_null_and_whitespace(&text)
}

/// Converts ID3 frames to Meta values.
fn frames_to_meta(frames: Vec<Id3Frame>) -> Vec<Meta> {
   frames
      .into_iter()
      .filter_map(|frame| {
         let key = frame_id_to_key(frame.id);

         // Only process text frames (start with 'T') and comment frames
         if !key.starts_with('T') && key != "COMM" {
            return None;
         }

         let value = if key == "COMM" {
            decode_comment_frame(&frame.data)
         } else {
            decode_id3_text(&frame.data)
         }?;

         let name = frame_name(frame.id).to_string();

         Some(Meta { key, name, value })
      })
      .collect()
}

/// Decodes a COMM (comment) frame.
fn decode_comment_frame(data: &[u8]) -> Option<String> {
   if data.len() < 5 {
      return None;
   }

   let encoding = data[0];
   // Skip language (3 bytes) and find the actual comment
   let comment_data = &data[4..];

   // Find the null terminator separating description from comment
   let null_pos = match encoding {
      1 | 2 => {
         // UTF-16: look for double null
         comment_data
            .chunks(2)
            .position(|chunk| chunk == [0, 0])
            .map(|p| p * 2 + 2)
      }
      _ => {
         // Single-byte encodings
         comment_data.iter().position(|&b| b == 0).map(|p| p + 1)
      }
   };

   let actual_comment = match null_pos {
      Some(pos) if pos < comment_data.len() => &comment_data[pos..],
      _ => comment_data,
   };

   // Prepend encoding byte for decode_id3_text
   let mut full_data = vec![encoding];
   full_data.extend_from_slice(actual_comment);
   decode_id3_text(&full_data)
}

#[cfg(test)]
mod tests {
   use super::*;

   #[test]
   fn test_decode_id3_text_utf8() {
      let data = [3, b'H', b'e', b'l', b'l', b'o']; // encoding=3 (UTF-8)
      assert_eq!(decode_id3_text(&data), Some("Hello".to_string()));
   }

   #[test]
   fn test_decode_id3_text_latin1() {
      let data = [0, b'H', b'e', b'l', b'l', b'o']; // encoding=0 (Latin-1)
      assert_eq!(decode_id3_text(&data), Some("Hello".to_string()));
   }

   #[test]
   fn test_decode_id3_text_utf16_le() {
      // encoding=1, BOM=FF FE (LE), "Hi"
      let data = [1, 0xFF, 0xFE, 0x48, 0x00, 0x69, 0x00];
      assert_eq!(decode_id3_text(&data), Some("Hi".to_string()));
   }

   #[test]
   fn test_decode_id3_text_utf16_be() {
      // encoding=1, BOM=FE FF (BE), "Hi"
      let data = [1, 0xFE, 0xFF, 0x00, 0x48, 0x00, 0x69];
      assert_eq!(decode_id3_text(&data), Some("Hi".to_string()));
   }

   #[test]
   fn test_decode_id3_text_empty() {
      assert_eq!(decode_id3_text(&[]), None);
      assert_eq!(decode_id3_text(&[0]), None); // Only encoding byte, no text
   }
}

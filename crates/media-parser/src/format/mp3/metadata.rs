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
use crate::helpers::{
   decode_latin1, decode_utf8, decode_utf16_be, decode_utf16_with_bom, detect_image_format,
   trim_null_and_whitespace,
};
use crate::stream::StreamReader;
use crate::types::{CoverArt, Meta, Metadata, PixelFormat};
use crate::{MediaParserError, Result};

const ID3_HEADER_SIZE: usize = 10;
const ID3_FRAME_HEADER_SIZE: usize = 10;
const MAX_ID3_TAG_BYTES: usize = 64 * 1024 * 1024;

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

/// Reads embedded ID3v2 APIC cover artwork, when present.
pub async fn read_cover(reader: &dyn StreamReader) -> Result<Option<CoverArt>> {
   let Some(header) = try_read_id3_header(reader).await? else {
      return Ok(None);
   };

   let frames = read_id3_frames(reader, &header).await?;
   let mut fallback = None;
   for frame in frames {
      if frame.id != *b"APIC" {
         continue;
      }
      if let Some((picture_type, cover)) = decode_apic_frame(&frame.data) {
         if picture_type == 3 {
            return Ok(Some(cover));
         }
         fallback.get_or_insert(cover);
      }
   }
   Ok(fallback)
}

#[derive(Debug)]
struct Id3Header {
   version: (u8, u8),
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
   // ID3v2.2 uses 6-byte frame headers with 3-character IDs; parsing it with
   // the 10-byte header layout would misread sizes, so its frames are skipped.
   if header.version.0 < 3 {
      return Ok(Vec::new());
   }

   let tag_size = usize::try_from(header.tag_size)
      .map_err(|_| MediaParserError::InvalidFormat("ID3 tag is too large".to_string()))?;
   if tag_size > MAX_ID3_TAG_BYTES {
      return Err(MediaParserError::InvalidFormat(format!(
         "ID3 tag exceeds {MAX_ID3_TAG_BYTES} bytes"
      )));
   }

   let mut tag_data = Vec::new();
   tag_data
      .try_reserve_exact(tag_size)
      .map_err(|_| MediaParserError::InvalidFormat("ID3 tag is too large".to_string()))?;
   tag_data.resize(tag_size, 0);
   let bytes_read = reader
      .read_at(ID3_HEADER_SIZE as u64, &mut tag_data)
      .await?;
   // A tag whose declared size runs past the end of the object yields the
   // frames that arrived instead of failing the whole file.
   tag_data.truncate(bytes_read);

   let tag_unsynchronized = header.flags & 0x80 != 0;
   if tag_unsynchronized && header.version.0 < 4 {
      tag_data = deunsynchronize(&tag_data)?;
   }
   parse_id3_frames(&tag_data, header, tag_unsynchronized)
}

fn parse_id3_frames(
   tag_data: &[u8],
   header: &Id3Header,
   tag_unsynchronized: bool,
) -> Result<Vec<Id3Frame>> {
   let mut frames = Vec::new();
   let mut offset = 0usize;

   // Skip extended header if present (bit 6 of flags)
   if header.flags & 0x40 != 0 {
      let ext_size_bytes: [u8; 4] = tag_data
         .get(0..4)
         .and_then(|bytes| bytes.try_into().ok())
         .ok_or_else(|| {
            MediaParserError::InvalidFormat("truncated ID3 extended header".to_string())
         })?;
      let ext_size = if header.version.0 >= 4 {
         usize::try_from(decode_syncsafe(ext_size_bytes)).map_err(|_| {
            MediaParserError::InvalidFormat("ID3 extended header is too large".to_string())
         })?
      } else {
         usize::try_from(u32::from_be_bytes(ext_size_bytes))
            .ok()
            .and_then(|size| size.checked_add(4))
            .ok_or_else(|| {
               MediaParserError::InvalidFormat("ID3 extended header is too large".to_string())
            })?
      };
      if ext_size < 4 || ext_size > tag_data.len() {
         return Err(MediaParserError::InvalidFormat(
            "invalid ID3 extended header size".to_string(),
         ));
      }
      offset = ext_size;
   }

   while offset
      .checked_add(ID3_FRAME_HEADER_SIZE)
      .is_some_and(|end| end <= tag_data.len())
   {
      let frame_header: [u8; ID3_FRAME_HEADER_SIZE] = tag_data
         .get(offset..offset + ID3_FRAME_HEADER_SIZE)
         .and_then(|bytes| bytes.try_into().ok())
         .ok_or_else(|| MediaParserError::InvalidFormat("truncated ID3 frame".to_string()))?;

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
         decode_syncsafe([
            frame_header[4],
            frame_header[5],
            frame_header[6],
            frame_header[7],
         ])
      } else {
         // Regular integer for ID3v2.3 and earlier
         ((frame_header[4] as u32) << 24)
            | ((frame_header[5] as u32) << 16)
            | ((frame_header[6] as u32) << 8)
            | (frame_header[7] as u32)
      };

      offset += ID3_FRAME_HEADER_SIZE;
      let size = usize::try_from(size)
         .map_err(|_| MediaParserError::InvalidFormat("ID3 frame is too large".to_string()))?;
      let frame_end = offset
         .checked_add(size)
         .ok_or_else(|| MediaParserError::InvalidFormat("ID3 frame size overflow".to_string()))?;

      if size == 0 {
         break;
      }
      if frame_end > tag_data.len() {
         // A frame that overruns the tag ends the walk; the frames gathered
         // so far are still returned.
         break;
      }

      let format_flags = frame_header[9];
      let mut data = Vec::new();
      data
         .try_reserve_exact(size)
         .map_err(|_| MediaParserError::InvalidFormat("ID3 frame is too large".to_string()))?;
      data.extend_from_slice(&tag_data[offset..frame_end]);
      offset = frame_end;

      if header.version.0 >= 4 {
         if format_flags & 0x0c != 0 {
            return Err(MediaParserError::InvalidFormat(format!(
               "compressed or encrypted ID3 frame {} is unsupported",
               String::from_utf8_lossy(&id)
            )));
         }
         if tag_unsynchronized || format_flags & 0x02 != 0 {
            data = deunsynchronize(&data)?;
         }
         if format_flags & 0x40 != 0 {
            if data.is_empty() {
               return Err(MediaParserError::InvalidFormat(
                  "truncated ID3 grouping identity".to_string(),
               ));
            }
            data.remove(0);
         }
         if format_flags & 0x01 != 0 {
            if data.len() < 4 {
               return Err(MediaParserError::InvalidFormat(
                  "truncated ID3 data length indicator".to_string(),
               ));
            }
            data.drain(..4);
         }
      } else {
         if format_flags & 0xc0 != 0 {
            return Err(MediaParserError::InvalidFormat(format!(
               "compressed or encrypted ID3 frame {} is unsupported",
               String::from_utf8_lossy(&id)
            )));
         }
         if format_flags & 0x20 != 0 {
            if data.is_empty() {
               return Err(MediaParserError::InvalidFormat(
                  "truncated ID3 grouping identity".to_string(),
               ));
            }
            data.remove(0);
         }
      }

      frames.push(Id3Frame { id, data });
   }

   Ok(frames)
}

fn decode_syncsafe(bytes: [u8; 4]) -> u32 {
   ((bytes[0] as u32 & 0x7f) << 21)
      | ((bytes[1] as u32 & 0x7f) << 14)
      | ((bytes[2] as u32 & 0x7f) << 7)
      | (bytes[3] as u32 & 0x7f)
}

fn deunsynchronize(data: &[u8]) -> Result<Vec<u8>> {
   let mut decoded = Vec::new();
   decoded
      .try_reserve(data.len())
      .map_err(|_| MediaParserError::InvalidFormat("ID3 tag is too large".to_string()))?;
   let mut offset = 0usize;
   while offset < data.len() {
      let byte = data[offset];
      decoded.push(byte);
      offset += 1;
      if byte == 0xff && data.get(offset) == Some(&0) {
         offset += 1;
      }
   }
   Ok(decoded)
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

fn decode_apic_frame(data: &[u8]) -> Option<(u8, CoverArt)> {
   let encoding = *data.first()?;
   let mime_end = data.get(1..)?.iter().position(|byte| *byte == 0)? + 1;
   let mime = std::str::from_utf8(data.get(1..mime_end)?).ok()?;
   let picture_type = *data.get(mime_end + 1)?;
   let description_offset = mime_end.checked_add(2)?;
   if description_offset > data.len() {
      return None;
   }

   let image_offset = find_encoded_terminator(data, description_offset, encoding)?;
   let image = data.get(image_offset..)?;
   if image.is_empty() {
      return None;
   }
   let format = cover_format(mime, image)?;
   let mut image_data = Vec::new();
   image_data.try_reserve_exact(image.len()).ok()?;
   image_data.extend_from_slice(image);

   Some((
      picture_type,
      CoverArt {
         mime_type: format.mime_type().to_string(),
         format,
         data: image_data,
      },
   ))
}

fn find_encoded_terminator(data: &[u8], start: usize, encoding: u8) -> Option<usize> {
   match encoding {
      1 | 2 => data
         .get(start..)?
         .chunks_exact(2)
         .position(|unit| unit == [0, 0])
         .map(|position| start + position * 2 + 2),
      _ => data
         .get(start..)?
         .iter()
         .position(|byte| *byte == 0)
         .map(|position| start + position + 1),
   }
}

fn cover_format(mime: &str, image: &[u8]) -> Option<PixelFormat> {
   match mime.to_ascii_lowercase().as_str() {
      "image/jpeg" | "image/jpg" => Some(PixelFormat::Jpeg),
      "image/png" => Some(PixelFormat::Png),
      _ => detect_image_format(image),
   }
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

   #[test]
   fn test_decode_syncsafe_ignores_high_bits() {
      assert_eq!(decode_syncsafe([0xff, 0x81, 0x82, 0x83]), 0x0fe0_4103);
   }

   #[test]
   fn test_deunsynchronize_removes_inserted_zeroes_and_preserves_final_byte() {
      let decoded = deunsynchronize(&[0xff, 0, 0xff, 0, 0x12, 0xff]).expect("decode");
      assert_eq!(decoded, [0xff, 0xff, 0x12, 0xff]);
   }

   struct BytesReader(Vec<u8>);

   #[async_trait::async_trait]
   impl StreamReader for BytesReader {
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

   struct UnreadableReader;

   #[async_trait::async_trait]
   impl StreamReader for UnreadableReader {
      async fn read_at(&self, _offset: u64, _buf: &mut [u8]) -> Result<usize> {
         Err(MediaParserError::Other("unexpected read".into()))
      }

      async fn size(&self) -> Result<u64> {
         Err(MediaParserError::Other("unexpected size read".into()))
      }
   }

   fn id3_header(version: u8, tag_size: usize) -> Vec<u8> {
      let size = tag_size as u32;
      vec![
         b'I',
         b'D',
         b'3',
         version,
         0,
         0,
         ((size >> 21) & 0x7f) as u8,
         ((size >> 14) & 0x7f) as u8,
         ((size >> 7) & 0x7f) as u8,
         (size & 0x7f) as u8,
      ]
   }

   /// Builds a v2.3 TIT2 text frame holding `text` (UTF-8 encoding byte).
   fn tit2_frame(text: &str) -> Vec<u8> {
      let mut frame = b"TIT2".to_vec();
      frame.extend_from_slice(&(text.len() as u32 + 1).to_be_bytes());
      frame.extend_from_slice(&[0, 0]);
      frame.push(3);
      frame.extend_from_slice(text.as_bytes());
      frame
   }

   /// Builds `count` 417-byte MPEG-1 Layer III frames (128 kbps, 44.1 kHz).
   fn audio_frames(count: usize) -> Vec<u8> {
      const FRAME_SIZE: usize = 417;
      let mut data = vec![0; FRAME_SIZE * count];
      for index in 0..count {
         data[FRAME_SIZE * index..FRAME_SIZE * index + 4]
            .copy_from_slice(&[0xff, 0xfb, 0x90, 0x00]);
      }
      data
   }

   #[tokio::test]
   async fn read_metadata_keeps_frames_and_zero_duration_for_truncated_tag() {
      // Declared tag size runs past the end of the object.
      let mut data = id3_header(3, 1000);
      data.extend_from_slice(&tit2_frame("Hi"));
      data.extend_from_slice(&audio_frames(2));

      let metadata = read_metadata(&BytesReader(data)).await.unwrap();

      assert!(
         metadata
            .values
            .iter()
            .any(|meta| meta.key == "TIT2" && meta.value == "Hi")
      );
      assert_eq!(metadata.duration, 0);
   }

   #[tokio::test]
   async fn read_metadata_stops_walk_on_frame_overrunning_tag() {
      let mut data = id3_header(3, tit2_frame("Hi").len() + ID3_FRAME_HEADER_SIZE);
      data.extend_from_slice(&tit2_frame("Hi"));
      // A frame header whose declared size overruns the end of the tag.
      data.extend_from_slice(b"TALB");
      data.extend_from_slice(&500u32.to_be_bytes());
      data.extend_from_slice(&[0, 0]);
      data.extend_from_slice(&audio_frames(2));

      let metadata = read_metadata(&BytesReader(data)).await.unwrap();

      assert!(
         metadata
            .values
            .iter()
            .any(|meta| meta.key == "TIT2" && meta.value == "Hi")
      );
      assert!(metadata.values.iter().all(|meta| meta.key != "TALB"));
      assert_eq!(metadata.duration, 52);
   }

   #[tokio::test]
   async fn read_id3_frames_skips_id3v22_without_reading_tag_data() {
      let header = Id3Header {
         version: (2, 0),
         flags: 0,
         tag_size: 9,
      };

      let frames = read_id3_frames(&UnreadableReader, &header).await.unwrap();

      assert!(frames.is_empty());
   }
}

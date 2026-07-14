//! MPEG frame header parsing.
//!
//! Provides types and functions for parsing MPEG audio frame headers.

use super::tables::{
   MpegLayer, MpegVersion, bitrate_kbps, frame_size, sample_rate_hz, samples_per_frame,
};
use crate::Result;
use crate::stream::StreamReader;

/// Frame header size in bytes.
pub const FRAME_HEADER_SIZE: usize = 4;

/// Maximum bytes to search for frame sync.
pub const MAX_SYNC_SEARCH: u64 = 64 * 1024;

/// Minimum search distance before accepting a frame without next-frame validation.
/// After this point, we trust a valid-looking header even if we can't verify the next frame
/// (e.g., near EOF or in truncated files).
const MIN_SEARCH_BEFORE_FALLBACK: u64 = 1024;

/// Cap on next-frame validations that fall outside the current buffer.
/// In-buffer validations are free; out-of-buffer ones cost a real read (an HTTP
/// range GET for `HttpStreamReader`), so a crafted stream of header-like bytes
/// could otherwise fan a single call out into many requests. The common case
/// (frame smaller than the buffer) never needs these reads.
const MAX_OUT_OF_BUFFER_VALIDATIONS: usize = 16;

/// Frame sync word (11 bits set).
const SYNC_MASK: u16 = 0xFFE0;
const SYNC_WORD: u16 = 0xFFE0;

/// Parsed MPEG frame header (immutable value type).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FrameHeader {
   /// MPEG version (1, 2, or 2.5).
   pub version: MpegVersion,
   /// MPEG layer (1, 2, or 3).
   pub layer: MpegLayer,
   /// Bitrate in kbps.
   pub bitrate_kbps: u16,
   /// Sample rate in Hz.
   pub sample_rate_hz: u32,
   /// Padding flag.
   pub padding: bool,
   /// Channel mode (0=stereo, 1=joint stereo, 2=dual channel, 3=mono).
   pub channel_mode: u8,
}

impl FrameHeader {
   /// Returns samples per frame for this header's version and layer.
   pub const fn samples_per_frame(&self) -> u32 {
      samples_per_frame(self.version, self.layer)
   }

   /// Returns frame duration in microseconds.
   pub const fn duration_us(&self) -> u64 {
      if self.sample_rate_hz == 0 {
         return 0;
      }
      (self.samples_per_frame() as u64 * 1_000_000) / self.sample_rate_hz as u64
   }

   /// Returns frame duration in milliseconds.
   pub const fn duration_ms(&self) -> u64 {
      if self.sample_rate_hz == 0 {
         return 0;
      }
      (self.samples_per_frame() as u64 * 1_000) / self.sample_rate_hz as u64
   }

   /// Returns frame size in bytes including header.
   pub fn size(&self) -> Option<usize> {
      frame_size(
         self.version,
         self.layer,
         self.bitrate_kbps,
         self.sample_rate_hz,
         self.padding,
      )
   }

   /// Returns offset of Xing/Info header within frame data.
   /// Depends on version and channel mode.
   pub const fn xing_offset(&self) -> usize {
      match (self.version, self.channel_mode) {
         (MpegVersion::V1, 3) => 17, // MPEG1 mono
         (MpegVersion::V1, _) => 32, // MPEG1 stereo
         (_, 3) => 9,                // MPEG2/2.5 mono
         (_, _) => 17,               // MPEG2/2.5 stereo
      }
   }
}

/// Frame parsing result.
#[derive(Debug, Clone, Copy)]
pub enum FrameParseResult {
   /// Valid frame header found.
   Found {
      /// Parsed frame header.
      header: FrameHeader,
      /// Offset where frame starts.
      offset: u64,
   },
   /// No frame sync found in search range.
   NotFound,
   /// End of data reached.
   EndOfData,
}

impl FrameParseResult {
   /// Returns the header if found.
   pub fn header(&self) -> Option<FrameHeader> {
      match self {
         FrameParseResult::Found { header, .. } => Some(*header),
         _ => None,
      }
   }

   /// Returns the offset if a valid frame was found.
   pub fn offset(&self) -> Option<u64> {
      match self {
         FrameParseResult::Found { offset, .. } => Some(*offset),
         _ => None,
      }
   }

   /// Returns true if a valid frame was found.
   pub fn is_found(&self) -> bool {
      matches!(self, FrameParseResult::Found { .. })
   }
}

/// Parses MPEG version from header bits.
const fn parse_version(bits: u8) -> Option<MpegVersion> {
   match bits {
      0b00 => Some(MpegVersion::V25),
      0b01 => None, // Reserved
      0b10 => Some(MpegVersion::V2),
      0b11 => Some(MpegVersion::V1),
      _ => None,
   }
}

/// Parses MPEG layer from header bits.
const fn parse_layer(bits: u8) -> Option<MpegLayer> {
   match bits {
      0b00 => None, // Reserved
      0b01 => Some(MpegLayer::Layer3),
      0b10 => Some(MpegLayer::Layer2),
      0b11 => Some(MpegLayer::Layer1),
      _ => None,
   }
}

/// Parses frame header from 4 bytes.
///
/// # Returns
///
/// - `Some(FrameHeader)` if valid MPEG frame header
/// - `None` if invalid sync, reserved values, or invalid combinations
///
/// # Example
///
/// ```no_run
/// use media_parser::format::mp3::frame::parse_header;
///
/// let bytes = [0xFF, 0xFB, 0x90, 0x00]; // MPEG1 Layer3, 128kbps, 44100Hz
/// if let Some(header) = parse_header(bytes) {
///     assert_eq!(header.bitrate_kbps, 128);
/// }
/// ```
pub fn parse_header(bytes: [u8; 4]) -> Option<FrameHeader> {
   // Check sync word (11 bits)
   let sync = ((bytes[0] as u16) << 8) | (bytes[1] as u16);
   if (sync & SYNC_MASK) != SYNC_WORD {
      return None;
   }

   // Extract fields from header
   let version_bits = (bytes[1] >> 3) & 0x03;
   let layer_bits = (bytes[1] >> 1) & 0x03;
   // let protection = (bytes[1] & 0x01) == 0; // Has CRC
   let bitrate_index = (bytes[2] >> 4) & 0x0F;
   let sample_rate_index = (bytes[2] >> 2) & 0x03;
   let padding = (bytes[2] >> 1) & 0x01 == 1;
   // let private = bytes[2] & 0x01;
   let channel_mode = (bytes[3] >> 6) & 0x03;

   // Parse version and layer
   let version = parse_version(version_bits)?;
   let layer = parse_layer(layer_bits)?;

   // Lookup bitrate and sample rate
   let bitrate = bitrate_kbps(version, layer, bitrate_index)?;
   let sample_rate = sample_rate_hz(version, sample_rate_index)?;

   Some(FrameHeader {
      version,
      layer,
      bitrate_kbps: bitrate,
      sample_rate_hz: sample_rate,
      padding,
      channel_mode,
   })
}

/// Finds first valid frame using a reader.
///
/// Scans from `start_offset` up to `max_search` bytes looking for
/// a valid MPEG frame sync and header.
///
/// # Parameters
///
/// - `reader`: Stream reader for async I/O
/// - `start_offset`: Byte offset to start searching
/// - `max_search`: Maximum bytes to search before giving up
///
/// # Returns
///
/// `FrameParseResult`.
///
/// # Example
///
/// ```no_run
/// use media_parser::format::mp3::frame::{find_first_frame, MAX_SYNC_SEARCH};
///
/// async fn example(reader: &dyn media_parser::StreamReader) -> media_parser::Result<()> {
///     let result = find_first_frame(reader, 0, MAX_SYNC_SEARCH).await?;
///     if let media_parser::format::mp3::frame::FrameParseResult::Found { header, offset } = result {
///         println!("Found frame at offset {}: {:?}", offset, header);
///     }
///     Ok(())
/// }
/// ```
pub async fn find_first_frame(
   reader: &dyn StreamReader,
   start_offset: u64,
   max_search: u64,
) -> Result<FrameParseResult> {
   const BUFFER_SIZE: usize = 4096;
   let mut buffer = vec![0u8; BUFFER_SIZE];
   let mut offset = start_offset;
   let end_offset = start_offset.checked_add(max_search).ok_or_else(|| {
      crate::errors::MediaParserError::Other("MP3 frame search range overflow".into())
   })?;
   let mut external_validations = 0usize;

   while offset < end_offset {
      let remaining = end_offset - offset;
      if remaining < FRAME_HEADER_SIZE as u64 {
         return Ok(FrameParseResult::NotFound);
      }

      let read_size = usize::try_from(remaining)
         .unwrap_or(BUFFER_SIZE)
         .min(BUFFER_SIZE);
      let bytes_read = reader.read_at(offset, &mut buffer[..read_size]).await?;

      if bytes_read < FRAME_HEADER_SIZE {
         return Ok(FrameParseResult::EndOfData);
      }

      // Scan buffer for sync word
      for i in 0..bytes_read.saturating_sub(FRAME_HEADER_SIZE - 1) {
         // Quick check for sync byte
         if buffer[i] != 0xFF {
            continue;
         }

         // Check second byte has sync bits set
         if (buffer[i + 1] & 0xE0) != 0xE0 {
            continue;
         }

         // Try to parse full header
         let header_bytes = [buffer[i], buffer[i + 1], buffer[i + 2], buffer[i + 3]];

         if let Some(header) = parse_header(header_bytes) {
            // Validate frame by checking that the next frame parses as a header.
            if let Some(frame_size) = header.size() {
               let frame_offset = offset + i as u64;
               let next_offset = frame_offset + frame_size as u64;
               // Position of the next header within the current buffer.
               let rel = (next_offset - offset) as usize;

               let next_is_valid = if rel + FRAME_HEADER_SIZE <= bytes_read {
                  // Common case: next frame is already buffered, validate for free.
                  let next_bytes = [
                     buffer[rel],
                     buffer[rel + 1],
                     buffer[rel + 2],
                     buffer[rel + 3],
                  ];
                  parse_header(next_bytes).is_some()
               } else if external_validations < MAX_OUT_OF_BUFFER_VALIDATIONS {
                  // Next frame falls outside the buffer; pay for a bounded read.
                  external_validations += 1;
                  let mut next_header = [0u8; FRAME_HEADER_SIZE];
                  let bytes_read = reader.read_at(next_offset, &mut next_header).await?;
                  bytes_read >= FRAME_HEADER_SIZE && parse_header(next_header).is_some()
               } else {
                  false
               };

               if next_is_valid {
                  // Found valid frame with valid next frame.
                  return Ok(FrameParseResult::Found {
                     header,
                     offset: frame_offset,
                  });
               }

               // No valid next frame, but this frame looks valid.
               // Accept it if we've searched enough bytes.
               if frame_offset.saturating_sub(start_offset) > MIN_SEARCH_BEFORE_FALLBACK {
                  return Ok(FrameParseResult::Found {
                     header,
                     offset: frame_offset,
                  });
               }
            }
         }
      }

      // Move to next chunk, overlapping to catch headers split across chunks
      offset += (bytes_read - FRAME_HEADER_SIZE + 1) as u64;
   }

   Ok(FrameParseResult::NotFound)
}

#[cfg(test)]
mod tests {
   use super::*;
   use async_trait::async_trait;

   struct BytesReader(Vec<u8>);

   struct FailingReader;

   #[async_trait]
   impl StreamReader for BytesReader {
      async fn read_at(&self, offset: u64, buf: &mut [u8]) -> crate::Result<usize> {
         let start = usize::try_from(offset)
            .unwrap_or(usize::MAX)
            .min(self.0.len());
         let read = buf.len().min(self.0.len() - start);
         buf[..read].copy_from_slice(&self.0[start..start + read]);
         Ok(read)
      }

      async fn size(&self) -> crate::Result<u64> {
         Ok(self.0.len() as u64)
      }
   }

   #[async_trait]
   impl StreamReader for FailingReader {
      async fn read_at(&self, _: u64, _: &mut [u8]) -> crate::Result<usize> {
         Err(crate::errors::MediaParserError::Other(
            "read failure".into(),
         ))
      }

      async fn size(&self) -> crate::Result<u64> {
         Ok(0)
      }
   }

   #[test]
   fn test_parse_header_mpeg1_l3_128kbps() {
      // MPEG1 Layer3, 128kbps, 44100Hz, no padding, stereo
      let bytes = [0xFF, 0xFB, 0x90, 0x00];
      let header = parse_header(bytes).expect("should parse");

      assert_eq!(header.version, MpegVersion::V1);
      assert_eq!(header.layer, MpegLayer::Layer3);
      assert_eq!(header.bitrate_kbps, 128);
      assert_eq!(header.sample_rate_hz, 44100);
      assert!(!header.padding);
   }

   #[test]
   fn test_parse_header_mpeg1_l3_320kbps() {
      // MPEG1 Layer3, 320kbps, 44100Hz, no padding
      let bytes = [0xFF, 0xFB, 0xE0, 0x00];
      let header = parse_header(bytes).expect("should parse");

      assert_eq!(header.bitrate_kbps, 320);
      assert_eq!(header.sample_rate_hz, 44100);
   }

   #[test]
   fn test_parse_header_mpeg1_l3_48khz() {
      // MPEG1 Layer3, 128kbps, 48000Hz
      let bytes = [0xFF, 0xFB, 0x94, 0x00];
      let header = parse_header(bytes).expect("should parse");

      assert_eq!(header.sample_rate_hz, 48000);
   }

   #[test]
   fn test_parse_header_invalid_sync() {
      let bytes = [0x00, 0x00, 0x00, 0x00];
      assert!(parse_header(bytes).is_none());

      let bytes = [0xFF, 0x00, 0x00, 0x00];
      assert!(parse_header(bytes).is_none());
   }

   #[test]
   fn test_parse_header_reserved_values() {
      // Reserved layer (0b00)
      let bytes = [0xFF, 0xF1, 0x90, 0x00];
      assert!(parse_header(bytes).is_none());

      // Reserved version (0b01)
      let bytes = [0xFF, 0xEB, 0x90, 0x00];
      assert!(parse_header(bytes).is_none());
   }

   #[test]
   fn test_frame_header_size() {
      let header = FrameHeader {
         version: MpegVersion::V1,
         layer: MpegLayer::Layer3,
         bitrate_kbps: 128,
         sample_rate_hz: 44100,
         padding: false,
         channel_mode: 0,
      };

      assert_eq!(header.size(), Some(417));
      assert_eq!(header.samples_per_frame(), 1152);
   }

   #[test]
   fn test_frame_header_duration() {
      let header = FrameHeader {
         version: MpegVersion::V1,
         layer: MpegLayer::Layer3,
         bitrate_kbps: 128,
         sample_rate_hz: 44100,
         padding: false,
         channel_mode: 0,
      };

      // 1152 samples at 44100Hz = ~26.12ms
      assert_eq!(header.duration_ms(), 26);
      assert_eq!(header.duration_us(), 26122);
   }

   #[test]
   fn test_frame_header_xing_offset() {
      let stereo = FrameHeader {
         version: MpegVersion::V1,
         layer: MpegLayer::Layer3,
         bitrate_kbps: 128,
         sample_rate_hz: 44100,
         padding: false,
         channel_mode: 0, // stereo
      };
      assert_eq!(stereo.xing_offset(), 32);

      let mono = FrameHeader {
         version: MpegVersion::V1,
         layer: MpegLayer::Layer3,
         bitrate_kbps: 128,
         sample_rate_hz: 44100,
         padding: false,
         channel_mode: 3, // mono
      };
      assert_eq!(mono.xing_offset(), 17);
   }

   #[test]
   fn test_frame_parse_result_methods() {
      let found = FrameParseResult::Found {
         header: FrameHeader {
            version: MpegVersion::V1,
            layer: MpegLayer::Layer3,
            bitrate_kbps: 128,
            sample_rate_hz: 44100,
            padding: false,
            channel_mode: 0,
         },
         offset: 100,
      };

      assert!(found.is_found());
      assert_eq!(found.offset(), Some(100));
      assert!(found.header().is_some());

      let not_found = FrameParseResult::NotFound;
      assert!(!not_found.is_found());
      assert_eq!(not_found.offset(), None);
      assert!(not_found.header().is_none());
   }

   #[tokio::test]
   async fn test_find_first_frame_returns_not_found_after_scanning_window() {
      let reader = BytesReader(vec![0; 16]);

      assert!(matches!(
         find_first_frame(&reader, 0, 16).await.unwrap(),
         FrameParseResult::NotFound
      ));
   }

   #[tokio::test]
   async fn test_find_first_frame_returns_end_of_data_for_short_input() {
      let reader = BytesReader(vec![0; FRAME_HEADER_SIZE - 1]);

      assert!(matches!(
         find_first_frame(&reader, 0, 16).await.unwrap(),
         FrameParseResult::EndOfData
      ));
   }

   #[tokio::test]
   async fn test_find_first_frame_propagates_reader_errors() {
      assert!(matches!(
         find_first_frame(&FailingReader, 0, 16).await,
         Err(crate::errors::MediaParserError::Other(message)) if message == "read failure"
      ));
   }
}

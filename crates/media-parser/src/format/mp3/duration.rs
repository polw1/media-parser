//! Duration calculation.
//!
//! Provides multiple options for calculating MP3 duration:
//! - VBR: Uses Xing/VBRI header information
//! - CBR: Calculates from file size and bitrate
//! - Auto: Tries VBR first, falls back to CBR

use super::frame::{
   FRAME_HEADER_SIZE, FrameHeader, FrameParseResult, MAX_SYNC_SEARCH, find_first_frame,
};
use crate::Result;
use crate::helpers::read_u32_be;
use crate::stream::StreamReader;

/// Duration calculation result.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Duration {
   /// Duration in milliseconds.
   pub millis: u64,
   /// Method used for calculation.
   pub method: DurationMethod,
}

impl Duration {
   /// Creates a new duration value.
   pub const fn new(millis: u64, method: DurationMethod) -> Self {
      Self { millis, method }
   }

   /// Returns duration in seconds as floating point.
   pub fn seconds(&self) -> f64 {
      self.millis as f64 / 1000.0
   }

   /// Returns zero duration with estimated method.
   pub const fn zero() -> Self {
      Self {
         millis: 0,
         method: DurationMethod::Estimated,
      }
   }
}

/// Method used for duration calculation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DurationMethod {
   /// Calculated from Xing/VBRI header (exact for VBR files).
   VbrHeader,
   /// Calculated from file size and bitrate (exact for CBR files).
   Cbr,
   /// Estimated (fallback when other methods fail).
   Estimated,
}

/// VBR header information (Xing or VBRI).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct VbrInfo {
   /// Total number of frames in file.
   pub total_frames: u32,
   /// Total bytes of audio data (optional).
   pub total_bytes: Option<u32>,
   /// VBR header type.
   pub header_type: VbrHeaderType,
   /// Encoder delay in samples (from LAME/Lavc tag).
   pub enc_delay: u16,
   /// Encoder padding in samples (from LAME/Lavc tag).
   pub enc_padding: u16,
}

/// Type of VBR header found.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum VbrHeaderType {
   /// Xing header (common in LAME encoded files).
   Xing,
   /// Info header (CBR files encoded with LAME).
   Info,
   /// VBRI header (Fraunhofer encoder).
   Vbri,
}

/// Trait for duration calculation options.
///
/// Enables polymorphism and dependency injection for different
/// calculation methods.
pub trait DurationStrategy: Send + Sync {
   /// Calculates duration from available information.
   ///
   /// # Parameters
   ///
   /// - `first_frame`: Parsed header of first audio frame
   /// - `audio_start`: Byte offset where audio data starts
   /// - `file_size`: Total file size in bytes
   /// - `vbr_info`: Optional VBR header information
   fn calculate(
      &self,
      first_frame: &FrameHeader,
      audio_start: u64,
      file_size: u64,
      vbr_info: Option<&VbrInfo>,
   ) -> Duration;
}

/// CBR (Constant Bit Rate) duration strategy.
///
/// Calculates duration from file size and bitrate:
/// `duration = audio_bytes * 8 / bitrate`
pub struct CbrStrategy;

impl DurationStrategy for CbrStrategy {
   fn calculate(
      &self,
      first_frame: &FrameHeader,
      audio_start: u64,
      file_size: u64,
      _vbr_info: Option<&VbrInfo>,
   ) -> Duration {
      if first_frame.bitrate_kbps == 0 || file_size <= audio_start {
         return Duration::zero();
      }

      let audio_bytes = file_size - audio_start;
      let millis = (audio_bytes * 8) / first_frame.bitrate_kbps as u64;

      Duration::new(millis, DurationMethod::Cbr)
   }
}

/// VBR (Variable Bit Rate) duration strategy.
///
/// Calculates duration from Xing/VBRI header:
/// `duration = total_frames * samples_per_frame / sample_rate`
pub struct VbrStrategy;

impl DurationStrategy for VbrStrategy {
   fn calculate(
      &self,
      first_frame: &FrameHeader,
      _audio_start: u64,
      _file_size: u64,
      vbr_info: Option<&VbrInfo>,
   ) -> Duration {
      let info = match vbr_info {
         Some(i) => i,
         None => return Duration::zero(),
      };

      if first_frame.sample_rate_hz == 0 || info.total_frames == 0 {
         return Duration::zero();
      }

      let samples_per_frame = first_frame.samples_per_frame() as u64;
      let raw_samples = info.total_frames as u64 * samples_per_frame;
      let gapless_trim = info.enc_delay as u64 + info.enc_padding as u64;
      let total_samples = raw_samples.saturating_sub(gapless_trim);
      let millis = (total_samples * 1000) / first_frame.sample_rate_hz as u64;

      Duration::new(millis, DurationMethod::VbrHeader)
   }
}

/// Auto strategy: tries VBR first, falls back to CBR.
///
/// This is the recommended strategy for general use.
pub struct AutoStrategy;

impl DurationStrategy for AutoStrategy {
   fn calculate(
      &self,
      first_frame: &FrameHeader,
      audio_start: u64,
      file_size: u64,
      vbr_info: Option<&VbrInfo>,
   ) -> Duration {
      match vbr_info {
         Some(info) if info.total_frames > 0 => {
            VbrStrategy.calculate(first_frame, audio_start, file_size, Some(info))
         }
         _ => CbrStrategy.calculate(first_frame, audio_start, file_size, None),
      }
   }
}

/// Xing/Info header flags.
const XING_FLAG_FRAMES: u32 = 0x0001;
const XING_FLAG_BYTES: u32 = 0x0002;
const XING_FLAG_TOC: u32 = 0x0004;
const XING_FLAG_QUALITY: u32 = 0x0008;

/// Minimum bytes from LAME tag start needed to read delay/padding (offset 21 + 3 bytes).
const LAME_TAG_MIN_SIZE: usize = 24;

/// Max Xing header (all flags) + LAME tag: 4+4+4+4+100+4 + 36 = 156.
const XING_PLUS_LAME_MAX: usize = 156;

/// Parses Xing/Info/VBRI header from frame data.
///
/// # Parameters
///
/// - `reader`: Stream reader for I/O
/// - `frame_offset`: Byte offset of first frame
/// - `header`: Parsed frame header (for calculating Xing offset)
///
/// # Returns
///
/// `Some(VbrInfo)` if VBR header found, `None` otherwise.
pub async fn parse_vbr_header(
   reader: &dyn StreamReader,
   frame_offset: u64,
   header: &FrameHeader,
) -> Option<VbrInfo> {
   // Try Xing/Info header first
   if let Some(info) = parse_xing_header(reader, frame_offset, header).await {
      return Some(info);
   }

   // Try VBRI header (always at fixed offset 32 from frame start)
   parse_vbri_header(reader, frame_offset).await
}

/// Parses Xing or Info header, including LAME/Lavc gapless info.
async fn parse_xing_header(
   reader: &dyn StreamReader,
   frame_offset: u64,
   header: &FrameHeader,
) -> Option<VbrInfo> {
   // Xing header starts after side info
   let xing_offset = frame_offset + FRAME_HEADER_SIZE as u64 + header.xing_offset() as u64;

   let mut buffer = [0u8; XING_PLUS_LAME_MAX];
   let bytes_read = reader.read_at(xing_offset, &mut buffer).await.ok()?;

   // Minimum: 4 (ID) + 4 (flags) = 8 bytes
   if bytes_read < 8 {
      return None;
   }

   // Check for Xing or Info mark
   let header_type = if &buffer[0..4] == b"Xing" {
      VbrHeaderType::Xing
   } else if &buffer[0..4] == b"Info" {
      VbrHeaderType::Info
   } else {
      return None;
   };

   let flags = read_u32_be(&buffer[..bytes_read], 4)?;

   // Walk pos through each optional field based on flags
   let mut pos = 8;
   let mut total_frames = 0u32;
   let mut total_bytes = None;

   if flags & XING_FLAG_FRAMES != 0 {
      total_frames = read_u32_be(&buffer[..bytes_read], pos)?;
      pos += 4;
   }

   if flags & XING_FLAG_BYTES != 0 {
      total_bytes = read_u32_be(&buffer[..bytes_read], pos);
      pos += 4;
   }

   if flags & XING_FLAG_TOC != 0 {
      pos += 100;
   }

   if flags & XING_FLAG_QUALITY != 0 {
      pos += 4;
   }

   if total_frames == 0 {
      return None;
   }

   // Try to parse LAME/Lavc tag at pos (right after last Xing field)
   let (enc_delay, enc_padding) =
      parse_lame_gapless(&buffer, bytes_read, pos, total_frames, header);

   Some(VbrInfo {
      total_frames,
      total_bytes,
      header_type,
      enc_delay,
      enc_padding,
   })
}

/// Parses encoder delay/padding from LAME/Lavc tag.
///
/// Two 12-bit values packed in 3 bytes at offset 21 from tag start.
/// See: http://gabriel.mp3-tech.org/mp3infotag.html
fn parse_lame_gapless(
   buffer: &[u8],
   bytes_read: usize,
   lame_start: usize,
   total_frames: u32,
   header: &FrameHeader,
) -> (u16, u16) {
   let needed = match lame_start.checked_add(LAME_TAG_MIN_SIZE) {
      Some(v) => v,
      None => return (0, 0),
   };
   if bytes_read < needed {
      return (0, 0);
   }

   let tag = &buffer[lame_start..];

   let is_known_encoder = tag.starts_with(b"LAME")
      || tag.starts_with(b"Lavc")
      || tag.starts_with(b"Lavf")
      || tag.starts_with(b"L3.9");

   if !is_known_encoder {
      return (0, 0);
   }

   let delay = ((tag[21] as u16) << 4) | ((tag[22] as u16) >> 4);
   let padding = (((tag[22] & 0x0F) as u16) << 8) | (tag[23] as u16);

   // Semantic validation: trim must not exceed total raw samples
   let raw_samples = total_frames as u64 * header.samples_per_frame() as u64;
   if delay as u64 + padding as u64 > raw_samples {
      return (0, 0);
   }

   (delay, padding)
}

/// Parses VBRI header (Fraunhofer format).
async fn parse_vbri_header(reader: &dyn StreamReader, frame_offset: u64) -> Option<VbrInfo> {
   // VBRI header is always at offset 32 from frame start (after 4-byte header)
   let vbri_offset = frame_offset + FRAME_HEADER_SIZE as u64 + 32;

   let mut buffer = [0u8; 26];
   let bytes_read = reader.read_at(vbri_offset, &mut buffer).await.ok()?;

   if bytes_read < 26 {
      return None;
   }

   // Check VBRI mark
   if &buffer[0..4] != b"VBRI" {
      return None;
   }

   // Version at offset 4 (2 bytes)
   // Delay at offset 6 (2 bytes)
   // Quality at offset 8 (2 bytes)
   // Bytes at offset 10 (4 bytes)
   let total_bytes = read_u32_be(&buffer, 10)?;

   // Frames at offset 14 (4 bytes)
   let total_frames = read_u32_be(&buffer, 14)?;

   if total_frames == 0 {
      return None;
   }

   Some(VbrInfo {
      total_frames,
      total_bytes: Some(total_bytes),
      header_type: VbrHeaderType::Vbri,
      enc_delay: 0,
      enc_padding: 0,
   })
}

/// Calculates MP3 duration using auto option.
///
/// This is the main entry point for duration calculation.
/// Composes frame finding, VBR parsing, and strategy selection.
///
/// # Parameters
///
/// - `reader`: Stream reader for file I/O
/// - `id3_size`: Size of ID3 tag to skip (0 if no ID3)
///
/// # Returns
///
/// `Duration` with calculated milliseconds and method used.
///
/// # Example
///
/// ```no_run
/// use media_parser::format::mp3::duration::calculate_duration;
///
/// async fn example(reader: &dyn media_parser::StreamReader) {
///     let duration = calculate_duration(reader, 0).await.unwrap();
///     println!("Duration: {} seconds", duration.seconds());
/// }
/// ```
pub async fn calculate_duration(reader: &dyn StreamReader, id3_size: u64) -> Result<Duration> {
   calculate_duration_with_strategy(reader, id3_size, &AutoStrategy).await
}

/// Calculates duration using a specific option.
///
/// Allows dependency injection of custom options for testing
/// or specialized use cases.
pub async fn calculate_duration_with_strategy<S: DurationStrategy>(
   reader: &dyn StreamReader,
   id3_size: u64,
   strategy: &S,
) -> Result<Duration> {
   // Get file size
   let file_size = reader.size().await?;

   if file_size <= id3_size {
      return Ok(Duration::zero());
   }

   // Find first audio frame
   let frame_result = find_first_frame(reader, id3_size, MAX_SYNC_SEARCH).await?;

   let (header, frame_offset) = match frame_result {
      FrameParseResult::Found { header, offset } => (header, offset),
      _ => return Ok(Duration::zero()),
   };

   // Try to parse VBR header
   let vbr_info = parse_vbr_header(reader, frame_offset, &header).await;

   // Calculate duration using strategy
   let duration = strategy.calculate(&header, frame_offset, file_size, vbr_info.as_ref());

   Ok(duration)
}

#[cfg(test)]
mod tests {
   use super::*;

   fn make_header(bitrate: u16, sample_rate: u32) -> FrameHeader {
      use super::super::tables::{MpegLayer, MpegVersion};
      FrameHeader {
         version: MpegVersion::V1,
         layer: MpegLayer::Layer3,
         bitrate_kbps: bitrate,
         sample_rate_hz: sample_rate,
         padding: false,
         channel_mode: 0,
      }
   }

   #[test]
   fn test_cbr_strategy() {
      let header = make_header(128, 44100);
      let strategy = CbrStrategy;

      // 1MB of audio at 128kbps = ~62.5 seconds
      let duration = strategy.calculate(&header, 0, 1_000_000, None);

      assert_eq!(duration.method, DurationMethod::Cbr);
      // 1_000_000 * 8 * 1000 / 128000 = 62500 ms
      assert_eq!(duration.millis, 62500);
   }

   #[test]
   fn test_cbr_strategy_with_id3() {
      let header = make_header(128, 44100);
      let strategy = CbrStrategy;

      // 1MB file, 10KB ID3 tag
      let duration = strategy.calculate(&header, 10_000, 1_000_000, None);

      // (1_000_000 - 10_000) * 8 * 1000 / 128000 = 61875 ms
      assert_eq!(duration.millis, 61875);
   }

   #[test]
   fn test_vbr_strategy() {
      let header = make_header(128, 44100);
      let strategy = VbrStrategy;

      let vbr_info = VbrInfo {
         total_frames: 10000,
         total_bytes: None,
         header_type: VbrHeaderType::Xing,
         enc_delay: 0,
         enc_padding: 0,
      };

      let duration = strategy.calculate(&header, 0, 0, Some(&vbr_info));

      assert_eq!(duration.method, DurationMethod::VbrHeader);
      // 10000 frames * 1152 samples / 44100 = ~261.22 seconds = 261224 ms
      assert_eq!(duration.millis, 261224);
   }

   #[test]
   fn test_vbr_strategy_no_info() {
      let header = make_header(128, 44100);
      let strategy = VbrStrategy;

      let duration = strategy.calculate(&header, 0, 1_000_000, None);

      assert_eq!(duration.method, DurationMethod::Estimated);
      assert_eq!(duration.millis, 0);
   }

   #[test]
   fn test_auto_strategy_with_vbr() {
      let header = make_header(128, 44100);
      let strategy = AutoStrategy;

      let vbr_info = VbrInfo {
         total_frames: 10000,
         total_bytes: None,
         header_type: VbrHeaderType::Xing,
         enc_delay: 0,
         enc_padding: 0,
      };

      let duration = strategy.calculate(&header, 0, 1_000_000, Some(&vbr_info));

      // Should use VBR method
      assert_eq!(duration.method, DurationMethod::VbrHeader);
   }

   #[test]
   fn test_auto_strategy_without_vbr() {
      let header = make_header(128, 44100);
      let strategy = AutoStrategy;

      let duration = strategy.calculate(&header, 0, 1_000_000, None);

      // Should fall back to CBR method
      assert_eq!(duration.method, DurationMethod::Cbr);
   }

   #[test]
   fn test_duration_seconds() {
      let duration = Duration::new(62500, DurationMethod::Cbr);
      assert!((duration.seconds() - 62.5).abs() < 0.001);
   }

   #[test]
   fn test_duration_zero() {
      let duration = Duration::zero();
      assert_eq!(duration.millis, 0);
      assert_eq!(duration.method, DurationMethod::Estimated);
   }
}

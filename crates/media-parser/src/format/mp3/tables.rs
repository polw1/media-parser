//! MPEG audio lookup tables as pure functions.
//!
//! Provides constants and lookup functions for MPEG audio parameters.
//! All functions are `const fn` for compile-time evaluation.

/// MPEG audio version.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MpegVersion {
   /// MPEG Version 1
   V1,
   /// MPEG Version 2
   V2,
   /// MPEG Version 2.5 (unofficial extension)
   V25,
}

impl std::fmt::Display for MpegVersion {
   fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
      f.write_str(match self {
         Self::V1 => "V1",
         Self::V2 => "V2",
         Self::V25 => "V25",
      })
   }
}

/// MPEG audio layer.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MpegLayer {
   /// Layer I
   Layer1,
   /// Layer II
   Layer2,
   /// Layer III
   Layer3,
}

impl std::fmt::Display for MpegLayer {
   fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
      f.write_str(match self {
         Self::Layer1 => "Layer1",
         Self::Layer2 => "Layer2",
         Self::Layer3 => "Layer3",
      })
   }
}

/// Bitrate table for MPEG1 Layer 1 (kbps).
/// Index 0 = free, Index 15 = bad.
const BITRATE_V1_L1: [u16; 16] = [
   0, 32, 64, 96, 128, 160, 192, 224, 256, 288, 320, 352, 384, 416, 448, 0,
];

/// Bitrate table for MPEG1 Layer 2 (kbps).
const BITRATE_V1_L2: [u16; 16] = [
   0, 32, 48, 56, 64, 80, 96, 112, 128, 160, 192, 224, 256, 320, 384, 0,
];

/// Bitrate table for MPEG1 Layer 3 (kbps).
const BITRATE_V1_L3: [u16; 16] = [
   0, 32, 40, 48, 56, 64, 80, 96, 112, 128, 160, 192, 224, 256, 320, 0,
];

/// Bitrate table for MPEG2/2.5 Layer 1 (kbps).
const BITRATE_V2_L1: [u16; 16] = [
   0, 32, 48, 56, 64, 80, 96, 112, 128, 144, 160, 176, 192, 224, 256, 0,
];

/// Bitrate table for MPEG2/2.5 Layer 2/3 (kbps).
const BITRATE_V2_L23: [u16; 16] = [
   0, 8, 16, 24, 32, 40, 48, 56, 64, 80, 96, 112, 128, 144, 160, 0,
];

/// Sample rate table for MPEG1 (Hz).
const SAMPLE_RATE_V1: [u32; 4] = [44100, 48000, 32000, 0];

/// Sample rate table for MPEG2 (Hz).
const SAMPLE_RATE_V2: [u32; 4] = [22050, 24000, 16000, 0];

/// Sample rate table for MPEG2.5 (Hz).
const SAMPLE_RATE_V25: [u32; 4] = [11025, 12000, 8000, 0];

/// Returns bitrate in kbps for given version, layer, and index.
///
/// # Returns
///
/// - `Some(bitrate)` for valid combinations
/// - `None` for reserved indices (0 or 15) or invalid combinations
///
/// # Example
///
/// ```no_run
/// use media_parser::format::mp3::tables::{MpegVersion, MpegLayer, bitrate_kbps};
///
/// let bitrate = bitrate_kbps(MpegVersion::V1, MpegLayer::Layer3, 9);
/// assert_eq!(bitrate, Some(128));
/// ```
pub const fn bitrate_kbps(version: MpegVersion, layer: MpegLayer, index: u8) -> Option<u16> {
   if index == 0 || index >= 15 {
      return None;
   }

   let table = match (version, layer) {
      (MpegVersion::V1, MpegLayer::Layer1) => &BITRATE_V1_L1,
      (MpegVersion::V1, MpegLayer::Layer2) => &BITRATE_V1_L2,
      (MpegVersion::V1, MpegLayer::Layer3) => &BITRATE_V1_L3,
      (MpegVersion::V2 | MpegVersion::V25, MpegLayer::Layer1) => &BITRATE_V2_L1,
      (MpegVersion::V2 | MpegVersion::V25, MpegLayer::Layer2 | MpegLayer::Layer3) => {
         &BITRATE_V2_L23
      }
   };

   let value = table[index as usize];
   if value == 0 { None } else { Some(value) }
}

/// Returns sample rate in Hz for given version and index.
///
/// # Returns
///
/// - `Some(sample_rate)` for valid indices (0-2)
/// - `None` for reserved index (3)
///
/// # Example
///
/// ```no_run
/// use media_parser::format::mp3::tables::{MpegVersion, sample_rate_hz};
///
/// let rate = sample_rate_hz(MpegVersion::V1, 0);
/// assert_eq!(rate, Some(44100));
/// ```
pub const fn sample_rate_hz(version: MpegVersion, index: u8) -> Option<u32> {
   if index >= 3 {
      return None;
   }

   let table = match version {
      MpegVersion::V1 => &SAMPLE_RATE_V1,
      MpegVersion::V2 => &SAMPLE_RATE_V2,
      MpegVersion::V25 => &SAMPLE_RATE_V25,
   };

   let value = table[index as usize];
   if value == 0 { None } else { Some(value) }
}

/// Returns samples per frame for given version and layer.
///
/// # Example
///
/// ```no_run
/// use media_parser::format::mp3::tables::{MpegVersion, MpegLayer, samples_per_frame};
///
/// let samples = samples_per_frame(MpegVersion::V1, MpegLayer::Layer3);
/// assert_eq!(samples, 1152);
/// ```
pub const fn samples_per_frame(version: MpegVersion, layer: MpegLayer) -> u32 {
   match (version, layer) {
      (MpegVersion::V1, MpegLayer::Layer1) => 384,
      (MpegVersion::V1, MpegLayer::Layer2) => 1152,
      (MpegVersion::V1, MpegLayer::Layer3) => 1152,
      (MpegVersion::V2 | MpegVersion::V25, MpegLayer::Layer1) => 384,
      (MpegVersion::V2 | MpegVersion::V25, MpegLayer::Layer2) => 1152,
      (MpegVersion::V2 | MpegVersion::V25, MpegLayer::Layer3) => 576,
   }
}

/// Returns slot size in bytes for given layer.
///
/// Layer 1 uses 4-byte slots, Layers 2/3 use 1-byte slots.
pub const fn slot_size(layer: MpegLayer) -> usize {
   match layer {
      MpegLayer::Layer1 => 4,
      MpegLayer::Layer2 | MpegLayer::Layer3 => 1,
   }
}

/// Calculates frame size in bytes.
///
/// # Formula
///
/// - Layer 1: `(12 * bitrate / sample_rate + padding) * 4`
/// - Layer 2/3: `144 * bitrate / sample_rate + padding` (MPEG1)
/// - Layer 3: `72 * bitrate / sample_rate + padding` (MPEG2/2.5)
///
/// # Example
///
/// ```no_run
/// use media_parser::format::mp3::tables::{MpegVersion, MpegLayer, frame_size};
///
/// // 128kbps, 44100Hz, no padding -> 417 bytes
/// let size = frame_size(MpegVersion::V1, MpegLayer::Layer3, 128, 44100, false);
/// assert_eq!(size, Some(417));
/// ```
pub const fn frame_size(
   version: MpegVersion,
   layer: MpegLayer,
   bitrate_kbps: u16,
   sample_rate_hz: u32,
   padding: bool,
) -> Option<usize> {
   if sample_rate_hz == 0 || bitrate_kbps == 0 {
      return None;
   }

   let bitrate = bitrate_kbps as usize * 1000;
   let sample_rate = sample_rate_hz as usize;
   let pad = if padding { slot_size(layer) } else { 0 };

   let size = match layer {
      MpegLayer::Layer1 => (12 * bitrate / sample_rate) * 4 + pad,
      MpegLayer::Layer2 => 144 * bitrate / sample_rate + pad,
      MpegLayer::Layer3 => {
         let coefficient = match version {
            MpegVersion::V1 => 144,
            MpegVersion::V2 | MpegVersion::V25 => 72,
         };
         coefficient * bitrate / sample_rate + pad
      }
   };

   Some(size)
}

#[cfg(test)]
mod tests {
   use super::*;

   #[test]
   fn test_bitrate_v1_l3() {
      assert_eq!(bitrate_kbps(MpegVersion::V1, MpegLayer::Layer3, 0), None);
      assert_eq!(
         bitrate_kbps(MpegVersion::V1, MpegLayer::Layer3, 1),
         Some(32)
      );
      assert_eq!(
         bitrate_kbps(MpegVersion::V1, MpegLayer::Layer3, 9),
         Some(128)
      );
      assert_eq!(
         bitrate_kbps(MpegVersion::V1, MpegLayer::Layer3, 14),
         Some(320)
      );
      assert_eq!(bitrate_kbps(MpegVersion::V1, MpegLayer::Layer3, 15), None);
   }

   #[test]
   fn test_bitrate_v2_l3() {
      assert_eq!(
         bitrate_kbps(MpegVersion::V2, MpegLayer::Layer3, 5),
         Some(40)
      );
      assert_eq!(
         bitrate_kbps(MpegVersion::V25, MpegLayer::Layer3, 5),
         Some(40)
      );
   }

   #[test]
   fn test_sample_rate() {
      assert_eq!(sample_rate_hz(MpegVersion::V1, 0), Some(44100));
      assert_eq!(sample_rate_hz(MpegVersion::V1, 1), Some(48000));
      assert_eq!(sample_rate_hz(MpegVersion::V1, 2), Some(32000));
      assert_eq!(sample_rate_hz(MpegVersion::V1, 3), None);

      assert_eq!(sample_rate_hz(MpegVersion::V2, 0), Some(22050));
      assert_eq!(sample_rate_hz(MpegVersion::V25, 0), Some(11025));
   }

   #[test]
   fn test_samples_per_frame() {
      assert_eq!(samples_per_frame(MpegVersion::V1, MpegLayer::Layer1), 384);
      assert_eq!(samples_per_frame(MpegVersion::V1, MpegLayer::Layer3), 1152);
      assert_eq!(samples_per_frame(MpegVersion::V2, MpegLayer::Layer3), 576);
   }

   #[test]
   fn test_frame_size() {
      // MPEG1 Layer3, 128kbps, 44100Hz, no padding = 417 bytes
      assert_eq!(
         frame_size(MpegVersion::V1, MpegLayer::Layer3, 128, 44100, false),
         Some(417)
      );

      // MPEG1 Layer3, 128kbps, 44100Hz, with padding = 418 bytes
      assert_eq!(
         frame_size(MpegVersion::V1, MpegLayer::Layer3, 128, 44100, true),
         Some(418)
      );

      // MPEG1 Layer3, 320kbps, 44100Hz, no padding = 1044 bytes
      assert_eq!(
         frame_size(MpegVersion::V1, MpegLayer::Layer3, 320, 44100, false),
         Some(1044)
      );
   }
}

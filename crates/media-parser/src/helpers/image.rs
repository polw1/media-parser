//! Image format detection from magic bytes.

use crate::types::PixelFormat;

const JPEG_MAGIC: [u8; 3] = [0xff, 0xd8, 0xff];
const PNG_MAGIC: [u8; 8] = [0x89, b'P', b'N', b'G', 0x0d, 0x0a, 0x1a, 0x0a];

/// Detects the image format of `data` from its magic bytes.
///
/// Returns `Some(PixelFormat::Jpeg)` for JPEG data, `Some(PixelFormat::Png)`
/// for PNG data, and `None` for anything else (including too-short input).
pub fn detect_image_format(data: &[u8]) -> Option<PixelFormat> {
   if data.starts_with(&JPEG_MAGIC) {
      Some(PixelFormat::Jpeg)
   } else if data.starts_with(&PNG_MAGIC) {
      Some(PixelFormat::Png)
   } else {
      None
   }
}

#[cfg(test)]
mod tests {
   use super::*;

   #[test]
   fn test_detect_jpeg() {
      let data = [0xff, 0xd8, 0xff, 0xe0, 0x00, 0x10];
      assert_eq!(detect_image_format(&data), Some(PixelFormat::Jpeg));
   }

   #[test]
   fn test_detect_png() {
      let data = [0x89, b'P', b'N', b'G', 0x0d, 0x0a, 0x1a, 0x0a, 0x00];
      assert_eq!(detect_image_format(&data), Some(PixelFormat::Png));
   }

   #[test]
   fn test_detect_neither() {
      let data = [0x47, 0x49, 0x46, 0x38, 0x39, 0x61];
      assert_eq!(detect_image_format(&data), None);
   }

   #[test]
   fn test_detect_too_short() {
      assert_eq!(detect_image_format(&[]), None);
      assert_eq!(detect_image_format(&[0xff]), None);
      assert_eq!(detect_image_format(&[0xff, 0xd8]), None);
      assert_eq!(detect_image_format(&[0x89, b'P', b'N']), None);
   }
}

//! Embedded MP4 cover-art parsing.

use super::Mp4Nav;
use crate::helpers::{detect_image_format, read_u32_be};
use crate::types::{CoverArt, PixelFormat};

pub fn parse_cover_art(moov_payload: &[u8]) -> Option<CoverArt> {
   let meta = moov_payload.nav(&[*b"udta", *b"meta"])?;
   let covr = meta
      .get(4..)
      .and_then(|payload| payload.nav(&[*b"ilst", *b"covr"]))
      .or_else(|| meta.nav(&[*b"ilst", *b"covr"]))?;
   let data = covr.nav(&[*b"data"])?;
   let image = data.get(8..)?;
   if image.is_empty() {
      return None;
   }

   let format = match read_u32_be(data, 0)? {
      13 => PixelFormat::Jpeg,
      14 => PixelFormat::Png,
      _ => detect_image_format(image)?,
   };

   let mut image_data = Vec::new();
   image_data.try_reserve_exact(image.len()).ok()?;
   image_data.extend_from_slice(image);

   Some(CoverArt {
      mime_type: format.mime_type().to_string(),
      format,
      data: image_data,
   })
}

#[cfg(test)]
mod tests {
   use super::*;

   fn mp4_box(fourcc: &[u8; 4], payload: &[u8]) -> Vec<u8> {
      let mut result = Vec::with_capacity(8 + payload.len());
      result.extend_from_slice(&u32::try_from(8 + payload.len()).unwrap().to_be_bytes());
      result.extend_from_slice(fourcc);
      result.extend_from_slice(payload);
      result
   }

   fn cover_payload(data_payload: &[u8]) -> Vec<u8> {
      let data = mp4_box(b"data", data_payload);
      let covr = mp4_box(b"covr", &data);
      let ilst = mp4_box(b"ilst", &covr);
      let mut meta_payload = vec![0; 4];
      meta_payload.extend_from_slice(&ilst);
      let meta = mp4_box(b"meta", &meta_payload);
      mp4_box(b"udta", &meta)
   }

   #[test]
   fn detects_cover_format_from_magic_bytes_when_data_type_is_unknown() {
      let png = [0x89, b'P', b'N', b'G', 0x0d, 0x0a, 0x1a, 0x0a, 1];
      let mut data_payload = vec![0; 8];
      data_payload.extend_from_slice(&png);

      let cover = parse_cover_art(&cover_payload(&data_payload)).expect("cover should parse");

      assert_eq!(cover.format, PixelFormat::Png);
      assert_eq!(cover.mime_type, "image/png");
      assert_eq!(cover.data, png);
   }

   #[test]
   fn rejects_missing_or_truncated_cover_structure() {
      assert_eq!(parse_cover_art(&[]), None);
      assert_eq!(parse_cover_art(&cover_payload(&[0; 7])), None);
   }
}

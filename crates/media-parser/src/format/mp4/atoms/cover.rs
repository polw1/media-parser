//! Embedded MP4 cover-art parsing.

use super::Mp4Nav;
use crate::helpers::read_u32_be;
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
      _ if image.starts_with(&[0xff, 0xd8, 0xff]) => PixelFormat::Jpeg,
      _ if image.starts_with(&[0x89, b'P', b'N', b'G', 0x0d, 0x0a, 0x1a, 0x0a]) => PixelFormat::Png,
      _ => return None,
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

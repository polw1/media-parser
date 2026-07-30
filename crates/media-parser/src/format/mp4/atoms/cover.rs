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

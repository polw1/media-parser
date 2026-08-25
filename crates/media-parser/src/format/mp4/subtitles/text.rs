use super::super::atoms;
use crate::MediaParserError;
use std::char::decode_utf16;

#[derive(Debug)]
pub(super) enum DecodeSampleError {
   Track(MediaParserError),
   Fatal(MediaParserError),
}

pub(super) fn decode_sample(
   codec: &str,
   data: &[u8],
) -> std::result::Result<Option<String>, DecodeSampleError> {
   decode_sample_bounded(codec, data, None)
}

#[cfg(test)]
fn decode_sample_with_limit(
   codec: &str,
   data: &[u8],
   max_capacity: usize,
) -> std::result::Result<Option<String>, DecodeSampleError> {
   decode_sample_bounded(codec, data, Some(max_capacity))
}

fn decode_sample_bounded(
   codec: &str,
   data: &[u8],
   capacity_limit: Option<usize>,
) -> std::result::Result<Option<String>, DecodeSampleError> {
   if !matches!(codec, "tx3g" | "wvtt" | "stpp" | "text") {
      return unsupported_codec(codec, capacity_limit);
   }

   let mut output = sample_text_builder(data, capacity_limit)?;
   decode_supported_sample(codec, data, &mut output)?;
   Ok(output.finish())
}

fn sample_text_builder(
   data: &[u8],
   capacity_limit: Option<usize>,
) -> std::result::Result<TextBuilder, DecodeSampleError> {
   let natural_ceiling = data.len().checked_mul(3).ok_or_else(capacity_error)?;
   let max_capacity = capacity_limit.map_or(natural_ceiling, |limit| limit.min(natural_ceiling));
   Ok(TextBuilder::new(max_capacity))
}

fn decode_supported_sample(
   codec: &str,
   data: &[u8],
   output: &mut TextBuilder,
) -> std::result::Result<(), DecodeSampleError> {
   match codec {
      "tx3g" | "text" => {
         let length_bytes = data.get(..2).ok_or_else(invalid_subtitle)?;
         let declared_length = usize::from(u16::from_be_bytes([length_bytes[0], length_bytes[1]]));
         let text_end = 2_usize
            .checked_add(declared_length)
            .ok_or_else(invalid_subtitle)?;
         let text = data.get(2..text_end).ok_or_else(invalid_subtitle)?;
         output.append_decoded(text)?;
      }
      "wvtt" => decode_wvtt(data, output)?,
      "stpp" => {
         output.append_decoded(data)?;
      }
      _ => unreachable!("unsupported codecs return before sample text allocation"),
   }
   Ok(())
}

#[cfg(test)]
fn decode_sample_with_reservation_count(
   codec: &str,
   data: &[u8],
) -> std::result::Result<(Option<String>, usize), DecodeSampleError> {
   let mut output = sample_text_builder(data, None)?;
   decode_supported_sample(codec, data, &mut output)?;
   let reservation_count = output.reservation_count();
   Ok((output.finish(), reservation_count))
}

fn decode_wvtt(
   data: &[u8],
   output: &mut TextBuilder,
) -> std::result::Result<(), DecodeSampleError> {
   walk_boxes(data, |fourcc, payload| match &fourcc {
      b"payl" => append_wvtt_payload(output, payload),
      b"vttc" => walk_boxes(payload, |child_fourcc, child_payload| {
         if &child_fourcc == b"payl" {
            append_wvtt_payload(output, child_payload)?;
         }
         Ok(())
      }),
      _ => Ok(()),
   })
}

fn walk_boxes(
   data: &[u8],
   mut visit: impl FnMut([u8; 4], &[u8]) -> std::result::Result<(), DecodeSampleError>,
) -> std::result::Result<(), DecodeSampleError> {
   let mut offset = 0_usize;
   while offset < data.len() {
      let parsed = atoms::read_box(data, offset).ok_or_else(invalid_subtitle)?;
      let next = offset
         .checked_add(parsed.total_size)
         .filter(|next| *next > offset && *next <= data.len())
         .ok_or_else(invalid_subtitle)?;
      visit(parsed.fourcc, parsed.payload)?;
      offset = next;
   }
   Ok(())
}

fn append_wvtt_payload(
   output: &mut TextBuilder,
   payload: &[u8],
) -> std::result::Result<(), DecodeSampleError> {
   let previous_len = output.len();
   if output.append_decoded(payload)? && previous_len != 0 {
      output.insert(previous_len, '\n')?;
   }
   Ok(())
}

struct TextBuilder {
   text: String,
   max_capacity: usize,
   #[cfg(test)]
   reservation_count: usize,
}

impl TextBuilder {
   fn new(max_capacity: usize) -> Self {
      Self {
         text: String::new(),
         max_capacity,
         #[cfg(test)]
         reservation_count: 0,
      }
   }

   fn len(&self) -> usize {
      self.text.len()
   }

   #[cfg(test)]
   fn reservation_count(&self) -> usize {
      self.reservation_count
   }

   fn finish(self) -> Option<String> {
      if self.text.is_empty() {
         None
      } else {
         Some(self.text)
      }
   }

   fn append_decoded(&mut self, data: &[u8]) -> std::result::Result<bool, DecodeSampleError> {
      if let Some(bytes) = data.strip_prefix(&[0xfe, 0xff]) {
         return self.append_utf16(bytes, Endian::Big);
      }
      if let Some(bytes) = data.strip_prefix(&[0xff, 0xfe]) {
         return self.append_utf16(bytes, Endian::Little);
      }
      if let Ok(text) = std::str::from_utf8(data) {
         let trimmed = text.trim_matches(is_trim_character);
         if trimmed.is_empty() {
            return Ok(false);
         }
         self.append_str(trimmed)?;
         return Ok(true);
      }
      if !data.len().is_multiple_of(2) {
         return Err(invalid_subtitle());
      }
      self.append_utf16(data, Endian::Big)
   }

   fn append_utf16(
      &mut self,
      data: &[u8],
      endian: Endian,
   ) -> std::result::Result<bool, DecodeSampleError> {
      if !data.len().is_multiple_of(2) {
         return Err(invalid_subtitle());
      }

      let start = self.text.len();
      let code_units = data.len() / 2;
      let conservative_capacity = code_units.checked_mul(3).ok_or_else(capacity_error)?;
      self.reserve(conservative_capacity)?;

      let units = data.chunks_exact(2).map(|bytes| match endian {
         Endian::Big => u16::from_be_bytes([bytes[0], bytes[1]]),
         Endian::Little => u16::from_le_bytes([bytes[0], bytes[1]]),
      });
      for character in decode_utf16(units) {
         self.text.push(character.map_err(|_| invalid_subtitle())?);
      }

      Ok(self.trim_from(start))
   }

   fn append_str(&mut self, value: &str) -> std::result::Result<(), DecodeSampleError> {
      self.reserve(value.len())?;
      self.text.push_str(value);
      Ok(())
   }

   fn insert(
      &mut self,
      index: usize,
      character: char,
   ) -> std::result::Result<(), DecodeSampleError> {
      self.reserve(character.len_utf8())?;
      self.text.insert(index, character);
      Ok(())
   }

   fn reserve(&mut self, additional: usize) -> std::result::Result<(), DecodeSampleError> {
      const MIN_GROWTH: usize = 8;

      let target = self
         .text
         .len()
         .checked_add(additional)
         .ok_or_else(capacity_error)?;
      if target > self.max_capacity {
         return Err(capacity_error());
      }
      if self.text.capacity() >= target {
         return Ok(());
      }

      let doubled = self
         .text
         .capacity()
         .checked_mul(2)
         .unwrap_or(self.max_capacity);
      let desired_capacity = doubled.max(MIN_GROWTH).max(target).min(self.max_capacity);
      let reservation = desired_capacity
         .checked_sub(self.text.len())
         .ok_or_else(capacity_error)?;
      #[cfg(test)]
      {
         self.reservation_count += 1;
      }
      self
         .text
         .try_reserve_exact(reservation)
         .map_err(|_| capacity_error())
   }

   fn trim_from(&mut self, start: usize) -> bool {
      let segment = &self.text[start..];
      let Some((first, _)) = segment
         .char_indices()
         .find(|(_, character)| !is_trim_character(*character))
      else {
         self.text.truncate(start);
         return false;
      };
      let last = segment
         .char_indices()
         .rev()
         .find(|(_, character)| !is_trim_character(*character))
         .map(|(index, character)| index + character.len_utf8())
         .unwrap_or(first);

      self.text.truncate(start + last);
      if first != 0 {
         drop(self.text.drain(start..start + first));
      }
      true
   }
}

#[derive(Clone, Copy)]
enum Endian {
   Big,
   Little,
}

fn is_trim_character(character: char) -> bool {
   character == '\0' || character.is_whitespace()
}

fn invalid_subtitle() -> DecodeSampleError {
   DecodeSampleError::Track(MediaParserError::SubtitleError(
      "invalid MP4 subtitle sample".to_owned(),
   ))
}

fn capacity_error() -> DecodeSampleError {
   DecodeSampleError::Fatal(MediaParserError::Other(
      "MP4 subtitle text capacity exceeded".to_owned(),
   ))
}

fn unsupported_codec(
   codec: &str,
   capacity_limit: Option<usize>,
) -> std::result::Result<Option<String>, DecodeSampleError> {
   const MAX_CODEC_IDENTIFIER_BYTES: usize = 16;

   let mut end = 0_usize;
   for (index, character) in codec.char_indices() {
      let next = index + character.len_utf8();
      if next > MAX_CODEC_IDENTIFIER_BYTES {
         break;
      }
      end = next;
   }
   if end > capacity_limit.unwrap_or(MAX_CODEC_IDENTIFIER_BYTES) {
      return Err(capacity_error());
   }

   let mut identifier = String::new();
   identifier
      .try_reserve_exact(end)
      .map_err(|_| capacity_error())?;
   identifier.push_str(&codec[..end]);
   Err(DecodeSampleError::Track(
      MediaParserError::UnsupportedCodec(identifier),
   ))
}

#[cfg(test)]
mod tests {
   use super::{
      DecodeSampleError, decode_sample, decode_sample_with_limit,
      decode_sample_with_reservation_count,
   };
   use crate::MediaParserError;

   fn mp4_box(fourcc: &[u8; 4], payload: &[u8]) -> Vec<u8> {
      let size = 8_u32 + u32::try_from(payload.len()).unwrap();
      let mut data = Vec::new();
      data.extend_from_slice(&size.to_be_bytes());
      data.extend_from_slice(fourcc);
      data.extend_from_slice(payload);
      data
   }

   fn length_prefixed_text(text: &[u8], trailing: &[u8]) -> Vec<u8> {
      let mut data = Vec::new();
      data.extend_from_slice(&u16::try_from(text.len()).unwrap().to_be_bytes());
      data.extend_from_slice(text);
      data.extend_from_slice(trailing);
      data
   }

   fn assert_subtitle_error(result: Result<Option<String>, DecodeSampleError>) {
      assert!(matches!(
         result,
         Err(DecodeSampleError::Track(MediaParserError::SubtitleError(_)))
      ));
   }

   fn assert_fatal(result: Result<Option<String>, DecodeSampleError>) {
      let Err(DecodeSampleError::Fatal(error)) = result else {
         panic!("expected a fatal subtitle decode error");
      };
      assert!(matches!(error, MediaParserError::Other(_)));
   }

   #[test]
   fn tx3g_decodes_strict_utf8() {
      assert_eq!(
         decode_sample("tx3g", &length_prefixed_text(" café ".as_bytes(), &[]),).unwrap(),
         Some("café".into())
      );
   }

   #[test]
   fn tx3g_decodes_bom_utf16_big_endian() {
      let bytes = [0xfe, 0xff, 0x00, b' ', 0x00, b'O', 0x00, b'K', 0x00, b' '];
      assert_eq!(
         decode_sample("tx3g", &length_prefixed_text(&bytes, &[])).unwrap(),
         Some("OK".into())
      );
   }

   #[test]
   fn tx3g_decodes_bom_utf16_little_endian() {
      let bytes = [0xff, 0xfe, b' ', 0x00, b'O', 0x00, b'K', 0x00, b' ', 0x00];
      assert_eq!(
         decode_sample("tx3g", &length_prefixed_text(&bytes, &[])).unwrap(),
         Some("OK".into())
      );
   }

   #[test]
   fn tx3g_treats_a_utf16_bom_without_text_as_a_gap() {
      assert_eq!(
         decode_sample("tx3g", &length_prefixed_text(&[0xfe, 0xff], &[])).unwrap(),
         None
      );
      assert_eq!(
         decode_sample("tx3g", &length_prefixed_text(&[0xff, 0xfe], &[])).unwrap(),
         None
      );
   }

   #[test]
   fn tx3g_decodes_a_utf16_supplementary_plane_character() {
      let bytes = [0xfe, 0xff, 0xd8, 0x3d, 0xde, 0x00];
      assert_eq!(
         decode_sample("tx3g", &length_prefixed_text(&bytes, &[])).unwrap(),
         Some("😀".into())
      );
   }

   #[test]
   fn tx3g_rejects_isolated_or_unpaired_utf16_surrogates() {
      assert_subtitle_error(decode_sample(
         "tx3g",
         &length_prefixed_text(&[0xfe, 0xff, 0xdc, 0x00], &[]),
      ));
      assert_subtitle_error(decode_sample(
         "tx3g",
         &length_prefixed_text(&[0xfe, 0xff, 0xd8, 0x00], &[]),
      ));
      assert_subtitle_error(decode_sample(
         "tx3g",
         &length_prefixed_text(&[0xfe, 0xff, 0xd8, 0x00, 0x00, b'A'], &[]),
      ));
   }

   #[test]
   fn tx3g_falls_back_to_bomless_utf16_big_endian() {
      let bytes = [0x00, 0x20, 0x00, 0xe9, 0x00, 0x20];
      assert_eq!(
         decode_sample("tx3g", &length_prefixed_text(&bytes, &[])).unwrap(),
         Some("é".into())
      );
   }

   #[test]
   fn tx3g_requires_a_complete_declared_text_slice() {
      assert_subtitle_error(decode_sample("tx3g", &[]));
      assert_subtitle_error(decode_sample("tx3g", &[0]));
      assert_subtitle_error(decode_sample("tx3g", &[0, 3, b'a', b'b']));
   }

   #[test]
   fn tx3g_ignores_trailing_style_records() {
      let data = length_prefixed_text(b"caption", &[0xff, 0xfe, 0xfd, 0xfc]);
      assert_eq!(
         decode_sample("tx3g", &data).unwrap(),
         Some("caption".into())
      );
   }

   #[test]
   fn tx3g_rejects_invalid_utf8_and_utf16() {
      assert_subtitle_error(decode_sample("tx3g", &length_prefixed_text(&[0xff], &[])));
      assert_subtitle_error(decode_sample(
         "tx3g",
         &length_prefixed_text(&[0xd8, 0x00], &[]),
      ));
      assert_subtitle_error(decode_sample(
         "tx3g",
         &length_prefixed_text(&[0xfe, 0xff, 0x00], &[]),
      ));
   }

   #[test]
   fn tx3g_empty_or_trimmed_empty_is_a_gap() {
      assert_eq!(decode_sample("tx3g", &[0, 0]).unwrap(), None);
      assert_eq!(
         decode_sample("tx3g", &length_prefixed_text(b" \t\r\n\0 ", &[])).unwrap(),
         None
      );
   }

   #[test]
   fn wvtt_decodes_direct_payload() {
      assert_eq!(
         decode_sample("wvtt", &mp4_box(b"payl", b" hello ")).unwrap(),
         Some("hello".into())
      );
   }

   #[test]
   fn wvtt_decodes_payload_nested_in_vttc() {
      let sample = mp4_box(b"vttc", &mp4_box(b"payl", b"nested"));
      assert_eq!(
         decode_sample("wvtt", &sample).unwrap(),
         Some("nested".into())
      );
   }

   #[test]
   fn wvtt_joins_all_non_gap_payloads_in_source_order() {
      let mut children = mp4_box(b"payl", b"second");
      children.extend(mp4_box(b"iden", b"ignored"));
      children.extend(mp4_box(b"payl", b" third "));

      let mut sample = mp4_box(b"payl", b" first ");
      sample.extend(mp4_box(b"junk", b"ignored"));
      sample.extend(mp4_box(b"vttc", &children));
      sample.extend(mp4_box(b"payl", b"fourth"));

      assert_eq!(
         decode_sample("wvtt", &sample).unwrap(),
         Some("first\nsecond\nthird\nfourth".into())
      );
   }

   #[test]
   fn wvtt_ignores_unknown_well_formed_boxes() {
      let nested_unknown = mp4_box(b"vttc", &mp4_box(b"sttg", b"line:0"));
      let mut sample = mp4_box(b"free", b"anything");
      sample.extend(nested_unknown);
      assert_eq!(decode_sample("wvtt", &sample).unwrap(), None);
   }

   #[test]
   fn wvtt_rejects_malformed_top_level_box_framing() {
      let malformed = [
         vec![0, 0, 0, 12, b'p', b'a', b'y', b'l'],
         vec![0, 0, 0, 0, b'p', b'a', b'y', b'l'],
         vec![0, 0, 0, 4, b'p', b'a', b'y', b'l'],
         vec![
            0, 0, 0, 1, b'p', b'a', b'y', b'l', 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff,
         ],
      ];
      for data in malformed {
         assert_subtitle_error(decode_sample("wvtt", &data));
      }

      let mut trailing = mp4_box(b"payl", b"ok");
      trailing.extend_from_slice(&[1, 2, 3]);
      assert_subtitle_error(decode_sample("wvtt", &trailing));
   }

   #[test]
   fn wvtt_rejects_malformed_nested_box_framing() {
      let sample = mp4_box(b"vttc", &[0, 0, 0, 0, b'p', b'a', b'y', b'l']);
      assert_subtitle_error(decode_sample("wvtt", &sample));

      let sample = mp4_box(b"vttc", &[1, 2, 3]);
      assert_subtitle_error(decode_sample("wvtt", &sample));
   }

   #[test]
   fn wvtt_without_nonempty_payload_is_a_gap() {
      assert_eq!(decode_sample("wvtt", &[]).unwrap(), None);
      assert_eq!(decode_sample("wvtt", &mp4_box(b"vttc", &[])).unwrap(), None);
      assert_eq!(
         decode_sample("wvtt", &mp4_box(b"payl", b" \0\t ")).unwrap(),
         None
      );
   }

   #[test]
   fn stpp_and_text_use_strict_deterministic_decoding_and_trimming() {
      assert_eq!(
         decode_sample("stpp", b" <p>cue</p>\0 ").unwrap(),
         Some("<p>cue</p>".into())
      );
      assert_eq!(
         decode_sample(
            "text",
            &length_prefixed_text(&[0xff, 0xfe, b'O', 0, b'K', 0], &[]),
         )
         .unwrap(),
         Some("OK".into())
      );
      assert_eq!(
         decode_sample("text", &length_prefixed_text(b" \n\0\t", &[])).unwrap(),
         None
      );
      assert_subtitle_error(decode_sample("stpp", &[0xff]));
   }

   #[test]
   fn text_trims_multibyte_unicode_whitespace_at_both_boundaries() {
      assert_eq!(
         decode_sample(
            "text",
            &length_prefixed_text("\u{2003}é\u{3000}".as_bytes(), &[]),
         )
         .unwrap(),
         Some("é".into())
      );
   }

   #[test]
   fn text_ignores_a_trailing_style_atom_with_non_utf8_bytes() {
      let style = mp4_box(b"styl", &[0x80]);
      let sample = length_prefixed_text(b"Fourteen bytes", &style);

      assert_eq!(
         decode_sample("text", &sample).unwrap(),
         Some("Fourteen bytes".into())
      );
   }

   #[test]
   fn text_requires_a_complete_declared_text_slice() {
      assert_subtitle_error(decode_sample("text", &[0, 3, b'a', b'b']));
   }

   #[test]
   fn unsupported_codec_is_a_track_local_error() {
      let Err(DecodeSampleError::Track(MediaParserError::UnsupportedCodec(codec))) =
         decode_sample("c608", b"caption")
      else {
         panic!("expected a track-local unsupported codec error");
      };
      assert_eq!(codec, "c608");
   }

   #[test]
   fn unsupported_codec_capacity_refusal_is_fatal() {
      assert_fatal(decode_sample_with_limit("c608", b"caption", 3));
   }

   #[test]
   fn capacity_refusal_is_fatal_for_utf8_utf16_and_wvtt_joining() {
      assert_fatal(decode_sample_with_limit(
         "text",
         &length_prefixed_text(b"valid", &[]),
         4,
      ));
      assert_fatal(decode_sample_with_limit(
         "text",
         &length_prefixed_text(&[0xfe, 0xff, 0x4f, 0x60], &[]),
         2,
      ));

      let mut sample = mp4_box(b"payl", b"ab");
      sample.extend(mp4_box(b"payl", b"cd"));
      assert_fatal(decode_sample_with_limit("wvtt", &sample, 4));
   }

   #[test]
   fn wvtt_many_tiny_payloads_use_logarithmically_bounded_growth() {
      const PAYLOAD_COUNT: usize = 512;
      let mut sample = Vec::new();
      for _ in 0..PAYLOAD_COUNT {
         sample.extend(mp4_box(b"payl", b"x"));
      }

      let (output, reservation_count) =
         decode_sample_with_reservation_count("wvtt", &sample).unwrap();
      let output = output.unwrap();

      assert_eq!(output.len(), PAYLOAD_COUNT * 2 - 1);
      assert!(
         reservation_count <= 16,
         "{} fallible reservation calls for {PAYLOAD_COUNT} payloads",
         reservation_count
      );
   }
}

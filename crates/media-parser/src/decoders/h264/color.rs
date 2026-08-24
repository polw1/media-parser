use super::{AvcColorMetadata, AvcConfig};
use std::collections::HashMap;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum MatrixCoefficients {
   Bt601,
   Bt709,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct ParsedSps {
   sps_id: u32,
   color: AvcColorMetadata,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct GopColor {
   pub(super) matrix: MatrixCoefficients,
   pub(super) full_range: bool,
}

impl GopColor {
   /// The policy applied whenever the stream carries no usable colour
   /// metadata. BT.601 limited is what OpenH264 0.9.7 has always produced, so
   /// unannotated streams keep their existing output byte for byte.
   pub(super) const DEFAULT: Self = Self {
      matrix: MatrixCoefficients::Bt601,
      full_range: false,
   };
}

struct BitReader<'a> {
   bytes: &'a [u8],
   offset: usize,
   current: u8,
   bits_remaining: u8,
   zero_count: usize,
}

impl<'a> BitReader<'a> {
   fn new(bytes: &'a [u8]) -> Self {
      Self {
         bytes,
         offset: 0,
         current: 0,
         bits_remaining: 0,
         zero_count: 0,
      }
   }

   fn next_rbsp_byte(&mut self) -> Result<u8, String> {
      loop {
         let byte = *self
            .bytes
            .get(self.offset)
            .ok_or_else(|| "truncated H.264 RBSP".to_string())?;
         self.offset += 1;
         if self.zero_count >= 2 && byte == 3 {
            let next = self
               .bytes
               .get(self.offset)
               .ok_or_else(|| "truncated H.264 emulation prevention sequence".to_string())?;
            if *next > 3 {
               return Err("invalid H.264 emulation prevention sequence".to_string());
            }
            self.zero_count = 0;
            continue;
         }
         self.zero_count = if byte == 0 { self.zero_count + 1 } else { 0 };
         return Ok(byte);
      }
   }

   fn read_bit(&mut self) -> Result<bool, String> {
      if self.bits_remaining == 0 {
         self.current = self.next_rbsp_byte()?;
         self.bits_remaining = 8;
      }
      self.bits_remaining -= 1;
      let value = self.current & (1 << self.bits_remaining) != 0;
      Ok(value)
   }

   fn read_bits(&mut self, count: usize) -> Result<u32, String> {
      if count > 32 {
         return Err("invalid H.264 bit field".to_string());
      }
      let mut value = 0u32;
      for _ in 0..count {
         value = (value << 1) | u32::from(self.read_bit()?);
      }
      Ok(value)
   }

   fn read_ue(&mut self) -> Result<u32, String> {
      let mut leading_zero_bits = 0usize;
      while !self.read_bit()? {
         leading_zero_bits += 1;
         if leading_zero_bits >= 32 {
            return Err("H.264 Exp-Golomb value is too large".to_string());
         }
      }
      let suffix = self.read_bits(leading_zero_bits)?;
      Ok(((1u32 << leading_zero_bits) - 1) + suffix)
   }

   fn read_se(&mut self) -> Result<i64, String> {
      let code_num = self.read_ue()?;
      if code_num & 1 == 0 {
         Ok(-i64::from(code_num / 2))
      } else {
         Ok(i64::from(code_num / 2) + 1)
      }
   }
}

fn skip_scaling_list(bits: &mut BitReader<'_>, size: usize) -> Result<(), String> {
   let mut last_scale = 8i64;
   let mut next_scale = 8i64;
   for _ in 0..size {
      if next_scale != 0 {
         next_scale = (last_scale + bits.read_se()? + 256).rem_euclid(256);
      }
      if next_scale != 0 {
         last_scale = next_scale;
      }
   }
   Ok(())
}

fn bit_reader_from_nal(nal: &[u8], expected_type: u8) -> Result<BitReader<'_>, String> {
   let (&header, ebsp) = nal
      .split_first()
      .ok_or_else(|| "empty H.264 NAL unit".to_string())?;
   if header & 0x1f != expected_type {
      return Err("unexpected H.264 NAL unit type".to_string());
   }
   Ok(BitReader::new(ebsp))
}

fn parse_sps_color(nal: &[u8]) -> Result<ParsedSps, String> {
   let mut bits = bit_reader_from_nal(nal, 7)?;
   let profile_idc = bits.read_bits(8)?;
   bits.read_bits(8)?; // constraint flags and reserved bits
   bits.read_bits(8)?; // level_idc
   let sps_id = bits.read_ue()?;
   if sps_id >= 32 {
      return Err(format!("H.264 SPS id is out of range: {sps_id}"));
   }
   if matches!(
      profile_idc,
      100 | 110 | 122 | 244 | 44 | 83 | 86 | 118 | 128 | 138 | 139 | 134 | 135
   ) {
      let chroma_format_idc = bits.read_ue()?;
      if chroma_format_idc > 3 {
         return Err("invalid H.264 chroma format".to_string());
      }
      if chroma_format_idc == 3 {
         bits.read_bit()?; // separate_colour_plane_flag
      }
      bits.read_ue()?; // bit_depth_luma_minus8
      bits.read_ue()?; // bit_depth_chroma_minus8
      bits.read_bit()?; // qpprime_y_zero_transform_bypass_flag
      if bits.read_bit()? {
         let scaling_list_count = if chroma_format_idc == 3 { 12 } else { 8 };
         for list in 0..scaling_list_count {
            if bits.read_bit()? {
               skip_scaling_list(&mut bits, if list < 6 { 16 } else { 64 })?;
            }
         }
      }
   } else if !matches!(profile_idc, 66 | 77 | 88) {
      return Err("unsupported H.264 SPS profile for color parsing".to_string());
   }
   bits.read_ue()?; // log2_max_frame_num_minus4
   let pic_order_cnt_type = bits.read_ue()?;
   match pic_order_cnt_type {
      0 => {
         bits.read_ue()?; // log2_max_pic_order_cnt_lsb_minus4
      }
      1 => {
         bits.read_bit()?; // delta_pic_order_always_zero_flag
         bits.read_ue()?; // offset_for_non_ref_pic (signed Exp-Golomb)
         bits.read_ue()?; // offset_for_top_to_bottom_field (signed Exp-Golomb)
         let cycle_len = bits.read_ue()?;
         if cycle_len > 255 {
            return Err("H.264 picture order count cycle is too large".to_string());
         }
         for _ in 0..cycle_len {
            bits.read_ue()?; // offset_for_ref_frame (signed Exp-Golomb)
         }
      }
      2 => {}
      _ => return Err("unsupported H.264 picture order count type".to_string()),
   }
   bits.read_ue()?; // max_num_ref_frames
   bits.read_bit()?; // gaps_in_frame_num_value_allowed_flag
   bits.read_ue()?; // pic_width_in_mbs_minus1
   bits.read_ue()?; // pic_height_in_map_units_minus1
   let frame_mbs_only = bits.read_bit()?;
   if !frame_mbs_only {
      bits.read_bit()?; // mb_adaptive_frame_field_flag
   }
   bits.read_bit()?; // direct_8x8_inference_flag
   if bits.read_bit()? {
      for _ in 0..4 {
         bits.read_ue()?;
      }
   }
   if !bits.read_bit()? {
      return Ok(ParsedSps {
         sps_id,
         color: AvcColorMetadata::default(),
      });
   }
   if bits.read_bit()? {
      let aspect_ratio_idc = bits.read_bits(8)?;
      if aspect_ratio_idc == 255 {
         bits.read_bits(16)?;
         bits.read_bits(16)?;
      }
   }
   if bits.read_bit()? {
      bits.read_bit()?;
   }
   let mut color = AvcColorMetadata::default();
   if bits.read_bit()? {
      bits.read_bits(3)?;
      color.full_range = Some(bits.read_bit()?);
      if bits.read_bit()? {
         bits.read_bits(8)?;
         bits.read_bits(8)?;
         color.matrix_coefficients = match bits.read_bits(8)? as u16 {
            2 => None,
            value => Some(value),
         };
      }
   }
   Ok(ParsedSps { sps_id, color })
}

fn parse_pps_ids(nal: &[u8]) -> Result<(u32, u32), String> {
   let mut bits = bit_reader_from_nal(nal, 8)?;
   let pps_id = bits.read_ue()?;
   if pps_id >= 256 {
      return Err(format!("H.264 PPS id is out of range: {pps_id}"));
   }
   Ok((pps_id, bits.read_ue()?))
}

fn parse_vcl_pps_id(nal: &[u8]) -> Result<u32, String> {
   let nal_type = nal
      .first()
      .map(|header| header & 0x1f)
      .ok_or_else(|| "empty H.264 NAL unit".to_string())?;
   if !matches!(nal_type, 1 | 2 | 5) {
      return Err("unexpected H.264 VCL NAL unit type".to_string());
   }
   let mut bits = bit_reader_from_nal(nal, nal_type)?;
   bits.read_ue()?; // first_mb_in_slice
   bits.read_ue()?; // slice_type
   bits.read_ue()
}

pub(super) fn visit_avc_nals(
   sample: &[u8],
   length_size: usize,
   mut visit: impl FnMut(&[u8]) -> Result<(), String>,
) -> Result<(), String> {
   if !(1..=4).contains(&length_size) {
      return Err(format!("invalid H.264 NAL length size: {length_size}"));
   }
   let mut offset = 0usize;
   let mut count = 0usize;
   while offset < sample.len() {
      let length_end = offset
         .checked_add(length_size)
         .filter(|end| *end <= sample.len())
         .ok_or_else(|| "truncated H.264 NAL length".to_string())?;
      let mut nal_len = 0usize;
      for &byte in &sample[offset..length_end] {
         nal_len = nal_len
            .checked_mul(256)
            .and_then(|length| length.checked_add(usize::from(byte)))
            .ok_or_else(|| "H.264 NAL length overflow".to_string())?;
      }
      offset = length_end;
      if nal_len == 0 {
         return Err("empty H.264 NAL unit".to_string());
      }
      let nal_end = offset
         .checked_add(nal_len)
         .filter(|end| *end <= sample.len())
         .ok_or_else(|| "truncated H.264 NAL unit".to_string())?;
      visit(&sample[offset..nal_end])?;
      offset = nal_end;
      count += 1;
   }
   if count == 0 {
      return Err("H.264 sample contains no NAL units".to_string());
   }
   Ok(())
}

/// Maps an ISO/IEC 23091-2 matrix identifier onto the two matrices this module
/// implements, resolving the rest to their closest implemented neighbour: a
/// thumbnail with approximate colour beats no thumbnail at all.
///
/// `None` means "no usable hint", leaving the caller on [`GopColor::DEFAULT`].
fn matrix_coefficients(value: u16) -> Option<MatrixCoefficients> {
   match value {
      // BT.709 exactly, then SMPTE 240M (Kr 0.212 / Kb 0.087) and both BT.2020
      // variants (Kr 0.2627 / Kb 0.0593), which sit far closer to BT.709's
      // luma weights than to BT.601's. BT.2020 still gets no tone mapping, so
      // an HDR source stays an approximation rather than a colour-managed one.
      1 | 7 | 9 | 10 => Some(MatrixCoefficients::Bt709),
      // BT.470BG and SMPTE 170M are BT.601; FCC (Kr 0.30 / Kb 0.11) rounds to it.
      4..=6 => Some(MatrixCoefficients::Bt601),
      // 0 (identity/GBR), 2 (unspecified), 8 (YCgCo) and anything newer have no
      // BT.601/BT.709 approximation worth applying.
      _ => None,
   }
}

/// Overlays the active SPS onto the container hints, field by field. A source
/// that declares a matrix this module cannot approximate is treated as silent
/// for that field, so a usable container hint still gets its chance.
fn resolved_color(sps: AvcColorMetadata, container: AvcColorMetadata) -> GopColor {
   GopColor {
      matrix: sps
         .matrix_coefficients
         .and_then(matrix_coefficients)
         .or_else(|| container.matrix_coefficients.and_then(matrix_coefficients))
         .unwrap_or(GopColor::DEFAULT.matrix),
      full_range: sps
         .full_range
         .or(container.full_range)
         .unwrap_or(GopColor::DEFAULT.full_range),
   }
}

/// Resolves the colour policy for one decoded segment from the first slice that
/// names a parameter set pair we could read.
///
/// This is deliberately infallible. Colour is a presentation hint, and the
/// segment is decoded either way, so unreadable, unsupported or internally
/// inconsistent metadata degrades to [`GopColor::DEFAULT`] instead of failing
/// the whole thumbnail request. Mid-segment colour changes keep the first
/// slice's policy: one decoded GOP produces one set of coefficients.
pub(super) fn resolve_gop_color<S: AsRef<[u8]>>(config: &AvcConfig, samples: &[S]) -> GopColor {
   let mut sps_by_id = HashMap::new();
   let mut pps_to_sps = HashMap::new();
   for nal in &config.sps {
      if let Ok(parsed) = parse_sps_color(nal) {
         sps_by_id.insert(parsed.sps_id, parsed.color);
      }
   }
   for nal in &config.pps {
      if let Ok((pps_id, sps_id)) = parse_pps_ids(nal) {
         pps_to_sps.insert(pps_id, sps_id);
      }
   }

   let mut gop_color = None;
   for sample in samples {
      // A malformed NAL length simply ends the search for this sample; the
      // decoder reports its own error for the same data.
      let _ = visit_avc_nals(sample.as_ref(), config.length_size, |nal| {
         match nal[0] & 0x1f {
            7 => {
               if let Ok(parsed) = parse_sps_color(nal) {
                  sps_by_id.insert(parsed.sps_id, parsed.color);
               }
            }
            8 => {
               if let Ok((pps_id, sps_id)) = parse_pps_ids(nal) {
                  pps_to_sps.insert(pps_id, sps_id);
               }
            }
            1 | 2 | 5 if gop_color.is_none() => {
               gop_color = parse_vcl_pps_id(nal)
                  .ok()
                  .and_then(|pps_id| pps_to_sps.get(&pps_id))
                  .and_then(|sps_id| sps_by_id.get(sps_id))
                  .map(|sps| resolved_color(*sps, config.color));
            }
            _ => {}
         }
         Ok(())
      });
      if gop_color.is_some() {
         break;
      }
   }
   gop_color.unwrap_or(GopColor::DEFAULT)
}

#[cfg(test)]
mod tests {
   use super::*;
   use crate::decoders::h264::{AvcColorMetadata, AvcConfig};

   struct BitWriter {
      bytes: Vec<u8>,
      bit_len: usize,
   }

   impl BitWriter {
      fn new() -> Self {
         Self {
            bytes: Vec::new(),
            bit_len: 0,
         }
      }

      fn bit(&mut self, value: bool) {
         if self.bit_len.is_multiple_of(8) {
            self.bytes.push(0);
         }
         if value {
            let shift = 7 - self.bit_len % 8;
            let last = self.bytes.len() - 1;
            self.bytes[last] |= 1 << shift;
         }
         self.bit_len += 1;
      }

      fn bits(&mut self, value: u32, count: usize) {
         for shift in (0..count).rev() {
            self.bit(value & (1 << shift) != 0);
         }
      }

      fn ue(&mut self, value: u32) {
         let code_num = value + 1;
         let width = (u32::BITS - code_num.leading_zeros()) as usize;
         for _ in 1..width {
            self.bit(false);
         }
         self.bits(code_num, width);
      }

      fn se(&mut self, value: i32) {
         let code_num = if value <= 0 {
            value.unsigned_abs() * 2
         } else {
            value as u32 * 2 - 1
         };
         self.ue(code_num);
      }

      fn finish(mut self) -> Vec<u8> {
         self.bit(true);
         while !self.bit_len.is_multiple_of(8) {
            self.bit(false);
         }
         self.bytes
      }
   }

   fn sps(
      profile_idc: u8,
      sps_id: u32,
      matrix: Option<u8>,
      full_range: bool,
      pic_order_cnt_type: u32,
      scaling_lists: u8,
   ) -> Vec<u8> {
      let mut bits = BitWriter::new();
      bits.bits(u32::from(profile_idc), 8);
      bits.bits(0, 8); // constraint flags and reserved bits
      bits.bits(30, 8); // level_idc
      bits.ue(sps_id);
      if profile_idc == 100 {
         bits.ue(1); // chroma_format_idc: 4:2:0
         bits.ue(0); // bit_depth_luma_minus8
         bits.ue(0); // bit_depth_chroma_minus8
         bits.bit(false); // qpprime_y_zero_transform_bypass_flag
         bits.bit(scaling_lists != 0);
         if scaling_lists != 0 {
            for list in 0..8 {
               let present = scaling_lists == 2 && list == 0;
               bits.bit(present);
               if present {
                  for _ in 0..16 {
                     bits.se(0); // delta_scale
                  }
               }
            }
         }
      }
      bits.ue(0); // log2_max_frame_num_minus4
      bits.ue(pic_order_cnt_type);
      if pic_order_cnt_type == 0 {
         bits.ue(0); // log2_max_pic_order_cnt_lsb_minus4
      } else if pic_order_cnt_type == 1 {
         bits.bit(false); // delta_pic_order_always_zero_flag
         bits.se(0); // offset_for_non_ref_pic
         bits.se(0); // offset_for_top_to_bottom_field
         bits.ue(1); // num_ref_frames_in_pic_order_cnt_cycle
         bits.se(0); // offset_for_ref_frame[0]
      }
      bits.ue(1); // max_num_ref_frames
      bits.bit(false); // gaps_in_frame_num_value_allowed_flag
      bits.ue(1); // pic_width_in_mbs_minus1
      bits.ue(1); // pic_height_in_map_units_minus1
      bits.bit(true); // frame_mbs_only_flag
      bits.bit(true); // direct_8x8_inference_flag
      bits.bit(false); // frame_cropping_flag
      bits.bit(true); // vui_parameters_present_flag
      bits.bit(false); // aspect_ratio_info_present_flag
      bits.bit(false); // overscan_info_present_flag
      bits.bit(true); // video_signal_type_present_flag
      bits.bits(5, 3); // video_format
      bits.bit(full_range);
      bits.bit(matrix.is_some());
      if let Some(matrix) = matrix {
         bits.bits(1, 8); // colour_primaries
         bits.bits(1, 8); // transfer_characteristics
         bits.bits(u32::from(matrix), 8);
      }
      let mut nal = vec![0x67];
      nal.extend(bits.finish());
      nal
   }

   fn baseline_sps(sps_id: u32, matrix: u8, full_range: bool) -> Vec<u8> {
      sps(66, sps_id, Some(matrix), full_range, 0, 0)
   }

   fn pps(pps_id: u32, sps_id: u32) -> Vec<u8> {
      let mut bits = BitWriter::new();
      bits.ue(pps_id);
      bits.ue(sps_id);
      let mut nal = vec![0x68];
      nal.extend(bits.finish());
      nal
   }

   fn idr_slice(pps_id: u32) -> Vec<u8> {
      let mut bits = BitWriter::new();
      bits.ue(0); // first_mb_in_slice
      bits.ue(2); // I slice
      bits.ue(pps_id);
      let mut nal = vec![0x65];
      nal.extend(bits.finish());
      nal
   }

   fn avc_sample(nals: &[Vec<u8>]) -> Vec<u8> {
      let mut sample = Vec::new();
      for nal in nals {
         sample.extend_from_slice(
            &u32::try_from(nal.len())
               .expect("test NAL fits u32")
               .to_be_bytes(),
         );
         sample.extend_from_slice(nal);
      }
      sample
   }

   #[test]
   fn parses_matrix_and_range_from_sps_vui() {
      let parsed = parse_sps_color(&baseline_sps(3, 1, true)).expect("valid SPS parses");

      assert_eq!(parsed.sps_id, 3);
      assert_eq!(parsed.color.matrix_coefficients, Some(1));
      assert_eq!(parsed.color.full_range, Some(true));
   }

   #[test]
   fn rejects_sps_ids_outside_the_h264_limit() {
      let error = parse_sps_color(&baseline_sps(32, 1, false))
         .expect_err("H.264 allows at most 32 SPS identifiers");

      assert!(error.contains("SPS id"));
   }

   #[test]
   fn rejects_pps_ids_outside_the_h264_limit() {
      let error =
         parse_pps_ids(&pps(256, 0)).expect_err("H.264 allows at most 256 PPS identifiers");

      assert!(error.contains("PPS id"));
   }

   #[test]
   fn removes_consecutive_emulation_prevention_sequences() {
      let mut bits = bit_reader_from_nal(&[0x67, 0, 0, 3, 0, 0, 3, 1], 7)
         .expect("valid SPS creates a bit reader");
      let rbsp = (0..5)
         .map(|_| bits.read_bits(8).expect("RBSP byte remains"))
         .collect::<Vec<_>>();

      assert_eq!(rbsp, [0, 0, 0, 0, 1]);
   }

   #[test]
   fn rejects_malformed_emulation_prevention_sequences() {
      for nal in [&[0x67, 0, 0, 3][..], &[0x67, 0, 0, 3, 4][..]] {
         let mut bits = bit_reader_from_nal(nal, 7).expect("SPS header is present");
         assert_eq!(bits.read_bits(16), Ok(0));

         let error = bits
            .read_bit()
            .expect_err("invalid emulation prevention must be rejected");

         assert!(error.contains("emulation prevention"));
      }
   }

   #[test]
   fn skips_present_scaling_lists_before_vui() {
      let parsed = parse_sps_color(&sps(100, 8, Some(1), false, 0, 2))
         .expect("custom scaling list is skipped");

      assert_eq!(parsed.sps_id, 8);
      assert_eq!(parsed.color.matrix_coefficients, Some(1));
   }

   #[test]
   fn parses_high_profile_sps_used_by_common_avc3_streams() {
      let parsed =
         parse_sps_color(&sps(100, 7, Some(6), false, 0, 0)).expect("valid high SPS parses");

      assert_eq!(parsed.sps_id, 7);
      assert_eq!(parsed.color.matrix_coefficients, Some(6));
      assert_eq!(parsed.color.full_range, Some(false));
   }

   #[test]
   fn skips_high_profile_scaling_matrix_flags_before_vui() {
      let parsed = parse_sps_color(&sps(100, 4, Some(1), true, 0, 1))
         .expect("high SPS with default scaling lists parses");

      assert_eq!(parsed.sps_id, 4);
      assert_eq!(parsed.color.matrix_coefficients, Some(1));
      assert_eq!(parsed.color.full_range, Some(true));
   }

   #[test]
   fn parses_sps_with_pic_order_count_type_two() {
      let parsed = parse_sps_color(&sps(66, 5, Some(1), false, 2, 0))
         .expect("POC type 2 does not add SPS fields");

      assert_eq!(parsed.sps_id, 5);
      assert_eq!(parsed.color.matrix_coefficients, Some(1));
   }

   #[test]
   fn skips_pic_order_count_type_one_fields_before_vui() {
      let parsed =
         parse_sps_color(&sps(66, 6, Some(6), false, 1, 0)).expect("POC type 1 fields are skipped");

      assert_eq!(parsed.sps_id, 6);
      assert_eq!(parsed.color.matrix_coefficients, Some(6));
   }

   #[test]
   fn in_band_active_sps_overrides_avcc_and_colr_for_avc3() {
      let config = AvcConfig {
         length_size: 4,
         sps: vec![baseline_sps(0, 6, false)],
         pps: vec![pps(0, 0)],
         color: AvcColorMetadata {
            matrix_coefficients: Some(6),
            full_range: Some(false),
         },
      };
      let sample = avc_sample(&[baseline_sps(1, 1, true), pps(3, 1), idr_slice(3)]);

      let color = resolve_gop_color(&config, &[sample]);

      assert_eq!(color.matrix, MatrixCoefficients::Bt709);
      assert!(color.full_range);
   }

   #[test]
   fn resolves_matrix_and_range_independently_across_metadata_sources() {
      let config = AvcConfig {
         length_size: 4,
         sps: Vec::new(),
         pps: Vec::new(),
         color: AvcColorMetadata {
            matrix_coefficients: Some(1),
            full_range: Some(false),
         },
      };
      let sample = avc_sample(&[sps(66, 2, None, true, 0, 0), pps(2, 2), idr_slice(2)]);

      let color = resolve_gop_color(&config, &[sample]);

      assert_eq!(color.matrix, MatrixCoefficients::Bt709);
      assert!(color.full_range);
   }

   #[test]
   fn keeps_the_first_slices_policy_when_color_changes_mid_segment() {
      let config = AvcConfig {
         length_size: 4,
         sps: Vec::new(),
         pps: Vec::new(),
         color: AvcColorMetadata::default(),
      };
      let first = avc_sample(&[baseline_sps(0, 6, false), pps(0, 0), idr_slice(0)]);
      let second = avc_sample(&[baseline_sps(1, 1, false), pps(1, 1), idr_slice(1)]);

      let color = resolve_gop_color(&config, &[first, second]);

      assert_eq!(color.matrix, MatrixCoefficients::Bt601);
   }

   #[test]
   fn ignores_unsupported_matrix_in_an_unused_sps() {
      let config = AvcConfig {
         length_size: 4,
         sps: vec![baseline_sps(9, 9, false), baseline_sps(1, 1, false)],
         pps: vec![pps(1, 1)],
         color: AvcColorMetadata::default(),
      };
      let sample = avc_sample(&[idr_slice(1)]);

      let color = resolve_gop_color(&config, &[sample]);

      assert_eq!(color.matrix, MatrixCoefficients::Bt709);
   }

   #[test]
   fn approximates_bt2020_with_bt709_instead_of_failing() {
      let config = AvcConfig {
         length_size: 4,
         sps: vec![baseline_sps(9, 9, false)],
         pps: vec![pps(9, 9)],
         color: AvcColorMetadata::default(),
      };
      let sample = avc_sample(&[idr_slice(9)]);

      let color = resolve_gop_color(&config, &[sample]);

      assert_eq!(color.matrix, MatrixCoefficients::Bt709);
      assert!(!color.full_range);
   }

   #[test]
   fn falls_back_to_the_container_matrix_when_the_sps_one_is_unusable() {
      // YCgCo has no BT.601/BT.709 approximation, so the `colr` hint wins.
      let config = AvcConfig {
         length_size: 4,
         sps: vec![baseline_sps(0, 8, false)],
         pps: vec![pps(0, 0)],
         color: AvcColorMetadata {
            matrix_coefficients: Some(1),
            full_range: None,
         },
      };
      let sample = avc_sample(&[idr_slice(0)]);

      let color = resolve_gop_color(&config, &[sample]);

      assert_eq!(color.matrix, MatrixCoefficients::Bt709);
   }

   #[test]
   fn falls_back_to_the_default_policy_for_unreadable_metadata() {
      let config = AvcConfig {
         length_size: 4,
         sps: vec![vec![0x67, 0xff]],
         pps: vec![vec![0x68, 0xff]],
         color: AvcColorMetadata::default(),
      };
      // An unsupported profile, a slice naming an unknown PPS, and a sample
      // with no slice at all must all decode with the default policy.
      let unreadable = avc_sample(&[sps(144, 0, Some(1), true, 0, 0), idr_slice(31)]);

      for samples in [vec![unreadable], vec![avc_sample(&[pps(0, 0)])], Vec::new()] {
         assert_eq!(resolve_gop_color(&config, &samples), GopColor::DEFAULT);
      }
   }

   #[test]
   fn resolves_color_from_a_non_vec_sample_view() {
      struct SampleView<'a>(&'a [u8]);

      impl AsRef<[u8]> for SampleView<'_> {
         fn as_ref(&self) -> &[u8] {
            self.0
         }
      }

      let config = AvcConfig {
         length_size: 4,
         sps: vec![baseline_sps(0, 8, false)],
         pps: vec![pps(0, 0)],
         color: AvcColorMetadata::default(),
      };
      let sample = avc_sample(&[idr_slice(0)]);
      let expected = resolve_gop_color(&config, std::slice::from_ref(&sample));

      let color = resolve_gop_color(&config, &[SampleView(&sample)]);

      assert_eq!(color, expected);
   }
}

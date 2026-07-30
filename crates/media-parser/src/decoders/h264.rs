//! H.264/AVC decoding helpers backed by OpenH264.

use openh264::decoder::{DecodeOptions, DecodedYUV, Decoder, Flush};
use openh264::formats::YUVSource;

const JPEG_QUALITY: u8 = 60;
const MAX_DECODED_IMAGE_BYTES: usize = 64 * 1024 * 1024;

/// AVC decoder configuration extracted from an MP4 `avcC` box.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AvcConfig {
   pub length_size: usize,
   pub sps: Vec<Vec<u8>>,
   pub pps: Vec<Vec<u8>>,
}

/// Decoded JPEG thumbnail.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DecodedImage {
   pub width: u32,
   pub height: u32,
   pub data: Vec<u8>,
}

/// Decodes one GOP and encodes the selected presentation-order pictures.
pub fn decode_frames_to_jpeg(
   config: &AvcConfig,
   samples: &[Vec<u8>],
   output_indices: &[usize],
) -> Result<Vec<DecodedImage>, String> {
   if samples.is_empty() {
      return Err("no H.264 samples to decode".to_string());
   }
   if output_indices
      .iter()
      .any(|output_index| *output_index >= samples.len())
   {
      return Err("requested H.264 output is outside the sample range".to_string());
   }

   let mut decoder = Decoder::new().map_err(|error| error.to_string())?;
   let no_flush = DecodeOptions::new().flush_after_decode(Flush::NoFlush);
   let headers = parameter_sets_annex_b(config)?;
   if decoder
      .decode_with_options(&headers, no_flush.clone())
      .map_err(|error| error.to_string())?
      .is_some()
   {
      return Err("OpenH264 emitted a frame for AVC parameter sets".to_string());
   }
   let mut decoded_count = 0usize;
   let mut selected = vec![None; output_indices.len()];

   for sample in samples {
      let annex_b = sample_to_annex_b(sample, config.length_size)?;
      if let Some(yuv) = decoder
         .decode_with_options(&annex_b, no_flush.clone())
         .map_err(|error| error.to_string())?
      {
         store_selected_frame(&yuv, decoded_count, output_indices, &mut selected)?;
         decoded_count = decoded_count
            .checked_add(1)
            .ok_or_else(|| "decoded frame count overflow".to_string())?;
      }
   }

   for yuv in decoder
      .flush_remaining()
      .map_err(|error| error.to_string())?
   {
      store_selected_frame(&yuv, decoded_count, output_indices, &mut selected)?;
      decoded_count = decoded_count
         .checked_add(1)
         .ok_or_else(|| "decoded frame count overflow".to_string())?;
   }

   if decoded_count != samples.len() {
      return Err(format!(
         "OpenH264 produced {decoded_count} frames for {} samples",
         samples.len()
      ));
   }
   selected
      .into_iter()
      .enumerate()
      .map(|(position, image)| {
         image.ok_or_else(|| {
            format!(
               "OpenH264 produced {decoded_count} frames, requested output {}",
               output_indices[position]
            )
         })
      })
      .collect()
}

fn store_selected_frame(
   yuv: &DecodedYUV<'_>,
   decoded_index: usize,
   output_indices: &[usize],
   selected: &mut [Option<DecodedImage>],
) -> Result<(), String> {
   let mut positions = output_indices
      .iter()
      .enumerate()
      .filter_map(|(position, output_index)| (*output_index == decoded_index).then_some(position));
   let Some(first_position) = positions.next() else {
      return Ok(());
   };
   let image = yuv_to_jpeg(yuv)?;
   selected[first_position] = Some(image.clone());
   for position in positions {
      selected[position] = Some(image.clone());
   }
   Ok(())
}

fn yuv_to_jpeg(yuv: &DecodedYUV<'_>) -> Result<DecodedImage, String> {
   let (width, height, rgb) = yuv_to_rgb(yuv)?;
   let width_u16 =
      u16::try_from(width).map_err(|_| "frame width exceeds JPEG limits".to_string())?;
   let height_u16 =
      u16::try_from(height).map_err(|_| "frame height exceeds JPEG limits".to_string())?;
   let mut data = Vec::new();
   data
      .try_reserve(rgb.len())
      .map_err(|_| "JPEG output is too large".to_string())?;
   jpeg_encoder::Encoder::new(&mut data, JPEG_QUALITY)
      .encode(&rgb, width_u16, height_u16, jpeg_encoder::ColorType::Rgb)
      .map_err(|error| error.to_string())?;

   Ok(DecodedImage {
      width,
      height,
      data,
   })
}

fn yuv_to_rgb(yuv: &DecodedYUV<'_>) -> Result<(u32, u32, Vec<u8>), String> {
   let (width, height) = yuv.dimensions();
   if yuv.rgb8_len() > MAX_DECODED_IMAGE_BYTES {
      return Err("decoded frame exceeds the image size limit".to_string());
   }
   let mut rgb = Vec::new();
   rgb.try_reserve_exact(yuv.rgb8_len())
      .map_err(|_| "decoded frame is too large".to_string())?;
   rgb.resize(yuv.rgb8_len(), 0);
   yuv.write_rgb8(&mut rgb);
   Ok((width as u32, height as u32, rgb))
}

fn parameter_sets_annex_b(config: &AvcConfig) -> Result<Vec<u8>, String> {
   let mut data = Vec::new();
   for parameter_set in config.sps.iter().chain(&config.pps) {
      append_annex_b_nal(&mut data, parameter_set)?;
   }
   if data.is_empty() {
      Err("avcC contains no SPS/PPS parameter sets".to_string())
   } else {
      Ok(data)
   }
}

fn sample_to_annex_b(sample: &[u8], length_size: usize) -> Result<Vec<u8>, String> {
   if !(1..=4).contains(&length_size) {
      return Err(format!("invalid H.264 NAL length size: {length_size}"));
   }

   let mut output = Vec::new();
   output
      .try_reserve(sample.len().saturating_add(4))
      .map_err(|_| "H.264 sample is too large".to_string())?;
   let mut offset = 0usize;
   while offset < sample.len() {
      let length_end = offset
         .checked_add(length_size)
         .ok_or_else(|| "NAL offset overflow".to_string())?;
      let length_bytes = sample
         .get(offset..length_end)
         .ok_or_else(|| "truncated NAL length".to_string())?;
      let nal_length = length_bytes
         .iter()
         .fold(0usize, |length, byte| (length << 8) | *byte as usize);
      offset = length_end;
      if nal_length == 0 {
         continue;
      }

      let nal_end = offset
         .checked_add(nal_length)
         .ok_or_else(|| "NAL size overflow".to_string())?;
      let nal = sample
         .get(offset..nal_end)
         .ok_or_else(|| "truncated NAL payload".to_string())?;
      append_annex_b_nal(&mut output, nal)?;
      offset = nal_end;
   }

   if output.is_empty() {
      Err("sample contained no H.264 NAL units".to_string())
   } else {
      Ok(output)
   }
}

fn append_annex_b_nal(output: &mut Vec<u8>, nal: &[u8]) -> Result<(), String> {
   if nal.is_empty() {
      return Err("empty H.264 NAL unit".to_string());
   }
   let additional = 4usize
      .checked_add(nal.len())
      .ok_or_else(|| "H.264 NAL size overflow".to_string())?;
   let total = output
      .len()
      .checked_add(additional)
      .ok_or_else(|| "H.264 sample size overflow".to_string())?;
   if total > MAX_DECODED_IMAGE_BYTES {
      return Err("H.264 sample exceeds the decode size limit".to_string());
   }
   output
      .try_reserve(additional)
      .map_err(|_| "H.264 sample is too large".to_string())?;
   output.extend_from_slice(&[0, 0, 0, 1]);
   output.extend_from_slice(nal);
   Ok(())
}

#[cfg(test)]
mod tests {
   use super::*;

   #[test]
   fn converts_length_prefixed_sample_to_annex_b() {
      let sample = [0, 0, 0, 2, 0x65, 0x88, 0, 0, 0, 1, 0x41];

      assert_eq!(
         sample_to_annex_b(&sample, 4).unwrap(),
         vec![0, 0, 0, 1, 0x65, 0x88, 0, 0, 0, 1, 0x41]
      );
   }
}

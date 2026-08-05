//! H.264/AVC decoding helpers backed by OpenH264.

use openh264::decoder::{DecodeOptions, DecodedYUV, Decoder, Flush};
use openh264::formats::YUVSource;
use std::sync::{
   Arc,
   atomic::{AtomicUsize, Ordering},
};

const MAX_DECODED_IMAGE_BYTES: usize = 64 * 1024 * 1024;

/// JPEG quality for encoded thumbnails, constrained to the encoder's 1–100
/// range so an out-of-range value cannot reach `jpeg_encoder`.
///
/// This knob trades size, not time: encoding a 1080p frame costs ~11 ms at
/// q40 and ~14 ms at q85, while the output grows from ~47 KiB to ~201 KiB.
/// Note that `jpeg_encoder` switches to 4:2:0 chroma subsampling below q90,
/// so 89 → 90 is a visible step rather than a smooth one.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct JpegQuality(u8);

impl JpegQuality {
   /// Thumbnail-grade default: ~64 KiB for a 1080p frame, where the size
   /// curve is still cheap.
   pub const DEFAULT: Self = Self(60);

   /// Returns `None` unless `quality` is within the encoder's 1–100 range.
   pub fn new(quality: u8) -> Option<Self> {
      (1..=100).contains(&quality).then_some(Self(quality))
   }

   pub fn get(self) -> u8 {
      self.0
   }
}

impl Default for JpegQuality {
   fn default() -> Self {
      Self::DEFAULT
   }
}

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

/// Request-scoped accounting shared by concurrent decode jobs.
#[derive(Debug, Clone)]
pub(crate) struct OutputBudget {
   max_bytes: Option<usize>,
   used_bytes: Arc<AtomicUsize>,
}

impl OutputBudget {
   pub(crate) fn new(max_bytes: Option<usize>) -> Self {
      Self {
         max_bytes,
         used_bytes: Arc::new(AtomicUsize::new(0)),
      }
   }

   fn reserve(&self, image_bytes: usize, output_count: usize) -> Result<(), String> {
      let Some(max_bytes) = self.max_bytes else {
         return Ok(());
      };
      let additional = image_bytes
         .checked_mul(output_count)
         .ok_or_else(|| "thumbnail payload is too large".to_string())?;
      let mut used = self.used_bytes.load(Ordering::Relaxed);
      loop {
         let total = used
            .checked_add(additional)
            .filter(|total| *total <= max_bytes)
            .ok_or_else(|| "thumbnail payload is too large".to_string())?;
         match self.used_bytes.compare_exchange_weak(
            used,
            total,
            Ordering::Relaxed,
            Ordering::Relaxed,
         ) {
            Ok(_) => return Ok(()),
            Err(current) => used = current,
         }
      }
   }

   #[cfg(test)]
   fn used(&self) -> usize {
      self.used_bytes.load(Ordering::Relaxed)
   }
}

struct OutputSelection<'a> {
   indices: &'a [usize],
   counts: &'a [usize],
   budget: &'a OutputBudget,
}

/// Decodes one GOP and encodes the selected presentation-order pictures.
pub fn decode_frames_to_jpeg(
   config: &AvcConfig,
   samples: &[Vec<u8>],
   output_indices: &[usize],
   output_counts: &[usize],
   quality: JpegQuality,
   output_budget: &OutputBudget,
) -> Result<Vec<DecodedImage>, String> {
   if samples.is_empty() {
      return Err("no H.264 samples to decode".to_string());
   }
   if output_indices.len() != output_counts.len() || output_counts.contains(&0) {
      return Err("invalid H.264 output multiplicities".to_string());
   }
   if output_indices
      .iter()
      .any(|output_index| *output_index >= samples.len())
   {
      return Err("requested H.264 output is outside the sample range".to_string());
   }
   let output_selection = OutputSelection {
      indices: output_indices,
      counts: output_counts,
      budget: output_budget,
   };

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
   // Scratch buffers reused across the GOP: every sample and every decoded
   // frame has the same shape, so after the first iteration `annex_b` and
   // `rgb` keep their capacity and `rgb`'s zero-fill becomes a no-op.
   let mut annex_b = Vec::new();
   let mut rgb = Vec::new();

   for sample in samples {
      sample_to_annex_b(sample, config.length_size, &mut annex_b)?;
      if let Some(yuv) = decoder
         .decode_with_options(&annex_b, no_flush.clone())
         .map_err(|error| error.to_string())?
      {
         store_selected_frame(
            &yuv,
            decoded_count,
            &output_selection,
            &mut selected,
            &mut rgb,
            quality,
         )?;
         decoded_count = decoded_count
            .checked_add(1)
            .ok_or_else(|| "decoded frame count overflow".to_string())?;
      }
   }

   for yuv in decoder
      .flush_remaining()
      .map_err(|error| error.to_string())?
   {
      store_selected_frame(
         &yuv,
         decoded_count,
         &output_selection,
         &mut selected,
         &mut rgb,
         quality,
      )?;
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
   output_selection: &OutputSelection<'_>,
   selected: &mut [Option<DecodedImage>],
   rgb: &mut Vec<u8>,
   quality: JpegQuality,
) -> Result<(), String> {
   let positions = output_selection
      .indices
      .iter()
      .enumerate()
      .filter_map(|(position, output_index)| (*output_index == decoded_index).then_some(position))
      .collect::<Vec<_>>();
   // Distinct timestamps can resolve to the same decoded frame; the last
   // position takes ownership so the common single-output case never copies.
   let Some((last_position, earlier_positions)) = positions.split_last() else {
      return Ok(());
   };
   let image = yuv_to_jpeg(yuv, rgb, quality)?;
   let output_count = positions.iter().try_fold(0usize, |total, position| {
      total.checked_add(output_selection.counts[*position])
   });
   output_selection.budget.reserve(
      image.data.len(),
      output_count.ok_or_else(|| "thumbnail output count overflow".to_string())?,
   )?;
   for position in earlier_positions {
      selected[*position] = Some(image.clone());
   }
   selected[*last_position] = Some(image);
   Ok(())
}

fn yuv_to_jpeg(
   yuv: &DecodedYUV<'_>,
   rgb: &mut Vec<u8>,
   quality: JpegQuality,
) -> Result<DecodedImage, String> {
   let (width, height) = yuv_to_rgb(yuv, rgb)?;
   let width_u16 =
      u16::try_from(width).map_err(|_| "frame width exceeds JPEG limits".to_string())?;
   let height_u16 =
      u16::try_from(height).map_err(|_| "frame height exceeds JPEG limits".to_string())?;
   let mut data = Vec::new();
   data
      .try_reserve(rgb.len())
      .map_err(|_| "JPEG output is too large".to_string())?;
   jpeg_encoder::Encoder::new(&mut data, quality.get())
      .encode(rgb, width_u16, height_u16, jpeg_encoder::ColorType::Rgb)
      .map_err(|error| error.to_string())?;

   Ok(DecodedImage {
      width,
      height,
      data,
   })
}

/// Converts `yuv` into `rgb`, which is grown in place. `write_rgb8` needs an
/// initialized slice, so the buffer is zero-filled the first time; reusing it
/// across frames of the same size makes the fill and the allocation no-ops.
fn yuv_to_rgb(yuv: &DecodedYUV<'_>, rgb: &mut Vec<u8>) -> Result<(u32, u32), String> {
   let (width, height) = yuv.dimensions();
   let rgb_len = yuv.rgb8_len();
   if rgb_len > MAX_DECODED_IMAGE_BYTES {
      return Err("decoded frame exceeds the image size limit".to_string());
   }
   rgb.try_reserve_exact(rgb_len.saturating_sub(rgb.len()))
      .map_err(|_| "decoded frame is too large".to_string())?;
   rgb.resize(rgb_len, 0);
   yuv.write_rgb8(rgb);
   Ok((width as u32, height as u32))
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

/// Rewrites a length-prefixed AVC sample into `output` as Annex B. `output` is
/// cleared first, so callers can reuse one buffer across a whole GOP.
fn sample_to_annex_b(
   sample: &[u8],
   length_size: usize,
   output: &mut Vec<u8>,
) -> Result<(), String> {
   if !(1..=4).contains(&length_size) {
      return Err(format!("invalid H.264 NAL length size: {length_size}"));
   }

   output.clear();
   // Lower bound only: with `length_size < 4` the 4-byte start codes make the
   // output longer than the input, and `append_annex_b_nal` grows from here.
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
      append_annex_b_nal(output, nal)?;
      offset = nal_end;
   }

   if output.is_empty() {
      Err("sample contained no H.264 NAL units".to_string())
   } else {
      Ok(())
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
   fn rejects_quality_outside_the_encoder_range() {
      assert_eq!(JpegQuality::new(0), None);
      assert_eq!(JpegQuality::new(101), None);
      assert_eq!(JpegQuality::new(1).map(JpegQuality::get), Some(1));
      assert_eq!(JpegQuality::new(100).map(JpegQuality::get), Some(100));
   }

   #[test]
   fn defaults_to_thumbnail_grade_quality() {
      assert_eq!(JpegQuality::default(), JpegQuality::DEFAULT);
      assert_eq!(JpegQuality::default().get(), 60);
   }

   #[test]
   fn output_budget_accepts_the_exact_weighted_limit() {
      let budget = OutputBudget::new(Some(12));

      budget
         .reserve(4, 3)
         .expect("three four-byte outputs fit exactly");

      assert_eq!(budget.used(), 12);
   }

   #[test]
   fn output_budget_rejects_without_consuming_the_failed_reservation() {
      let budget = OutputBudget::new(Some(11));

      let error = budget
         .reserve(4, 3)
         .expect_err("weighted output exceeds the byte budget");

      assert!(error.contains("thumbnail payload is too large"));
      assert_eq!(budget.used(), 0);
   }

   #[test]
   fn converts_length_prefixed_sample_to_annex_b() {
      let sample = [0, 0, 0, 2, 0x65, 0x88, 0, 0, 0, 1, 0x41];
      let mut output = Vec::new();

      sample_to_annex_b(&sample, 4, &mut output).unwrap();

      assert_eq!(output, vec![0, 0, 0, 1, 0x65, 0x88, 0, 0, 0, 1, 0x41]);
   }

   #[test]
   fn reuses_the_output_buffer_across_samples() {
      let first = [0, 0, 0, 2, 0x65, 0x88];
      let second = [0, 0, 0, 1, 0x41];
      let mut output = Vec::new();

      sample_to_annex_b(&first, 4, &mut output).unwrap();
      let capacity = output.capacity();
      sample_to_annex_b(&second, 4, &mut output).unwrap();

      assert_eq!(output, vec![0, 0, 0, 1, 0x41]);
      assert_eq!(output.capacity(), capacity);
   }

   #[test]
   fn expands_one_byte_length_prefixes_to_four_byte_start_codes() {
      let sample = [2, 0x65, 0x88, 1, 0x41];
      let mut output = Vec::new();

      sample_to_annex_b(&sample, 1, &mut output).unwrap();

      assert_eq!(output, vec![0, 0, 0, 1, 0x65, 0x88, 0, 0, 0, 1, 0x41]);
   }

   #[test]
   fn rejects_a_sample_without_nal_units() {
      let mut output = vec![0xff; 8];

      assert!(sample_to_annex_b(&[], 4, &mut output).is_err());
      assert!(output.is_empty());
   }
}

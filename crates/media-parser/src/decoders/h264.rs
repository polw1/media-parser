//! H.264/AVC decoding helpers backed by OpenH264.

use openh264::decoder::{DecodeOptions, DecodedYUV, Decoder, Flush};
use openh264::formats::YUVSource;
use std::{
   io::{self, Write},
   sync::{
      Arc,
      atomic::{AtomicUsize, Ordering},
   },
};

const MAX_DECODED_IMAGE_BYTES: usize = 64 * 1024 * 1024;
const DEFAULT_THUMBNAIL_BOUND: u32 = 320;

/// Aspect-ratio-preserving bounds applied before JPEG encoding.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ThumbnailSize {
   max_width: u32,
   max_height: u32,
}

impl ThumbnailSize {
   /// Creates non-zero output bounds accepted by the JPEG encoder.
   pub fn new(max_width: u32, max_height: u32) -> Option<Self> {
      (max_width > 0
         && max_height > 0
         && u16::try_from(max_width).is_ok()
         && u16::try_from(max_height).is_ok())
      .then_some(Self {
         max_width,
         max_height,
      })
   }
}

impl Default for ThumbnailSize {
   fn default() -> Self {
      Self {
         max_width: DEFAULT_THUMBNAIL_BOUND,
         max_height: DEFAULT_THUMBNAIL_BOUND,
      }
   }
}

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
   size: ThumbnailSize,
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
            size,
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
         size,
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
   size: ThumbnailSize,
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
   let image = yuv_to_jpeg(yuv, rgb, quality, size)?;
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
   size: ThumbnailSize,
) -> Result<DecodedImage, String> {
   let (width, height) = yuv_to_rgb(yuv, rgb, size)?;
   let width_u16 =
      u16::try_from(width).map_err(|_| "frame width exceeds JPEG limits".to_string())?;
   let height_u16 =
      u16::try_from(height).map_err(|_| "frame height exceeds JPEG limits".to_string())?;
   let mut output = FallibleJpegWriter::new(MAX_DECODED_IMAGE_BYTES);
   jpeg_encoder::Encoder::new(&mut output, quality.get())
      .encode(rgb, width_u16, height_u16, jpeg_encoder::ColorType::Rgb)
      .map_err(|error| error.to_string())?;

   Ok(DecodedImage {
      width,
      height,
      data: output.into_inner(),
   })
}

/// Grows with the compressed stream instead of reserving the much larger raw
/// RGB size. Allocation failures are surfaced through the encoder's I/O error.
struct FallibleJpegWriter {
   data: Vec<u8>,
   max_len: usize,
}

impl FallibleJpegWriter {
   fn new(max_len: usize) -> Self {
      Self {
         data: Vec::new(),
         max_len,
      }
   }

   fn into_inner(self) -> Vec<u8> {
      self.data
   }
}

impl Write for FallibleJpegWriter {
   fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
      let required_len = self
         .data
         .len()
         .checked_add(bytes.len())
         .filter(|len| *len <= self.max_len)
         .ok_or_else(|| io::Error::other("JPEG output is too large"))?;
      if required_len > self.data.capacity() {
         let target_capacity = self
            .data
            .capacity()
            .saturating_mul(2)
            .max(required_len)
            .min(self.max_len);
         self
            .data
            .try_reserve_exact(target_capacity - self.data.len())
            .map_err(|_| io::Error::other("JPEG output is too large"))?;
      }
      self.data.extend_from_slice(bytes);
      Ok(bytes.len())
   }

   fn flush(&mut self) -> io::Result<()> {
      Ok(())
   }
}

/// Converts `yuv` into `rgb`, which is grown in place. `write_rgb8` needs an
/// initialized slice, so the buffer is zero-filled the first time; reusing it
/// across frames of the same size makes the fill and the allocation no-ops.
fn yuv_to_rgb(
   yuv: &DecodedYUV<'_>,
   rgb: &mut Vec<u8>,
   size: ThumbnailSize,
) -> Result<(u32, u32), String> {
   let (source_width, source_height) = yuv.dimensions();
   let source_width =
      u32::try_from(source_width).map_err(|_| "decoded frame width is too large".to_string())?;
   let source_height =
      u32::try_from(source_height).map_err(|_| "decoded frame height is too large".to_string())?;
   let (width, height) = thumbnail_dimensions(source_width, source_height, size)?;
   let rgb_len = rgb_buffer_len(width, height)?;
   if rgb_len > MAX_DECODED_IMAGE_BYTES {
      return Err("decoded frame exceeds the image size limit".to_string());
   }
   rgb.try_reserve_exact(rgb_len.saturating_sub(rgb.len()))
      .map_err(|_| "decoded frame is too large".to_string())?;
   rgb.resize(rgb_len, 0);
   if (width, height) == (source_width, source_height) {
      yuv.write_rgb8(rgb);
   } else {
      write_scaled_rgb(yuv, rgb, width as usize, height as usize)?;
   }
   Ok((width, height))
}

fn thumbnail_dimensions(
   width: u32,
   height: u32,
   size: ThumbnailSize,
) -> Result<(u32, u32), String> {
   if width == 0 || height == 0 {
      return Err("decoded frame has zero dimensions".to_string());
   }
   if width <= size.max_width && height <= size.max_height {
      return Ok((width, height));
   }

   let width_limited = u64::from(size.max_width) * u64::from(height)
      <= u64::from(size.max_height) * u64::from(width);
   let (scaled_width, scaled_height) = if width_limited {
      let scaled_height = u64::from(height) * u64::from(size.max_width) / u64::from(width);
      (
         size.max_width,
         u32::try_from(scaled_height).unwrap_or(u32::MAX).max(1),
      )
   } else {
      let scaled_width = u64::from(width) * u64::from(size.max_height) / u64::from(height);
      (
         u32::try_from(scaled_width).unwrap_or(u32::MAX).max(1),
         size.max_height,
      )
   };
   Ok((scaled_width, scaled_height))
}

fn rgb_buffer_len(width: u32, height: u32) -> Result<usize, String> {
   usize::try_from(width)
      .ok()
      .and_then(|width| {
         usize::try_from(height)
            .ok()
            .and_then(|height| width.checked_mul(height))
      })
      .and_then(|pixels| pixels.checked_mul(3))
      .ok_or_else(|| "decoded frame is too large".to_string())
}

#[derive(Clone, Copy)]
struct AxisTaps {
   indices: [usize; 4],
   weights: [f32; 4],
   len: usize,
}

impl AxisTaps {
   fn bilinear(coordinate: f32, source_len: usize) -> Self {
      let first = coordinate.floor() as usize;
      let second = (first + 1).min(source_len - 1);
      let second_weight = coordinate - first as f32;
      Self {
         indices: [first, second, 0, 0],
         weights: [1.0 - second_weight, second_weight, 0.0, 0.0],
         len: 2,
      }
   }

   fn stratified(target: usize, target_len: usize, source_len: usize) -> Result<Self, String> {
      if source_len == 0 || target_len == 0 || target >= target_len {
         return Err("invalid scaled image axis".to_string());
      }
      let denominator = (target_len as u128)
         .checked_mul(8)
         .ok_or_else(|| "scaled image axis arithmetic overflow".to_string())?;
      let target_base = (target as u128)
         .checked_mul(8)
         .ok_or_else(|| "scaled image axis arithmetic overflow".to_string())?;
      let source_len = source_len as u128;
      let mut taps = Self {
         indices: [0; 4],
         weights: [0.25; 4],
         len: 4,
      };
      for sample in 0..4 {
         // Sample the center of each quarter of the output pixel's source
         // footprint. This is a bounded approximation of an area filter, not
         // FFmpeg's complete area filter, which can read every covered pixel.
         let numerator = target_base
            .checked_add((sample * 2 + 1) as u128)
            .ok_or_else(|| "scaled image axis arithmetic overflow".to_string())?;
         let index = numerator
            .checked_mul(source_len)
            .ok_or_else(|| "scaled image axis arithmetic overflow".to_string())?
            / denominator;
         taps.indices[sample] = usize::try_from(index.min(source_len - 1))
            .map_err(|_| "scaled image axis arithmetic overflow".to_string())?;
      }
      Ok(taps)
   }
}

fn axis_taps(source_len: usize, target_len: usize) -> Result<Vec<AxisTaps>, String> {
   if source_len == 0 || target_len == 0 {
      return Err("scaled image has zero dimensions".to_string());
   }
   let use_bilinear = target_len
      .checked_mul(2)
      .is_some_and(|twice_target_len| source_len <= twice_target_len);
   let mut taps = Vec::new();
   taps
      .try_reserve_exact(target_len)
      .map_err(|_| "scaled image is too large".to_string())?;
   for target in 0..target_len {
      taps.push(if use_bilinear {
         AxisTaps::bilinear(
            source_coordinate(target, target_len, source_len),
            source_len,
         )
      } else {
         AxisTaps::stratified(target, target_len, source_len)?
      });
   }
   Ok(taps)
}

struct ValidatedI420 {
   source_width: usize,
   source_height: usize,
   uv_width: usize,
   uv_height: usize,
   y_stride: usize,
   u_stride: usize,
   v_stride: usize,
}

fn validate_scaled_rgb(
   yuv: &impl YUVSource,
   target: &[u8],
   width: usize,
   height: usize,
) -> Result<ValidatedI420, String> {
   let (source_width, source_height) = yuv.dimensions();
   if source_width == 0 || source_height == 0 || width == 0 || height == 0 {
      return Err("scaled image has zero dimensions".to_string());
   }
   if source_width % 2 != 0 || source_height % 2 != 0 {
      return Err("decoded frame is not even I420".to_string());
   }
   let (y_stride, u_stride, v_stride) = yuv.strides();
   let uv_width = source_width / 2;
   let uv_height = source_height / 2;
   if y_stride < source_width || u_stride < uv_width || v_stride < uv_width {
      return Err("decoded I420 stride is too small".to_string());
   }
   let y_len = y_stride
      .checked_mul(source_height)
      .ok_or_else(|| "decoded I420 plane length overflow".to_string())?;
   let u_len = u_stride
      .checked_mul(uv_height)
      .ok_or_else(|| "decoded I420 plane length overflow".to_string())?;
   let v_len = v_stride
      .checked_mul(uv_height)
      .ok_or_else(|| "decoded I420 plane length overflow".to_string())?;
   if yuv.y().len() < y_len || yuv.u().len() < u_len || yuv.v().len() < v_len {
      return Err("decoded I420 plane is too short".to_string());
   }
   let target_len = width
      .checked_mul(height)
      .and_then(|pixels| pixels.checked_mul(3))
      .ok_or_else(|| "scaled RGB target length overflow".to_string())?;
   if target.len() != target_len {
      return Err("scaled RGB target has an invalid length".to_string());
   }
   Ok(ValidatedI420 {
      source_width,
      source_height,
      uv_width,
      uv_height,
      y_stride,
      u_stride,
      v_stride,
   })
}

fn write_scaled_rgb(
   yuv: &impl YUVSource,
   target: &mut [u8],
   width: usize,
   height: usize,
) -> Result<(), String> {
   const Y_MUL: f32 = 255.0 / 219.0;
   const RV_MUL: f32 = 255.0 / 224.0 * 1.402;
   const GV_MUL: f32 = -255.0 / 224.0 * 1.402 * 0.299 / 0.687;
   const GU_MUL: f32 = -255.0 / 224.0 * 1.772 * 0.114 / 0.587;
   const BU_MUL: f32 = 255.0 / 224.0 * 1.772;

   let source = validate_scaled_rgb(yuv, target, width, height)?;
   let y_x_taps = axis_taps(source.source_width, width)?;
   let y_y_taps = axis_taps(source.source_height, height)?;
   // Chroma uses its own normalized plane extent. This assumes the aligned
   // I420 layout supplied by OpenH264; VUI chroma siting is outside this path.
   let uv_x_taps = axis_taps(source.uv_width, width)?;
   let uv_y_taps = axis_taps(source.uv_height, height)?;
   for target_y in 0..height {
      let y_y = y_y_taps[target_y];
      let uv_y = uv_y_taps[target_y];
      for target_x in 0..width {
         let y = separable_sample(yuv.y(), source.y_stride, y_x_taps[target_x], y_y);
         let u = separable_sample(yuv.u(), source.u_stride, uv_x_taps[target_x], uv_y) - 128.0;
         let v = separable_sample(yuv.v(), source.v_stride, uv_x_taps[target_x], uv_y) - 128.0;
         let y = Y_MUL * (y - 16.0);
         let offset = (target_y * width + target_x) * 3;
         target[offset] = RV_MUL.mul_add(v, y) as u8;
         target[offset + 1] = GV_MUL.mul_add(v, GU_MUL.mul_add(u, y)) as u8;
         target[offset + 2] = BU_MUL.mul_add(u, y) as u8;
      }
   }
   Ok(())
}

fn separable_sample(plane: &[u8], stride: usize, x_taps: AxisTaps, y_taps: AxisTaps) -> f32 {
   let mut value = 0.0;
   for y in 0..y_taps.len {
      let row = y_taps.indices[y] * stride;
      for x in 0..x_taps.len {
         value += f32::from(plane[row + x_taps.indices[x]]) * y_taps.weights[y] * x_taps.weights[x];
      }
   }
   value
}

fn source_coordinate(target: usize, target_len: usize, source_len: usize) -> f32 {
   (((target as f32 + 0.5) * source_len as f32 / target_len as f32) - 0.5)
      .clamp(0.0, source_len.saturating_sub(1) as f32)
}

#[cfg(test)]
fn bilinear_sample(
   plane: &[u8],
   stride: usize,
   width: usize,
   height: usize,
   x: f32,
   y: f32,
) -> f32 {
   let x0 = x.floor() as usize;
   let y0 = y.floor() as usize;
   let x1 = (x0 + 1).min(width - 1);
   let y1 = (y0 + 1).min(height - 1);
   let x_weight = x - x0 as f32;
   let y_weight = y - y0 as f32;
   let top = f32::from(plane[y0 * stride + x0]) * (1.0 - x_weight)
      + f32::from(plane[y0 * stride + x1]) * x_weight;
   let bottom = f32::from(plane[y1 * stride + x0]) * (1.0 - x_weight)
      + f32::from(plane[y1 * stride + x1]) * x_weight;
   top * (1.0 - y_weight) + bottom * y_weight
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

   struct FakeYuv<'a> {
      dimensions: (usize, usize),
      strides: (usize, usize, usize),
      y: &'a [u8],
      u: &'a [u8],
      v: &'a [u8],
   }

   impl YUVSource for FakeYuv<'_> {
      fn dimensions(&self) -> (usize, usize) {
         self.dimensions
      }

      fn strides(&self) -> (usize, usize, usize) {
         self.strides
      }

      fn y(&self) -> &[u8] {
         self.y
      }

      fn u(&self) -> &[u8] {
         self.u
      }

      fn v(&self) -> &[u8] {
         self.v
      }
   }

   #[test]
   fn jpeg_writer_grows_geometrically_and_enforces_its_limit() {
      let mut output = FallibleJpegWriter::new(8);
      let mut capacity_changes = 0;
      let mut capacity = output.data.capacity();

      for byte in 0..8 {
         output.write_all(&[byte]).expect("write within limit");
         if output.data.capacity() != capacity {
            capacity_changes += 1;
            capacity = output.data.capacity();
         }
      }

      assert_eq!(output.data, (0..8).collect::<Vec<_>>());
      assert!(capacity_changes <= 4);
      assert!(output.write_all(&[8]).is_err());
      assert_eq!(output.data, (0..8).collect::<Vec<_>>());
   }

   #[test]
   fn default_thumbnail_size_downscales_8k_before_rgb_allocation() {
      let size = ThumbnailSize::default();

      assert_eq!(thumbnail_dimensions(7_680, 4_320, size), Ok((320, 180)));
      assert_eq!(rgb_buffer_len(320, 180), Ok(320 * 180 * 3));
   }

   #[test]
   fn thumbnail_size_preserves_aspect_ratio_without_upscaling() {
      let size = ThumbnailSize::new(640, 640).expect("non-zero bounds are valid");

      assert_eq!(thumbnail_dimensions(1_920, 1_080, size), Ok((640, 360)));
      assert_eq!(thumbnail_dimensions(1_080, 1_920, size), Ok((360, 640)));
      assert_eq!(thumbnail_dimensions(160, 90, size), Ok((160, 90)));
   }

   #[test]
   fn bilinear_yuv_scaling_interpolates_pixels_and_clamps_plane_edges() {
      use openh264::formats::YUVSlices;

      let y = [16, 235, 81, 145];
      let u = [128];
      let v = [128];
      let source = YUVSlices::new((&y, &u, &v), (2, 2), (2, 1, 1));
      let mut rgb = [0; 3];

      write_scaled_rgb(&source, &mut rgb, 1, 1).expect("valid source scales");

      assert_eq!(rgb, [120, 120, 120]);
      assert_eq!(bilinear_sample(&y, 2, 2, 2, 1.0, 1.0), 145.0);
   }

   #[test]
   fn moderate_downscale_keeps_centered_bilinear_taps() {
      use openh264::formats::YUVSlices;

      // 10 -> 6 is within the bilinear threshold. At x=0, centered
      // bilinear produces Y=84 (RGB=79), unlike the four strata (RGB=118).
      let y = [
         16, 220, 16, 16, 16, 16, 16, 16, 16, 16, 16, 220, 16, 16, 16, 16, 16, 16, 16, 16,
      ];
      let u = [128; 5];
      let v = [128; 5];
      let source = YUVSlices::new((&y, &u, &v), (10, 2), (10, 5, 5));
      let mut rgb = [0; 18];

      write_scaled_rgb(&source, &mut rgb, 6, 1).expect("valid source scales");

      assert_eq!(&rgb[..3], &[79, 79, 79]);
   }

   #[test]
   fn stratified_taps_do_not_overflow_for_large_dimensions() {
      let result =
         std::panic::catch_unwind(|| AxisTaps::stratified(usize::MAX - 1, usize::MAX, usize::MAX))
            .expect("large dimensions must not panic");

      if let Ok(taps) = result {
         assert_eq!(taps.len, 4);
         assert!(taps.indices.iter().all(|index| *index < usize::MAX));
         assert!((taps.weights.iter().sum::<f32>() - 1.0).abs() < f32::EPSILON);
      }
   }

   #[test]
   fn severe_downscale_stratifies_a_fine_luma_pattern() {
      use openh264::formats::YUVSlices;

      // A single bilinear sample lands between the two dark pixels at x=3/4.
      // Four samples across the 8-pixel footprint retain the surrounding bright
      // detail instead of turning the output black.
      let y = [
         235, 235, 235, 16, 16, 235, 235, 235, 235, 235, 235, 16, 16, 235, 235, 235, 235, 235, 235,
         16, 16, 235, 235, 235, 235, 235, 235, 16, 16, 235, 235, 235,
      ];
      let u = [128; 8];
      let v = [128; 8];
      let source = YUVSlices::new((&y, &u, &v), (16, 2), (16, 8, 8));
      let mut rgb = [0; 6];

      write_scaled_rgb(&source, &mut rgb, 2, 1).expect("valid source scales");

      assert_eq!(rgb, [191, 191, 191, 191, 191, 191]);
   }

   #[test]
   fn scales_chroma_in_its_own_normalized_plane_extent() {
      let y = [16; 16];
      let u = [16, 240, 16, 240];
      let v = [128; 4];
      let source = FakeYuv {
         dimensions: (4, 4),
         strides: (4, 2, 2),
         y: &y,
         u: &u,
         v: &v,
      };
      let mut rgb = [0; 3];

      write_scaled_rgb(&source, &mut rgb, 1, 1).expect("valid I420 source scales");

      assert_eq!(rgb, [0, 0, 0]);
   }

   #[test]
   fn scales_to_an_odd_sized_target() {
      let y = [16; 12];
      let u = [128; 3];
      let v = [128; 3];
      let source = FakeYuv {
         dimensions: (6, 2),
         strides: (6, 3, 3),
         y: &y,
         u: &u,
         v: &v,
      };
      let mut rgb = [0; 9];

      write_scaled_rgb(&source, &mut rgb, 3, 1).expect("odd target is valid");

      assert_eq!(rgb, [0; 9]);
   }

   #[test]
   fn rejects_invalid_i420_sources_and_targets() {
      let y = [16; 6];
      let uv = [128; 2];
      let target = &mut [0; 3];

      let zero_sized = FakeYuv {
         dimensions: (0, 2),
         strides: (0, 0, 0),
         y: &[],
         u: &[],
         v: &[],
      };
      assert!(
         write_scaled_rgb(&zero_sized, target, 1, 1)
            .expect_err("zero source is invalid")
            .contains("zero dimensions")
      );

      let odd_sized = FakeYuv {
         dimensions: (3, 2),
         strides: (3, 1, 1),
         y: &y,
         u: &uv[..1],
         v: &uv[..1],
      };
      assert!(
         write_scaled_rgb(&odd_sized, target, 1, 1)
            .expect_err("odd I420 source is invalid")
            .contains("even I420")
      );

      let short_stride = FakeYuv {
         dimensions: (2, 2),
         strides: (1, 0, 0),
         y: &y[..2],
         u: &[],
         v: &[],
      };
      assert!(
         write_scaled_rgb(&short_stride, target, 1, 1)
            .expect_err("short stride is invalid")
            .contains("stride")
      );

      let short_plane = FakeYuv {
         dimensions: (2, 2),
         strides: (2, 1, 1),
         y: &y[..3],
         u: &uv[..1],
         v: &uv[..1],
      };
      assert!(
         write_scaled_rgb(&short_plane, target, 1, 1)
            .expect_err("short plane is invalid")
            .contains("plane is too short")
      );

      let valid_source = FakeYuv {
         dimensions: (2, 2),
         strides: (2, 1, 1),
         y: &y[..4],
         u: &uv[..1],
         v: &uv[..1],
      };
      assert!(
         write_scaled_rgb(&valid_source, &mut [0; 2], 1, 1)
            .expect_err("wrong target length is invalid")
            .contains("target has an invalid length")
      );
   }

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

//! Binary envelopes used to return media payloads through Tauri IPC.
//!
//! Layout:
//! - A little-endian `u32` containing the JSON header length.
//! - A JSON header object `{ "version": <u32>, "entries": [...] }`.
//! - Concatenated binary payloads.
//!
//! Entry `offset` values are relative to the beginning of the payload region,
//! immediately after the JSON header. `version` identifies the shape of the
//! entries so decoders can reject a header they don't understand instead of
//! misreading it; bump it whenever an entry's fields change shape.

use media_parser::{CoverArt, Frame, PixelFormat, SubtitleTrack};
use serde::Serialize;
use std::collections::HashMap;
use std::io::{self, Write};

use crate::Result;

pub(crate) async fn run_envelope_task<T, F>(label: &'static str, task: F) -> Result<T>
where
   T: Send + 'static,
   F: FnOnce() -> Result<T> + Send + 'static,
{
   tauri::async_runtime::spawn_blocking(task)
      .await
      .map_err(|error| envelope_task_error(label, error))?
}

fn envelope_task_error(label: &str, error: impl std::fmt::Display) -> crate::Error {
   crate::Error::Custom(format!("{label} envelope task failed: {error}"))
}

/// Current envelope format version. Decoders should reject any header whose
/// `version` they don't recognize rather than guessing at its shape.
const ENVELOPE_VERSION: u32 = 1;
const ENVELOPE_PREFIX_BYTES: usize = std::mem::size_of::<u32>();
const SUBTITLE_ENVELOPE_EMPTY_HEADER_BYTES: usize = br#"{"version":1,"entries":[]}"#.len();
const SUBTITLE_ENVELOPE_PROJECTED_BASE_BYTES: usize =
   ENVELOPE_PREFIX_BYTES + SUBTITLE_ENVELOPE_EMPTY_HEADER_BYTES;
pub(crate) const MAX_SUBTITLE_OUTPUT_BYTES: usize = 64 * 1024 * 1024;
const SUBTITLE_TRACK_PROJECTION_BYTES: usize = 512;
const SUBTITLE_CUE_PROJECTION_BYTES: usize = 160;

/// Largest integer that JavaScript can represent without losing precision.
pub(crate) const JS_MAX_SAFE_INTEGER: u64 = 9_007_199_254_740_991;

#[derive(Serialize)]
struct EnvelopeHeader<T> {
   version: u32,
   entries: Vec<T>,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct CoverEnvelopeEntry {
   format: &'static str,
   mime_type: &'static str,
   offset: usize,
   length: usize,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct ThumbnailEnvelopeEntry {
   track_id: u32,
   width: u32,
   height: u32,
   timestamp_sec: f64,
   format: &'static str,
   mime_type: &'static str,
   offset: usize,
   length: usize,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct SubtitleEnvelopeEntry<'a> {
   id: u32,
   codec: &'a str,
   #[serde(skip_serializing_if = "Option::is_none")]
   language: Option<&'a str>,
   timescale: u32,
   duration: u64,
   cues: Vec<SubtitleCueEnvelopeEntry>,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct SubtitleCueEnvelopeEntry {
   cue_id: u32,
   start_sec: f64,
   end_sec: f64,
   offset: usize,
   length: usize,
}

pub(crate) fn cover_envelope(cover: Option<CoverArt>) -> Result<Vec<u8>> {
   let Some(cover) = cover else {
      return encode_binary_envelope(Vec::<CoverEnvelopeEntry>::new(), &[], usize::MAX);
   };
   if !matches!(&cover.format, PixelFormat::Jpeg | PixelFormat::Png) {
      return Err(crate::Error::Custom(format!(
         "cover must be JPEG or PNG, got {}",
         cover.format.label()
      )));
   }
   let entry = CoverEnvelopeEntry {
      format: cover.format.label(),
      mime_type: cover.format.mime_type(),
      offset: 0,
      length: cover.data.len(),
   };
   let payloads = [cover.data.as_slice()];
   encode_binary_envelope(vec![entry], &payloads, usize::MAX)
}

/// Builds one thumbnail entry after `encode_thumbnail_envelope` has enforced
/// the JPEG-only contract over every payload frame.
///
/// `ThumbnailInfo` publishes `format: 'jpeg'` and `mimeType: 'image/jpeg'` as
/// closed literals, and the TypeScript decoder casts the header without
/// validating it. `Frame::format` is open over the whole `PixelFormat` enum, so
/// any other format is a bug in the decode path.
fn thumbnail_envelope_entry(frame: &Frame, offset: usize) -> ThumbnailEnvelopeEntry {
   ThumbnailEnvelopeEntry {
      track_id: frame.track_id,
      width: frame.width,
      height: frame.height,
      timestamp_sec: frame.timestamp.as_secs_f64(),
      format: frame.format.label(),
      mime_type: frame.format.mime_type(),
      offset,
      length: frame.data.len(),
   }
}

/// Encodes one metadata entry per requested timestamp into the binary
/// envelope. `order` maps each output entry to a frame in `frames`, so
/// duplicate timestamps share the same payload bytes.
pub(crate) fn encode_thumbnail_envelope(
   frames: &[Frame],
   order: &[usize],
   max_output_bytes: usize,
) -> Result<Vec<u8>> {
   let mut offsets = Vec::new();
   offsets
      .try_reserve_exact(frames.len())
      .map_err(|_| crate::Error::Custom("too many thumbnail entries".to_string()))?;
   let mut payload_len = 0usize;
   for frame in frames {
      if frame.format != PixelFormat::Jpeg {
         return Err(crate::Error::Custom(format!(
            "thumbnail must be JPEG, got {}",
            frame.format.label()
         )));
      }
      offsets.push(payload_len);
      payload_len = payload_len
         .checked_add(frame.data.len())
         .filter(|total| *total <= max_output_bytes)
         .ok_or_else(|| crate::Error::Custom("thumbnail payload is too large".to_string()))?;
   }

   let mut entries = Vec::new();
   entries
      .try_reserve_exact(order.len())
      .map_err(|_| crate::Error::Custom("too many thumbnail entries".to_string()))?;
   for &index in order {
      let frame = frames
         .get(index)
         .ok_or_else(|| crate::Error::Custom("thumbnail frame index out of range".to_string()))?;
      entries.push(thumbnail_envelope_entry(frame, offsets[index]));
   }
   let mut payloads = Vec::new();
   payloads
      .try_reserve_exact(frames.len())
      .map_err(|_| crate::Error::Custom("too many thumbnail payloads".to_string()))?;
   for frame in frames {
      payloads.push(frame.data.as_slice());
   }
   encode_binary_envelope(entries, &payloads, max_output_bytes)
}

/// Encodes subtitle metadata and deduplicated UTF-8 cue text into one bounded
/// version-1 binary envelope.
pub(crate) fn encode_subtitle_envelope(
   tracks: &[SubtitleTrack],
   max_output_bytes: usize,
) -> Result<Vec<u8>> {
   let output_cap = max_output_bytes.min(MAX_SUBTITLE_OUTPUT_BYTES);
   let mut projected_bytes = 0usize;
   charge_subtitle_projection(
      &mut projected_bytes,
      SUBTITLE_ENVELOPE_PROJECTED_BASE_BYTES,
      output_cap,
   )?;
   let mut cue_count = 0usize;

   for track in tracks {
      charge_subtitle_projection(
         &mut projected_bytes,
         SUBTITLE_TRACK_PROJECTION_BYTES,
         output_cap,
      )?;
      require_js_safe_u64(u64::from(track.base.id), "subtitle track id")?;
      require_js_safe_u64(u64::from(track.base.timescale), "subtitle track timescale")?;
      require_js_safe_u64(track.base.duration, "subtitle duration")?;
      cue_count = cue_count
         .checked_add(track.cues.len())
         .ok_or_else(subtitle_envelope_too_large)?;

      for cue in &track.cues {
         require_js_safe_u64(u64::from(cue.cue_id), "subtitle cue id")?;
         let start_sec = cue.start_time.as_secs_f64();
         let end_sec = cue.end_time.as_secs_f64();
         if !start_sec.is_finite()
            || !end_sec.is_finite()
            || start_sec < 0.0
            || end_sec <= start_sec
         {
            return Err(crate::Error::Custom(
               "invalid subtitle cue timing".to_string(),
            ));
         }
         charge_subtitle_projection(
            &mut projected_bytes,
            SUBTITLE_CUE_PROJECTION_BYTES,
            output_cap,
         )?;
         charge_subtitle_projection(&mut projected_bytes, cue.text.len(), output_cap)?;
      }
   }

   let mut text_ranges = HashMap::<&str, (usize, usize)>::new();
   text_ranges
      .try_reserve(cue_count)
      .map_err(|_| subtitle_envelope_too_large())?;
   let mut payload_len = 0usize;
   for cue in tracks.iter().flat_map(|track| &track.cues) {
      if let std::collections::hash_map::Entry::Vacant(entry) = text_ranges.entry(cue.text.as_str())
      {
         let length = cue.text.len();
         require_js_safe_usize(payload_len, "subtitle payload offset")?;
         require_js_safe_usize(length, "subtitle payload length")?;
         let offset = payload_len;
         payload_len = payload_len
            .checked_add(length)
            .filter(|total| *total <= output_cap)
            .ok_or_else(subtitle_envelope_too_large)?;
         entry.insert((offset, length));
      }
   }

   let mut payloads = Vec::new();
   payloads
      .try_reserve_exact(text_ranges.len())
      .map_err(|_| subtitle_envelope_too_large())?;
   for text in text_ranges.keys() {
      payloads.push(text.as_bytes());
   }
   payloads.sort_unstable_by_key(|payload| {
      let text = std::str::from_utf8(payload).expect("subtitle payloads originate from UTF-8 text");
      text_ranges
         .get(text)
         .expect("every subtitle payload has a deduplicated range")
         .0
   });

   let mut entries = Vec::new();
   entries
      .try_reserve_exact(tracks.len())
      .map_err(|_| subtitle_envelope_too_large())?;
   for track in tracks {
      let mut cues = Vec::new();
      cues
         .try_reserve_exact(track.cues.len())
         .map_err(|_| subtitle_envelope_too_large())?;
      for cue in &track.cues {
         let &(offset, length) = text_ranges
            .get(cue.text.as_str())
            .expect("every subtitle cue has a deduplicated range");
         cues.push(SubtitleCueEnvelopeEntry {
            cue_id: cue.cue_id,
            start_sec: cue.start_time.as_secs_f64(),
            end_sec: cue.end_time.as_secs_f64(),
            offset,
            length,
         });
      }
      entries.push(SubtitleEnvelopeEntry {
         id: track.base.id,
         codec: track.base.codec.as_str(),
         language: track.base.language.as_deref(),
         timescale: track.base.timescale,
         duration: track.base.duration,
         cues,
      });
   }

   encode_binary_envelope(entries, &payloads, output_cap)
}

fn charge_subtitle_projection(total: &mut usize, bytes: usize, cap: usize) -> Result<()> {
   *total = total
      .checked_add(bytes)
      .filter(|total| *total <= cap)
      .ok_or_else(subtitle_envelope_too_large)?;
   Ok(())
}

fn subtitle_envelope_too_large() -> crate::Error {
   crate::Error::Custom("subtitle envelope is too large".to_string())
}

fn require_js_safe_u64(value: u64, field: &str) -> Result<()> {
   if value > JS_MAX_SAFE_INTEGER {
      return Err(crate::Error::Custom(format!(
         "{field} exceeds the JavaScript safe integer limit"
      )));
   }
   Ok(())
}

fn require_js_safe_usize(value: usize, field: &str) -> Result<()> {
   let value = u64::try_from(value).map_err(|_| {
      crate::Error::Custom(format!("{field} exceeds the JavaScript safe integer limit"))
   })?;
   require_js_safe_u64(value, field)
}

fn encode_binary_envelope<T: Serialize>(
   entries: Vec<T>,
   payloads: &[&[u8]],
   max_output_bytes: usize,
) -> Result<Vec<u8>> {
   let payload_len = payloads
      .iter()
      .try_fold(0usize, |total, payload| total.checked_add(payload.len()))
      .ok_or_else(|| crate::Error::Custom("envelope payload is too large".to_string()))?;
   let header_cap = checked_header_cap(max_output_bytes, payload_len)?;
   let header = EnvelopeHeader {
      version: ENVELOPE_VERSION,
      entries,
   };
   let mut counter = CountingWriter::new(header_cap);
   serde_json::to_writer(&mut counter, &header)
      .map_err(|error| crate::Error::Custom(format!("could not encode envelope: {error}")))?;
   let header_len = counter.len();
   let header_len_prefix = u32::try_from(header_len)
      .map_err(|_| crate::Error::Custom("envelope header is too large".to_string()))?;
   let envelope_len = checked_envelope_len(header_len, payload_len, max_output_bytes)?;
   let mut envelope = Vec::new();
   envelope
      .try_reserve_exact(envelope_len)
      .map_err(|_| envelope_too_large())?;
   envelope.extend_from_slice(&header_len_prefix.to_le_bytes());
   let header_end = ENVELOPE_PREFIX_BYTES
      .checked_add(header_len)
      .ok_or_else(envelope_too_large)?;
   {
      let mut writer = FixedVecWriter::new(&mut envelope, header_end);
      serde_json::to_writer(&mut writer, &header)
         .map_err(|error| crate::Error::Custom(format!("could not encode envelope: {error}")))?;
   }
   if envelope.len() != header_end {
      return Err(crate::Error::Custom(
         "envelope header size changed during encoding".to_string(),
      ));
   }
   for payload in payloads {
      envelope.extend_from_slice(payload);
   }
   if envelope.len() != envelope_len {
      return Err(envelope_too_large());
   }
   Ok(envelope)
}

fn checked_envelope_len(
   header_len: usize,
   payload_len: usize,
   max_output_bytes: usize,
) -> Result<usize> {
   ENVELOPE_PREFIX_BYTES
      .checked_add(header_len)
      .and_then(|length| length.checked_add(payload_len))
      .filter(|length| *length <= max_output_bytes)
      .ok_or_else(envelope_too_large)
}

fn checked_header_cap(max_output_bytes: usize, payload_len: usize) -> Result<usize> {
   max_output_bytes
      .checked_sub(ENVELOPE_PREFIX_BYTES)
      .and_then(|remaining| remaining.checked_sub(payload_len))
      .map(|cap| cap.min(u32::MAX as usize))
      .ok_or_else(envelope_too_large)
}

fn envelope_too_large() -> crate::Error {
   crate::Error::Custom("envelope is too large".to_string())
}

struct CountingWriter {
   length: usize,
   cap: usize,
}

impl CountingWriter {
   fn new(cap: usize) -> Self {
      Self { length: 0, cap }
   }

   fn len(&self) -> usize {
      self.length
   }
}

impl Write for CountingWriter {
   fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
      self.length = self
         .length
         .checked_add(bytes.len())
         .filter(|length| *length <= self.cap)
         .ok_or_else(writer_limit_error)?;
      Ok(bytes.len())
   }

   fn flush(&mut self) -> io::Result<()> {
      Ok(())
   }
}

struct FixedVecWriter<'a> {
   output: &'a mut Vec<u8>,
   end: usize,
}

impl<'a> FixedVecWriter<'a> {
   fn new(output: &'a mut Vec<u8>, end: usize) -> Self {
      Self { output, end }
   }
}

impl Write for FixedVecWriter<'_> {
   fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
      let new_len = self
         .output
         .len()
         .checked_add(bytes.len())
         .filter(|length| *length <= self.end && *length <= self.output.capacity())
         .ok_or_else(writer_limit_error)?;
      self.output.extend_from_slice(bytes);
      debug_assert_eq!(self.output.len(), new_len);
      Ok(bytes.len())
   }

   fn flush(&mut self) -> io::Result<()> {
      Ok(())
   }
}

fn writer_limit_error() -> io::Error {
   io::Error::other("envelope is too large")
}

#[cfg(test)]
mod tests {
   use super::*;
   use media_parser::{BaseTrackMeta, PixelFormat, SubtitleCue, SubtitleTrack};
   use serde::ser::SerializeSeq;
   use std::collections::HashMap;
   use std::sync::atomic::{AtomicUsize, Ordering};
   use std::time::Duration;

   /// Every frame here is JPEG because that is the only format the envelope
   /// accepts, matching what the decode path produces and what `ThumbnailInfo`
   /// publishes. See
   /// `thumbnail_envelope_rejects_an_unreferenced_non_jpeg_frame` for the
   /// boundary check itself.
   fn test_frames() -> Vec<Frame> {
      vec![
         Frame {
            track_id: 3,
            width: 320,
            height: 180,
            timestamp: Duration::from_millis(250),
            format: PixelFormat::Jpeg,
            data: vec![1, 2, 3],
            strides: None,
         },
         Frame {
            track_id: 3,
            width: 640,
            height: 360,
            timestamp: Duration::from_secs(1),
            format: PixelFormat::Jpeg,
            data: vec![4, 5],
            strides: None,
         },
      ]
   }

   fn envelope_parts(envelope: &[u8]) -> (serde_json::Value, &[u8]) {
      let header_len =
         u32::from_le_bytes(envelope[..ENVELOPE_PREFIX_BYTES].try_into().unwrap()) as usize;
      let header_end = ENVELOPE_PREFIX_BYTES + header_len;
      let header = serde_json::from_slice(&envelope[ENVELOPE_PREFIX_BYTES..header_end])
         .expect("header should be JSON");
      (header, &envelope[header_end..])
   }

   fn subtitle_track(language: Option<&str>, cues: Vec<SubtitleCue>) -> SubtitleTrack {
      SubtitleTrack {
         base: BaseTrackMeta {
            id: u32::MAX,
            codec: "wvtt".to_owned(),
            language: language.map(str::to_owned),
            timescale: u32::MAX,
            duration: JS_MAX_SAFE_INTEGER,
            properties: HashMap::new(),
         },
         cues,
      }
   }

   fn subtitle_cue(cue_id: u32, start_ms: u64, end_ms: u64, text: &str) -> SubtitleCue {
      SubtitleCue {
         cue_id,
         start_time: Duration::from_millis(start_ms),
         end_time: Duration::from_millis(end_ms),
         text: text.to_owned(),
      }
   }

   #[test]
   fn encodes_cover_as_a_single_entry_binary_envelope() {
      let envelope = cover_envelope(Some(CoverArt {
         format: PixelFormat::Jpeg,
         mime_type: "image/jpeg".to_string(),
         data: vec![1, 2, 3],
      }))
      .expect("cover should encode");
      let (header, payload) = envelope_parts(&envelope);

      assert_eq!(
         header,
         serde_json::json!({
            "version": 1,
            "entries": [{
               "format": "jpeg",
               "mimeType": "image/jpeg",
               "offset": 0,
               "length": 3,
            }],
         })
      );
      assert_eq!(payload, &[1, 2, 3]);
   }

   /// Unlike thumbnails, covers carry the format the file declares, so this
   /// pins that the entry reports the cover's own format instead of a constant.
   #[test]
   fn encodes_a_png_cover_with_its_own_format() {
      let envelope = cover_envelope(Some(CoverArt {
         format: PixelFormat::Png,
         // The envelope must derive this from `format`, not trust a second
         // independently mutable field.
         mime_type: "image/jpeg".to_string(),
         data: vec![4, 5],
      }))
      .expect("cover should encode");
      let (header, payload) = envelope_parts(&envelope);

      assert_eq!(
         header,
         serde_json::json!({
            "version": 1,
            "entries": [{
               "format": "png",
               "mimeType": "image/png",
               "offset": 0,
               "length": 2,
            }],
         })
      );
      assert_eq!(payload, &[4, 5]);
   }

   #[test]
   fn cover_envelope_rejects_an_unsupported_pixel_format() {
      let error = cover_envelope(Some(CoverArt {
         format: PixelFormat::Rgb24,
         mime_type: "application/octet-stream".to_string(),
         data: vec![1, 2, 3],
      }))
      .expect_err("a raw pixel buffer must not reach the published cover envelope");

      assert_eq!(error.to_string(), "cover must be JPEG or PNG, got rgb24");
   }

   #[test]
   fn encodes_missing_cover_as_an_empty_envelope() {
      let envelope = cover_envelope(None).expect("empty cover should encode");
      let (header, payload) = envelope_parts(&envelope);

      assert_eq!(header, serde_json::json!({ "version": 1, "entries": [] }));
      assert!(payload.is_empty());
   }

   #[test]
   fn encodes_thumbnail_metadata_and_image_bytes_in_one_binary_envelope() {
      let envelope = encode_thumbnail_envelope(&test_frames(), &[0, 1], usize::MAX)
         .expect("envelope should encode");
      let (header, payload) = envelope_parts(&envelope);

      assert_eq!(
         header,
         serde_json::json!({
            "version": 1,
            "entries": [
               {
                  "trackId": 3,
                  "width": 320,
                  "height": 180,
                  "timestampSec": 0.25,
                  "format": "jpeg",
                  "mimeType": "image/jpeg",
                  "offset": 0,
                  "length": 3,
               },
               {
                  "trackId": 3,
                  "width": 640,
                  "height": 360,
                  "timestampSec": 1.0,
                  "format": "jpeg",
                  "mimeType": "image/jpeg",
                  "offset": 3,
                  "length": 2,
               },
            ],
         })
      );
      assert_eq!(payload, &[1, 2, 3, 4, 5]);
   }

   #[test]
   fn duplicate_thumbnail_timestamps_share_the_same_payload_bytes() {
      let envelope = encode_thumbnail_envelope(&test_frames(), &[0, 1, 0], usize::MAX)
         .expect("envelope should encode");
      let (header, payload) = envelope_parts(&envelope);

      let entries = header["entries"]
         .as_array()
         .expect("header should carry an entries array");
      assert_eq!(entries.len(), 3);
      assert_eq!(entries[0]["offset"], entries[2]["offset"]);
      assert_eq!(entries[0]["length"], entries[2]["length"]);
      assert_eq!(entries[1]["offset"], serde_json::json!(3));
      assert_eq!(payload, &[1, 2, 3, 4, 5]);
   }

   #[test]
   fn thumbnail_envelope_rejects_an_unreferenced_non_jpeg_frame() {
      let mut frames = test_frames();
      frames[1].format = PixelFormat::Png;

      let error = encode_thumbnail_envelope(&frames, &[0], usize::MAX)
         .expect_err("every thumbnail payload must satisfy the published envelope");

      assert_eq!(error.to_string(), "thumbnail must be JPEG, got png");
   }

   #[test]
   fn thumbnail_envelope_rejects_payloads_beyond_the_output_cap() {
      let result = encode_thumbnail_envelope(&test_frames(), &[0, 1], ENVELOPE_PREFIX_BYTES);

      assert!(result.is_err());
   }

   #[test]
   fn thumbnail_envelope_counts_header_bytes_toward_the_output_cap() {
      let payload_len = test_frames().iter().map(|frame| frame.data.len()).sum();

      let result = encode_thumbnail_envelope(&test_frames(), &[0, 1], payload_len);

      assert!(result.is_err());
   }

   #[test]
   fn thumbnail_envelope_accepts_its_exact_total_size() {
      let uncapped = encode_thumbnail_envelope(&test_frames(), &[0, 1], usize::MAX)
         .expect("test envelope should encode");

      let capped = encode_thumbnail_envelope(&test_frames(), &[0, 1], uncapped.len())
         .expect("the exact complete-envelope limit should be accepted");

      assert_eq!(capped, uncapped);
   }

   #[test]
   fn subtitle_encodes_nested_v1_metadata_and_omits_absent_language() {
      let tracks = [subtitle_track(
         None,
         vec![subtitle_cue(u32::MAX, 250, 1_500, "hello")],
      )];

      let envelope = encode_subtitle_envelope(&tracks, MAX_SUBTITLE_OUTPUT_BYTES)
         .expect("subtitle envelope should encode");
      let (header, payload) = envelope_parts(&envelope);

      assert_eq!(
         header,
         serde_json::json!({
            "version": 1,
            "entries": [{
               "id": u32::MAX,
               "codec": "wvtt",
               "timescale": u32::MAX,
               "duration": JS_MAX_SAFE_INTEGER,
               "cues": [{
                  "cueId": u32::MAX,
                  "startSec": 0.25,
                  "endSec": 1.5,
                  "offset": 0,
                  "length": 5,
               }],
            }],
         })
      );
      assert!(header["entries"][0].get("language").is_none());
      assert_eq!(payload, b"hello");
   }

   #[test]
   fn empty_subtitle_envelope_matches_the_shared_projected_base() {
      let envelope = encode_subtitle_envelope(&[], SUBTITLE_ENVELOPE_PROJECTED_BASE_BYTES)
         .expect("the exact empty-envelope base should encode");

      assert_eq!(envelope.len(), SUBTITLE_ENVELOPE_PROJECTED_BASE_BYTES);
      let (header, payload) = envelope_parts(&envelope);
      assert_eq!(header, serde_json::json!({ "version": 1, "entries": [] }));
      assert!(payload.is_empty());
      assert!(encode_subtitle_envelope(&[], SUBTITLE_ENVELOPE_PROJECTED_BASE_BYTES - 1).is_err());
   }

   #[test]
   fn subtitle_offsets_are_stable_and_duplicate_text_shares_payload() {
      let tracks = [
         subtitle_track(
            Some("eng"),
            vec![
               subtitle_cue(1, 0, 1_000, "same"),
               subtitle_cue(2, 1_000, 2_000, "other"),
            ],
         ),
         subtitle_track(Some("spa"), vec![subtitle_cue(3, 2_000, 3_000, "same")]),
      ];

      let envelope = encode_subtitle_envelope(&tracks, MAX_SUBTITLE_OUTPUT_BYTES)
         .expect("subtitle envelope should encode");
      let (header, payload) = envelope_parts(&envelope);

      assert_eq!(header["entries"][0]["language"], "eng");
      assert_eq!(header["entries"][1]["language"], "spa");
      assert_eq!(header["entries"][0]["cues"][0]["offset"], 0);
      assert_eq!(header["entries"][0]["cues"][1]["offset"], 4);
      assert_eq!(header["entries"][1]["cues"][0]["offset"], 0);
      assert_eq!(header["entries"][1]["cues"][0]["length"], 4);
      assert_eq!(payload, b"sameother");
   }

   #[test]
   fn subtitle_projection_accepts_exact_64_mib_and_rejects_one_more_byte() {
      let text_len = MAX_SUBTITLE_OUTPUT_BYTES
         .checked_sub(
            SUBTITLE_ENVELOPE_PROJECTED_BASE_BYTES
               + SUBTITLE_TRACK_PROJECTION_BYTES
               + SUBTITLE_CUE_PROJECTION_BYTES,
         )
         .expect("subtitle constants must leave room for cue text");
      let mut text = "x".repeat(text_len);
      text
         .try_reserve_exact(1)
         .expect("boundary test text should reserve its rejecting byte");
      let mut tracks = [subtitle_track(
         None,
         vec![SubtitleCue {
            cue_id: 1,
            start_time: Duration::ZERO,
            end_time: Duration::from_secs(1),
            text,
         }],
      )];

      let envelope = encode_subtitle_envelope(&tracks, MAX_SUBTITLE_OUTPUT_BYTES)
         .expect("the exact conservative 64 MiB projection should be accepted");
      assert!(envelope.len() <= MAX_SUBTITLE_OUTPUT_BYTES);
      drop(envelope);

      tracks[0].cues[0].text.push('x');
      let error = encode_subtitle_envelope(&tracks, MAX_SUBTITLE_OUTPUT_BYTES)
         .expect_err("one byte beyond the conservative 64 MiB projection must fail");
      assert!(error.to_string().contains("subtitle envelope is too large"));
   }

   #[test]
   fn subtitle_rejects_zero_or_reversed_cue_timings() {
      for (start_ms, end_ms) in [(1_000, 1_000), (2_000, 1_000)] {
         let tracks = [subtitle_track(
            None,
            vec![subtitle_cue(1, start_ms, end_ms, "invalid")],
         )];

         let error = encode_subtitle_envelope(&tracks, MAX_SUBTITLE_OUTPUT_BYTES)
            .expect_err("subtitle cue end must be after its start");
         assert!(error.to_string().contains("invalid subtitle cue timing"));
      }
   }

   #[test]
   fn subtitle_accepts_js_safe_duration_and_rejects_the_next_integer() {
      let mut tracks = [subtitle_track(
         None,
         vec![subtitle_cue(1, 0, 1_000, "safe")],
      )];
      encode_subtitle_envelope(&tracks, MAX_SUBTITLE_OUTPUT_BYTES)
         .expect("Number.MAX_SAFE_INTEGER should be accepted");

      tracks[0].base.duration = JS_MAX_SAFE_INTEGER + 1;
      let error = encode_subtitle_envelope(&tracks, MAX_SUBTITLE_OUTPUT_BYTES)
         .expect_err("an integer above Number.MAX_SAFE_INTEGER must fail");
      assert!(error.to_string().contains("JavaScript safe integer"));
   }

   #[test]
   fn subtitle_checked_envelope_arithmetic_rejects_overflow() {
      assert_eq!(
         checked_header_cap(usize::MAX, 0).expect("header cap should be representable"),
         u32::MAX as usize
      );
      assert!(checked_header_cap(ENVELOPE_PREFIX_BYTES, 1).is_err());
      assert!(checked_envelope_len(usize::MAX, 1, usize::MAX).is_err());
      assert!(checked_envelope_len(1, usize::MAX, usize::MAX).is_err());
      assert!(checked_envelope_len(0, 0, ENVELOPE_PREFIX_BYTES - 1).is_err());
   }

   struct StreamingEntries<'a> {
      visited: &'a AtomicUsize,
      total: usize,
   }

   impl Serialize for StreamingEntries<'_> {
      fn serialize<S>(&self, serializer: S) -> std::result::Result<S::Ok, S::Error>
      where
         S: serde::Serializer,
      {
         let mut sequence = serializer.serialize_seq(Some(self.total))?;
         for value in 0..self.total {
            self.visited.fetch_add(1, Ordering::Relaxed);
            sequence.serialize_element(&value)?;
         }
         sequence.end()
      }
   }

   #[test]
   fn subtitle_generic_encoder_stops_counting_at_a_tiny_cap() {
      let visited = AtomicUsize::new(0);
      let entries = vec![StreamingEntries {
         visited: &visited,
         total: 1_000_000,
      }];

      let error = encode_binary_envelope(entries, &[], 32)
         .expect_err("oversized streamed JSON must exceed the tiny cap");

      assert!(error.to_string().contains("envelope is too large"));
      assert!(
         visited.load(Ordering::Relaxed) < 100,
         "the counting writer must stop before serializing proportional JSON"
      );
   }

   #[tokio::test(flavor = "current_thread")]
   async fn shared_envelope_task_runs_off_the_async_runtime_thread() {
      let runtime_thread = std::thread::current().id();

      let worker_thread = run_envelope_task("test", || Ok(std::thread::current().id()))
         .await
         .expect("blocking envelope work should complete");

      assert_ne!(worker_thread, runtime_thread);
   }

   #[test]
   fn shared_envelope_task_preserves_the_caller_error_label() {
      let error = envelope_task_error("subtitle", "fixture failure").to_string();

      assert_eq!(error, "subtitle envelope task failed: fixture failure");
   }
}

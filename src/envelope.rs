//! Binary envelopes used to return media payloads through Tauri IPC.
//!
//! Layout:
//! - 4 little-endian bytes containing the JSON header length.
//! - A JSON header object `{ "version": <u32>, "entries": [...] }`.
//! - Concatenated binary payloads.
//!
//! Entry `offset` values are relative to the beginning of the payload region,
//! immediately after the JSON header. `version` identifies the shape of the
//! entries so decoders can reject a header they don't understand instead of
//! misreading it; bump it whenever an entry's fields change shape.

use media_parser::{CoverArt, Frame, PixelFormat};
use serde::Serialize;

use crate::Result;

/// Current envelope format version. Decoders should reject any header whose
/// `version` they don't recognize rather than guessing at its shape.
const ENVELOPE_VERSION: u32 = 1;

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
   let payloads = frames
      .iter()
      .map(|frame| frame.data.as_slice())
      .collect::<Vec<_>>();
   encode_binary_envelope(entries, &payloads, max_output_bytes)
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
   let header = EnvelopeHeader {
      version: ENVELOPE_VERSION,
      entries,
   };
   let header = serde_json::to_vec(&header)
      .map_err(|error| crate::Error::Custom(format!("could not encode envelope: {error}")))?;
   let header_len = u32::try_from(header.len())
      .map_err(|_| crate::Error::Custom("envelope header is too large".to_string()))?;
   let envelope_len = 4usize
      .checked_add(header.len())
      .and_then(|length| length.checked_add(payload_len))
      .filter(|length| *length <= max_output_bytes)
      .ok_or_else(|| crate::Error::Custom("envelope is too large".to_string()))?;
   let mut envelope = Vec::new();
   envelope
      .try_reserve_exact(envelope_len)
      .map_err(|_| crate::Error::Custom("envelope is too large".to_string()))?;
   envelope.extend_from_slice(&header_len.to_le_bytes());
   envelope.extend_from_slice(&header);
   for payload in payloads {
      envelope.extend_from_slice(payload);
   }
   Ok(envelope)
}

#[cfg(test)]
mod tests {
   use super::*;
   use media_parser::PixelFormat;
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
      let header_len = u32::from_le_bytes(envelope[..4].try_into().unwrap()) as usize;
      let header_end = 4 + header_len;
      let header = serde_json::from_slice(&envelope[4..header_end]).expect("header should be JSON");
      (header, &envelope[header_end..])
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
      let result = encode_thumbnail_envelope(&test_frames(), &[0, 1], 4);

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
}

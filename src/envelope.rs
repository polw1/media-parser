//! Binary envelopes used to return media payloads through Tauri IPC.
//!
//! Layout:
//! - 4 little-endian bytes containing the JSON header length.
//! - A JSON array of metadata entries.
//! - Concatenated binary payloads.
//!
//! Entry `offset` values are relative to the beginning of the payload region,
//! immediately after the JSON header.

use media_parser::{CoverArt, Frame};

use crate::Result;

type EnvelopeMeta = serde_json::Map<String, serde_json::Value>;

pub(crate) fn cover_envelope(cover: Option<CoverArt>) -> Result<Vec<u8>> {
   let Some(cover) = cover else {
      return encode_binary_envelope(Vec::new(), &[]);
   };
   let mut meta = EnvelopeMeta::new();
   meta.insert("format".into(), cover.format.label().into());
   meta.insert("mimeType".into(), cover.mime_type.into());
   meta.insert("offset".into(), 0.into());
   meta.insert("length".into(), cover.data.len().into());
   let payloads = [cover.data.as_slice()];
   encode_binary_envelope(vec![meta], &payloads)
}

fn thumbnail_envelope_entry(frame: &Frame, offset: usize) -> EnvelopeMeta {
   let mut meta = EnvelopeMeta::new();
   meta.insert("trackId".into(), frame.track_id.into());
   meta.insert("width".into(), frame.width.into());
   meta.insert("height".into(), frame.height.into());
   meta.insert("timestampSec".into(), frame.timestamp.as_secs_f64().into());
   meta.insert("format".into(), frame.format.label().into());
   meta.insert("mimeType".into(), frame.format.mime_type().into());
   meta.insert("offset".into(), offset.into());
   meta.insert("length".into(), frame.data.len().into());
   meta
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
   encode_binary_envelope(entries, &payloads)
}

fn encode_binary_envelope(entries: Vec<EnvelopeMeta>, payloads: &[&[u8]]) -> Result<Vec<u8>> {
   let payload_len = payloads
      .iter()
      .try_fold(0usize, |total, payload| total.checked_add(payload.len()))
      .ok_or_else(|| crate::Error::Custom("envelope payload is too large".to_string()))?;
   let header = serde_json::to_vec(&entries)
      .map_err(|error| crate::Error::Custom(format!("could not encode envelope: {error}")))?;
   let header_len = u32::try_from(header.len())
      .map_err(|_| crate::Error::Custom("envelope header is too large".to_string()))?;
   let envelope_len = 4usize
      .checked_add(header.len())
      .and_then(|length| length.checked_add(payload_len))
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
            format: PixelFormat::Png,
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
         serde_json::json!([{
            "format": "jpeg",
            "mimeType": "image/jpeg",
            "offset": 0,
            "length": 3,
         }])
      );
      assert_eq!(payload, &[1, 2, 3]);
   }

   #[test]
   fn encodes_missing_cover_as_an_empty_envelope() {
      let envelope = cover_envelope(None).expect("empty cover should encode");
      let (header, payload) = envelope_parts(&envelope);

      assert_eq!(header, serde_json::json!([]));
      assert!(payload.is_empty());
   }

   #[test]
   fn encodes_thumbnail_metadata_and_image_bytes_in_one_binary_envelope() {
      let envelope = encode_thumbnail_envelope(&test_frames(), &[0, 1], usize::MAX)
         .expect("envelope should encode");
      let (header, payload) = envelope_parts(&envelope);

      assert_eq!(
         header,
         serde_json::json!([
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
               "format": "png",
               "mimeType": "image/png",
               "offset": 3,
               "length": 2,
            },
         ])
      );
      assert_eq!(payload, &[1, 2, 3, 4, 5]);
   }

   #[test]
   fn duplicate_thumbnail_timestamps_share_the_same_payload_bytes() {
      let envelope = encode_thumbnail_envelope(&test_frames(), &[0, 1, 0], usize::MAX)
         .expect("envelope should encode");
      let (header, payload) = envelope_parts(&envelope);

      let entries = header.as_array().expect("header should be an array");
      assert_eq!(entries.len(), 3);
      assert_eq!(entries[0]["offset"], entries[2]["offset"]);
      assert_eq!(entries[0]["length"], entries[2]["length"]);
      assert_eq!(entries[1]["offset"], serde_json::json!(3));
      assert_eq!(payload, &[1, 2, 3, 4, 5]);
   }

   #[test]
   fn thumbnail_envelope_rejects_payloads_beyond_the_output_cap() {
      let result = encode_thumbnail_envelope(&test_frames(), &[0, 1], 4);

      assert!(result.is_err());
   }
}

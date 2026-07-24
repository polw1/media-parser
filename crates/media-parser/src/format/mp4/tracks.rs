//! MP4 track metadata extraction.
//!
//! Reads information from each `trak` box without touching media samples.

use super::atoms::{
   Mp4Nav, audio_params, find_and_read_moov_box, fourcc_string, iter_boxes, parse_hdlr, parse_mdhd,
   parse_stsd, parse_tkhd, read_box, stts_sample_count, visual_dimensions,
};
use crate::Result;
use crate::errors::MediaParserError;
use crate::helpers::read_u32_be;
use crate::stream::StreamReader;
use crate::types::{
   AudioTrackMeta, BaseTrackMeta, SubtitleTrackMeta, TrackType, UnknownTrackMeta, VideoTrackMeta,
};
use std::collections::HashMap;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum TrackKind {
   Video,
   Audio,
   Subtitle,
   Unknown,
}

enum SampleEntry {
   Visual {
      width: Option<u32>,
      height: Option<u32>,
   },
   Audio {
      channels: Option<u16>,
      sample_rate: Option<u32>,
   },
   None,
}

/// Reads all MP4 tracks from the `moov/trak` boxes.
pub async fn read_tracks(reader: &dyn StreamReader) -> Result<Vec<TrackType>> {
   let moov_data = find_and_read_moov_box(reader).await?;
   let moov_payload = read_box(&moov_data, 0)
      .filter(|box_read| box_read.fourcc == *b"moov")
      .map(|box_read| box_read.payload)
      .ok_or_else(|| MediaParserError::InvalidFormat("invalid moov box".to_string()))?;

   let mut tracks = Vec::new();
   let mut trak_count = 0usize;
   let mut malformed_count = 0usize;
   for (fourcc, trak) in iter_boxes(moov_payload) {
      if &fourcc != b"trak" {
         continue;
      }
      trak_count += 1;

      // Best-effort: skip a malformed trak (missing the spec-required
      // tkhd/mdia/mdhd) instead of failing the whole file, so one bad track
      // doesn't drop every other track.
      match parse_trak(trak) {
         Ok(track) => tracks.push(track),
         Err(e) => {
            malformed_count += 1;
            tracing::warn!("skipping malformed trak: {e}");
         }
      }
   }

   if trak_count > 0 && tracks.is_empty() {
      return Err(MediaParserError::InvalidFormat(format!(
         "all trak boxes are malformed (count: {malformed_count})"
      )));
   }

   Ok(tracks)
}

fn parse_trak(trak: &[u8]) -> Result<TrackType> {
   let tkhd = trak
      .nav(&[*b"tkhd"])
      .and_then(parse_tkhd)
      .ok_or_else(|| MediaParserError::InvalidFormat("trak missing tkhd box".to_string()))?;

   let mdia = trak
      .nav(&[*b"mdia"])
      .ok_or_else(|| MediaParserError::InvalidFormat("trak missing mdia box".to_string()))?;

   let mdhd = mdia
      .nav(&[*b"mdhd"])
      .and_then(parse_mdhd)
      .ok_or_else(|| MediaParserError::InvalidFormat("trak missing mdhd box".to_string()))?;

   let handler = mdia
      .nav(&[*b"hdlr"])
      .and_then(parse_hdlr)
      .unwrap_or(*b"    ");
   let kind = classify_handler(handler);

   let stbl = mdia.nav(&[*b"minf", *b"stbl"]);
   let stsd = stbl
      .and_then(|stbl| stbl.nav(&[*b"stsd"]))
      .and_then(|stsd| {
         parse_stsd(stsd, |payload| match kind {
            TrackKind::Video => {
               let (width, height) = visual_dimensions(payload);
               SampleEntry::Visual { width, height }
            }
            TrackKind::Audio => {
               let (channels, sample_rate) = audio_params(payload);
               SampleEntry::Audio {
                  channels,
                  sample_rate,
               }
            }
            _ => SampleEntry::None,
         })
      });

   let mut properties = HashMap::new();
   properties.insert("handler_type".to_string(), fourcc_string(handler));
   properties.insert("tkhd_duration".to_string(), tkhd.duration.to_string());
   if let Some(stsd) = &stsd {
      properties.insert(
         "sample_entry_count".to_string(),
         stsd.entry_count.to_string(),
      );
   }
   add_sample_table_properties(stbl, &mut properties);

   let base = BaseTrackMeta {
      id: tkhd.id,
      codec: stsd
         .as_ref()
         .map(|s| s.codec.clone())
         .unwrap_or_else(|| "unknown".to_string()),
      language: mdhd.language.map(language_string),
      timescale: mdhd.timescale,
      duration: mdhd.duration,
      properties,
   };

   match kind {
      TrackKind::Video => {
         let (width, height) = match stsd.as_ref().map(|s| &s.entry) {
            Some(SampleEntry::Visual { width, height }) => (*width, *height),
            _ => (None, None),
         };
         Ok(TrackType::Video(VideoTrackMeta {
            base,
            width: width.unwrap_or(tkhd.width),
            height: height.unwrap_or(tkhd.height),
         }))
      }
      TrackKind::Audio => {
         let (channels, sample_rate) = match stsd.as_ref().map(|s| &s.entry) {
            Some(SampleEntry::Audio {
               channels,
               sample_rate,
            }) => (*channels, *sample_rate),
            _ => (None, None),
         };
         Ok(TrackType::Audio(AudioTrackMeta {
            base,
            channels: channels.unwrap_or(0),
            sample_rate: sample_rate.unwrap_or(0),
         }))
      }
      TrackKind::Subtitle => Ok(TrackType::Subtitle(SubtitleTrackMeta { base })),
      TrackKind::Unknown => Ok(TrackType::Unknown(UnknownTrackMeta { base })),
   }
}

fn add_sample_table_properties(stbl: Option<&[u8]>, properties: &mut HashMap<String, String>) {
   let Some(stbl) = stbl else {
      return;
   };

   if let Some(stts) = stbl.nav(&[*b"stts"]) {
      if let Some(entry_count) = read_u32_be(stts, 4) {
         properties.insert("stts_entry_count".to_string(), entry_count.to_string());
      }
      if let Some(sample_count) = stts_sample_count(stts) {
         properties.insert("sample_count".to_string(), sample_count.to_string());
      }
   }

   if let Some(stsz) = stbl.nav(&[*b"stsz"]) {
      if let Some(fixed_sample_size) = read_u32_be(stsz, 4) {
         properties.insert(
            "fixed_sample_size".to_string(),
            fixed_sample_size.to_string(),
         );
      }
      if let Some(sample_count) = read_u32_be(stsz, 8) {
         properties.insert("stsz_sample_count".to_string(), sample_count.to_string());
      }
   }
}

fn classify_handler(handler: [u8; 4]) -> TrackKind {
   match &handler {
      b"vide" => TrackKind::Video,
      b"soun" => TrackKind::Audio,
      b"sbtl" | b"subt" | b"text" | b"clcp" => TrackKind::Subtitle,
      _ => TrackKind::Unknown,
   }
}

fn language_string(language: [u8; 3]) -> String {
   String::from_utf8_lossy(&language).into_owned()
}

#[cfg(test)]
mod tests {
   use super::*;
   use async_trait::async_trait;

   fn mp4_box(fourcc: &[u8; 4], payload: &[u8]) -> Vec<u8> {
      let mut b = ((8 + payload.len()) as u32).to_be_bytes().to_vec();
      b.extend_from_slice(fourcc);
      b.extend_from_slice(payload);
      b
   }

   fn extended_mp4_box(fourcc: &[u8; 4], payload: &[u8]) -> Vec<u8> {
      let mut b = 1u32.to_be_bytes().to_vec();
      b.extend_from_slice(fourcc);
      b.extend_from_slice(&((16 + payload.len()) as u64).to_be_bytes());
      b.extend_from_slice(payload);
      b
   }

   fn tkhd(id: u32) -> Vec<u8> {
      // version 0; track_id at offset 12, width/height (fixed 16.16) at 76/80.
      let mut p = vec![0u8; 84];
      p[12..16].copy_from_slice(&id.to_be_bytes());
      p
   }

   fn mdhd() -> Vec<u8> {
      // version 0; timescale at offset 12, language (0 => None) at offset 20.
      let mut p = vec![0u8; 24];
      p[12..16].copy_from_slice(&1000u32.to_be_bytes());
      p
   }

   fn hdlr(handler: &[u8; 4]) -> Vec<u8> {
      // handler_type at offset 8.
      let mut p = vec![0u8; 12];
      p[8..12].copy_from_slice(handler);
      p
   }

   /// A spec-minimal `trak` payload (tkhd + mdia{mdhd, hdlr}).
   fn trak_payload(id: u32, handler: &[u8; 4]) -> Vec<u8> {
      let mdia = [mp4_box(b"mdhd", &mdhd()), mp4_box(b"hdlr", &hdlr(handler))].concat();
      [mp4_box(b"tkhd", &tkhd(id)), mp4_box(b"mdia", &mdia)].concat()
   }

   #[test]
   fn classify_handler_maps_known_and_unknown_types() {
      assert_eq!(classify_handler(*b"vide"), TrackKind::Video);
      assert_eq!(classify_handler(*b"soun"), TrackKind::Audio);
      for h in [b"sbtl", b"subt", b"text", b"clcp"] {
         assert_eq!(classify_handler(*h), TrackKind::Subtitle);
      }
      assert_eq!(classify_handler(*b"meta"), TrackKind::Unknown);
   }

   #[test]
   fn parse_trak_dispatches_by_handler() {
      assert!(matches!(
         parse_trak(&trak_payload(1, b"vide")).unwrap(),
         TrackType::Video(_)
      ));
      assert!(matches!(
         parse_trak(&trak_payload(2, b"soun")).unwrap(),
         TrackType::Audio(_)
      ));
      assert!(matches!(
         parse_trak(&trak_payload(3, b"subt")).unwrap(),
         TrackType::Subtitle(_)
      ));
      let unknown = parse_trak(&trak_payload(4, b"meta")).unwrap();
      assert!(matches!(&unknown, TrackType::Unknown(_)));
      if let TrackType::Unknown(meta) = unknown {
         assert_eq!(meta.base.id, 4);
      }
   }

   #[test]
   fn parse_trak_errors_on_missing_required_boxes() {
      // Missing tkhd.
      let mdia = mp4_box(b"mdia", &mp4_box(b"mdhd", &mdhd()));
      assert!(parse_trak(&mdia).is_err());
      // Missing mdia.
      assert!(parse_trak(&mp4_box(b"tkhd", &tkhd(1))).is_err());
      // mdia present but missing mdhd.
      let trak = [
         mp4_box(b"tkhd", &tkhd(1)),
         mp4_box(b"mdia", &mp4_box(b"hdlr", &hdlr(b"vide"))),
      ]
      .concat();
      assert!(parse_trak(&trak).is_err());
   }

   struct BytesReader(Vec<u8>);

   #[async_trait]
   impl StreamReader for BytesReader {
      async fn read_at(&self, offset: u64, buf: &mut [u8]) -> Result<usize> {
         let start = (offset as usize).min(self.0.len());
         let n = buf.len().min(self.0.len() - start);
         buf[..n].copy_from_slice(&self.0[start..start + n]);
         Ok(n)
      }

      async fn size(&self) -> Result<u64> {
         Ok(self.0.len() as u64)
      }
   }

   #[tokio::test]
   async fn read_tracks_skips_malformed_trak_and_keeps_valid_ones() {
      let good = mp4_box(b"trak", &trak_payload(1, b"vide"));
      // Malformed: has tkhd but no mdia, so parse_trak fails.
      let bad = mp4_box(b"trak", &mp4_box(b"tkhd", &tkhd(2)));
      let moov = mp4_box(b"moov", &[good, bad].concat());

      let tracks = read_tracks(&BytesReader(moov)).await.unwrap();

      assert_eq!(tracks.len(), 1);
      assert!(matches!(tracks[0], TrackType::Video(_)));
   }

   #[tokio::test]
   async fn read_tracks_supports_extended_size_moov_header() {
      let trak = mp4_box(b"trak", &trak_payload(1, b"vide"));
      let moov = extended_mp4_box(b"moov", &trak);

      let tracks = read_tracks(&BytesReader(moov)).await.unwrap();

      assert_eq!(tracks.len(), 1);
      assert!(matches!(tracks[0], TrackType::Video(_)));
   }

   #[tokio::test]
   async fn read_tracks_errors_when_every_trak_is_malformed() {
      let bad = mp4_box(b"trak", &mp4_box(b"tkhd", &tkhd(2)));
      let moov = mp4_box(b"moov", &bad);

      let error = read_tracks(&BytesReader(moov)).await.unwrap_err();

      assert!(
         error
            .to_string()
            .contains("all trak boxes are malformed (count: 1)")
      );
   }
}

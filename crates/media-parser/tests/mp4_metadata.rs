//! Integration tests for MP4 metadata and cover extraction.

mod common;

use common::fixtures_dir;
use media_parser::{FileStreamReader, MediaParser, PixelFormat};
use std::io::Write;

fn mp4_box(fourcc: &[u8; 4], payload: &[u8]) -> Vec<u8> {
   let size = 8 + payload.len();
   let mut data = Vec::with_capacity(size);
   data.extend_from_slice(&(size as u32).to_be_bytes());
   data.extend_from_slice(fourcc);
   data.extend_from_slice(payload);
   data
}

#[tokio::test]
async fn test_mp4_metadata_extraction() {
   let path = fixtures_dir().join("sample_metadata.mp4");
   let reader = FileStreamReader::new(&path).expect("Failed to open MP4 fixture");
   let parser = MediaParser::new(reader);

   let metadata = parser
      .metadata()
      .await
      .expect("Failed to parse MP4 metadata");

   assert_eq!(metadata.format, "MP4/M4A/MOV");
   assert_eq!(metadata.get("title"), Some("Tiny MP4 Title"));
   assert_eq!(metadata.get("artist"), Some("Tiny MP4 Artist"));
   assert_eq!(metadata.get("album"), Some("Tiny MP4 Album"));
}

#[tokio::test]
async fn test_mp4_covr_cover_extraction() {
   let image = [0xff, 0xd8, 0xff, 0xe0, 1, 2, 3, 0xff, 0xd9];
   let mut data_payload = Vec::new();
   data_payload.extend_from_slice(&13u32.to_be_bytes());
   data_payload.extend_from_slice(&0u32.to_be_bytes());
   data_payload.extend_from_slice(&image);

   let data = mp4_box(b"data", &data_payload);
   let covr = mp4_box(b"covr", &data);
   let ilst = mp4_box(b"ilst", &covr);
   let mut meta_payload = vec![0, 0, 0, 0];
   meta_payload.extend_from_slice(&ilst);
   let meta = mp4_box(b"meta", &meta_payload);
   let udta = mp4_box(b"udta", &meta);
   let moov = mp4_box(b"moov", &udta);
   let ftyp = mp4_box(b"ftyp", b"isom\0\0\0\0isom");

   let mut file = tempfile::NamedTempFile::new().expect("create temp mp4");
   file.write_all(&ftyp).expect("write ftyp");
   file.write_all(&moov).expect("write moov");
   file.flush().expect("flush temp mp4");

   let reader = FileStreamReader::new(file.path()).expect("open temp mp4");
   let parser = MediaParser::new(reader);
   let cover = parser
      .cover()
      .await
      .expect("parse cover")
      .expect("cover should exist");

   assert_eq!(cover.format, PixelFormat::Jpeg);
   assert_eq!(cover.mime_type, "image/jpeg");
   assert_eq!(cover.data, image);
}

#[tokio::test]
async fn test_mp4_duration() {
   let path = fixtures_dir().join("sample_metadata.mp4");
   let reader = FileStreamReader::new(&path).expect("Failed to open MP4 fixture");
   let parser = MediaParser::new(reader);

   let metadata = parser
      .metadata()
      .await
      .expect("Failed to parse MP4 metadata");

   let duration_seconds = metadata.duration as f64 / metadata.timescale as f64;
   assert_eq!(metadata.timescale, 1000);
   assert_eq!(duration_seconds, 1.0);
}

#[tokio::test]
async fn test_mov_format_and_duration() {
   // Real QuickTime .mov fixture (generated with ffmpeg, 1s testsrc).
   let path = fixtures_dir().join("sample_metadata.mov");
   let reader = FileStreamReader::new(&path).expect("Failed to open MOV fixture");
   let parser = MediaParser::new(reader);

   let metadata = parser
      .metadata()
      .await
      .expect("Failed to parse MOV metadata");

   assert_eq!(metadata.format, "MP4/M4A/MOV");
   assert_eq!(metadata.timescale, 1000);
   assert_eq!(metadata.duration as f64 / metadata.timescale as f64, 1.0);
}

#[tokio::test]
async fn test_mov_meta_ilst_values() {
   // Tags live under `udta/meta/ilst`. The `meta` box can appear with
   // different layouts depending on the container/encoder: ISO-BMFF-style
   // metadata has a 4-byte version/flags field before its child boxes, while
   // some QuickTime-style metadata has children starting immediately. The
   // parser probes both layouts; this fixture exercises that path so MOV
   // support does not silently drop tags.
   //
   // NOTE: QuickTime `ilst` entries are keyed by integer indices into a
   // `keys` atom rather than by fourcc, so key/name resolution is a known
   // separate gap. Here we only assert that the values are recovered through
   // the meta/ilst navigation path.
   let path = fixtures_dir().join("sample_metadata.mov");
   let reader = FileStreamReader::new(&path).expect("Failed to open MOV fixture");
   let parser = MediaParser::new(reader);

   let metadata = parser
      .metadata()
      .await
      .expect("Failed to parse MOV metadata");

   let values: Vec<&str> = metadata.values.iter().map(|m| m.value.as_str()).collect();
   assert!(
      values.contains(&"Tiny MOV Title"),
      "expected title value, got {:?}",
      metadata.values
   );
   assert!(
      values.contains(&"Tiny MOV Artist"),
      "expected artist value, got {:?}",
      metadata.values
   );
   assert!(
      values.contains(&"Tiny MOV Album"),
      "expected album value, got {:?}",
      metadata.values
   );
}

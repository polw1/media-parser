//! Integration tests for MP4 metadata extraction.

use media_parser::{FileStreamReader, MediaParser, TrackType};
use std::path::PathBuf;
fn fixtures_dir() -> PathBuf {
   PathBuf::from(env!("CARGO_MANIFEST_DIR"))
      .join("tests")
      .join("fixtures")
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

#[tokio::test]
async fn test_mp4_tracks_extraction() {
   let path = fixtures_dir().join("sample_metadata.mp4");
   let reader = FileStreamReader::new(&path).expect("Failed to open MP4 fixture");
   let parser = MediaParser::new(reader);

   let tracks = parser.tracks().await.expect("Failed to parse MP4 tracks");

   assert_eq!(tracks.len(), 1);
   match &tracks[0] {
      TrackType::Audio(audio) => {
         assert_eq!(audio.base.id, 1);
         assert_eq!(audio.base.codec, "mp4a");
         assert_eq!(audio.base.timescale, 44100);
         assert_eq!(audio.base.duration, 45124);
         assert_eq!(audio.channels, 1);
         assert_eq!(audio.sample_rate, 44100);
         assert_eq!(
            audio
               .base
               .properties
               .get("handler_type")
               .map(String::as_str),
            Some("soun")
         );
         assert_eq!(
            audio
               .base
               .properties
               .get("sample_count")
               .map(String::as_str),
            Some("45")
         );
      }
      other => panic!("expected audio track, got {other:?}"),
   }
}

#[tokio::test]
async fn test_multitrack_video_extraction() {
   // Exercises trak iteration and the visual/audio stsd layouts.
   let path = fixtures_dir().join("multitrack_video.mp4");
   let reader = FileStreamReader::new(&path).expect("Failed to open multitrack MP4 fixture");
   let parser = MediaParser::new(reader);

   let tracks = parser
      .tracks()
      .await
      .expect("Failed to parse multitrack MP4");

   assert_eq!(tracks.len(), 2);

   let video = tracks
      .iter()
      .find_map(|track| match track {
         TrackType::Video(video) => Some(video),
         _ => None,
      })
      .expect("expected a video track");
   assert_eq!(video.base.codec, "avc1");
   assert_eq!(video.width, 160);
   assert_eq!(video.height, 90);

   let audio = tracks
      .iter()
      .find_map(|track| match track {
         TrackType::Audio(audio) => Some(audio),
         _ => None,
      })
      .expect("expected an audio track");
   assert_eq!(audio.base.codec, "mp4a");
   assert_eq!(audio.channels, 2);
   assert_eq!(audio.sample_rate, 48_000);
}

#[tokio::test]
async fn test_tkhd_v1_video_extraction() {
   // MP4 with a 64-bit (v1) tkhd. Only `id`/`tkhd_duration` prove the
   // v1 offsets; width/height come from stsd here, not tkhd.
   let path = fixtures_dir().join("tkhd_v1_video.mp4");
   let reader = FileStreamReader::new(&path).expect("Failed to open tkhd v1 MP4 fixture");
   let parser = MediaParser::new(reader);

   let tracks = parser.tracks().await.expect("Failed to parse tkhd v1 MP4");

   assert_eq!(tracks.len(), 1);
   let TrackType::Video(video) = &tracks[0] else {
      panic!("expected a video track");
   };
   assert_eq!(video.base.id, 1);
   assert_eq!(video.base.codec, "avc1");
   assert_eq!(video.width, 160);
   assert_eq!(video.height, 90);

   let tkhd_duration: u64 = video
      .base
      .properties
      .get("tkhd_duration")
      .expect("tkhd_duration property should be present")
      .parse()
      .expect("tkhd_duration should be a valid u64");
   assert_eq!(tkhd_duration, 2_576_980_377);
}

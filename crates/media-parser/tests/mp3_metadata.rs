//! Integration tests for MP3 metadata extraction.

use media_parser::format::mp3::frame::FRAME_HEADER_SIZE;
use media_parser::format::mp3::{
   DurationMethod, FrameParseResult, MAX_SYNC_SEARCH, VbrHeaderType, calculate_duration,
   find_first_frame, parse_vbr_header,
};
use media_parser::{FileStreamReader, MediaParser, TrackType};
use std::path::PathBuf;
fn fixtures_dir() -> PathBuf {
   PathBuf::from(env!("CARGO_MANIFEST_DIR"))
      .join("tests")
      .join("fixtures")
}

#[tokio::test]
async fn test_id3v2_metadata_extraction() {
   let path = fixtures_dir().join("id3v2_tags.mp3");
   let reader = FileStreamReader::new(&path).expect("Failed to open ID3 fixture");
   let parser = MediaParser::new(reader);

   let metadata = parser.metadata().await.unwrap();

   assert_eq!(metadata.format, "MP3");
   assert_eq!(metadata.get("title"), Some("Test Title"));
   assert_eq!(metadata.get("artist"), Some("Test Artist"));
   assert_eq!(metadata.get("album"), Some("Test Album"));
}

#[tokio::test]
async fn test_cbr_duration() {
   let path = fixtures_dir().join("cbr_128kbps.mp3");
   let reader = FileStreamReader::new(&path).expect("Failed to open CBR fixture");
   let parser = MediaParser::new(reader);

   let metadata = parser.metadata().await.unwrap();

   assert_eq!(metadata.format, "MP3");
   assert_eq!(metadata.timescale, 1000);

   let duration_seconds = metadata.duration as f64 / metadata.timescale as f64;
   assert!(
      (0.9..=1.1).contains(&duration_seconds),
      "Expected ~1.0 seconds, got {}",
      duration_seconds
   );
}

#[tokio::test]
async fn test_vbr_xing_duration() {
   let path = fixtures_dir().join("vbr_xing.mp3");
   let reader = FileStreamReader::new(&path).expect("Failed to open VBR fixture");
   let parser = MediaParser::new(reader);

   let metadata = parser.metadata().await.unwrap();

   assert_eq!(metadata.format, "MP3");
   assert_eq!(metadata.timescale, 1000);
   let duration_seconds = metadata.duration as f64 / metadata.timescale as f64;
   assert_eq!(duration_seconds, 2.0);
}

// The functions below (`find_first_frame`, `parse_vbr_header`,
// `calculate_duration`) can be imported directly when you need more than what
// `MediaParser::metadata()` gives you — for example, the raw bitrate /
// sample-rate of the first frame, whether a Xing or VBRI header was found,
// or which duration strategy (VBR vs CBR) was actually used.

#[tokio::test]
async fn test_cbr_stereo_44100_192() {
   let path = fixtures_dir().join("stereo_cbr_192k.mp3");
   let reader = FileStreamReader::new(&path).expect("open fixture");

   let (header, offset) = match find_first_frame(&reader, 0, MAX_SYNC_SEARCH).await {
      FrameParseResult::Found { header, offset } => (header, offset),
      other => panic!("Expected Found, got {other:?}"),
   };

   // No ID3 → first frame at byte 0
   assert_eq!(offset, 0);
   // MPEG1 Layer3, stereo, 192 kbps, 44100 Hz
   assert_eq!(header.bitrate_kbps, 192);
   assert_eq!(header.sample_rate_hz, 44100);
   assert_ne!(header.channel_mode, 3, "expected stereo");
   assert_eq!(header.samples_per_frame(), 1152);
   assert_eq!(header.size().unwrap(), 626);
   assert_eq!(header.xing_offset() + FRAME_HEADER_SIZE, 36);

   // CBR → has Info tag (not Xing)
   let vbr = parse_vbr_header(&reader, offset, &header)
      .await
      .expect("CBR file should have an Info header");
   assert_eq!(vbr.header_type, VbrHeaderType::Info);
   assert_eq!(vbr.total_frames, 193);
   assert_eq!(vbr.total_bytes.unwrap(), 121625);

   let dur = calculate_duration(&reader, 0).await.unwrap();
   assert_eq!(dur.method, DurationMethod::VbrHeader);
   assert_eq!(dur.millis, 5000);
   assert_eq!(dur.seconds(), 5.0);
}

#[tokio::test]
async fn test_cbr_stereo_32000_64() {
   let path = fixtures_dir().join("stereo_cbr_32khz_64k.mp3");
   let reader = FileStreamReader::new(&path).expect("open fixture");

   let (header, offset) = match find_first_frame(&reader, 0, MAX_SYNC_SEARCH).await {
      FrameParseResult::Found { header, offset } => (header, offset),
      other => panic!("Expected Found, got {other:?}"),
   };

   assert_eq!(offset, 0);
   // MPEG1 Layer3, stereo, 64 kbps, 32000 Hz
   assert_eq!(header.bitrate_kbps, 64);
   assert_eq!(header.sample_rate_hz, 32000);
   assert_ne!(header.channel_mode, 3, "expected stereo");
   assert_eq!(header.samples_per_frame(), 1152);
   assert_eq!(header.size().unwrap(), 288);
   assert_eq!(header.xing_offset() + FRAME_HEADER_SIZE, 36);

   let vbr = parse_vbr_header(&reader, offset, &header)
      .await
      .expect("CBR file should have an Info header");
   assert_eq!(vbr.header_type, VbrHeaderType::Info);
   assert_eq!(vbr.total_frames, 140);
   assert_eq!(vbr.total_bytes.unwrap(), 40608);

   let dur = calculate_duration(&reader, 0).await.unwrap();
   assert_eq!(dur.method, DurationMethod::VbrHeader);
   assert_eq!(dur.millis, 5000);
   assert_eq!(dur.seconds(), 5.0);
}

#[tokio::test]
async fn test_vbr_stereo_44100() {
   let path = fixtures_dir().join("stereo_vbr_128k.mp3");
   let reader = FileStreamReader::new(&path).expect("open fixture");

   let (header, offset) = match find_first_frame(&reader, 0, MAX_SYNC_SEARCH).await {
      FrameParseResult::Found { header, offset } => (header, offset),
      other => panic!("Expected Found, got {other:?}"),
   };

   assert_eq!(offset, 0);
   // MPEG1 Layer3, stereo, first frame 64 kbps (VBR — first frame is the Xing frame), 44100 Hz
   assert_eq!(header.bitrate_kbps, 64);
   assert_eq!(header.sample_rate_hz, 44100);
   assert_ne!(header.channel_mode, 3, "expected stereo");
   assert_eq!(header.samples_per_frame(), 1152);
   assert_eq!(header.size().unwrap(), 208);
   assert_eq!(header.xing_offset() + FRAME_HEADER_SIZE, 36);

   // VBR → has Xing tag
   let vbr = parse_vbr_header(&reader, offset, &header)
      .await
      .expect("VBR file should have a Xing header");
   assert_eq!(vbr.header_type, VbrHeaderType::Xing);
   assert_eq!(vbr.total_frames, 193);
   assert_eq!(vbr.total_bytes.unwrap(), 21037);

   let dur = calculate_duration(&reader, 0).await.unwrap();
   assert_eq!(dur.method, DurationMethod::VbrHeader);
   assert_eq!(dur.millis, 5000);
   assert_eq!(dur.seconds(), 5.0);
}

#[tokio::test]
async fn test_vbr_mono_22050() {
   let path = fixtures_dir().join("mono_vbr_22khz.mp3");
   let reader = FileStreamReader::new(&path).expect("open fixture");

   let (header, offset) = match find_first_frame(&reader, 0, MAX_SYNC_SEARCH).await {
      FrameParseResult::Found { header, offset } => (header, offset),
      other => panic!("Expected Found, got {other:?}"),
   };

   assert_eq!(offset, 0);
   // MPEG2 Layer3, mono, first frame 56 kbps, 22050 Hz
   assert_eq!(header.bitrate_kbps, 56);
   assert_eq!(header.sample_rate_hz, 22050);
   assert_eq!(header.channel_mode, 3, "expected mono");
   assert_eq!(header.samples_per_frame(), 576);
   assert_eq!(header.size().unwrap(), 182);
   assert_eq!(header.xing_offset() + FRAME_HEADER_SIZE, 13);

   // VBR → has Xing tag
   let vbr = parse_vbr_header(&reader, offset, &header)
      .await
      .expect("VBR file should have a Xing header");
   assert_eq!(vbr.header_type, VbrHeaderType::Xing);
   assert_eq!(vbr.total_frames, 194);
   assert_eq!(vbr.total_bytes.unwrap(), 5747);

   let dur = calculate_duration(&reader, 0).await.unwrap();
   assert_eq!(dur.method, DurationMethod::VbrHeader);
   assert_eq!(dur.millis, 5000);
   assert_eq!(dur.seconds(), 5.0);
}

#[tokio::test]
async fn test_mp3_tracks_report_channels() {
   for (fixture, expected_channels, expected_sample_rate) in [
      ("stereo_cbr_192k.mp3", 2, 44_100),
      ("mono_vbr_22khz.mp3", 1, 22_050),
   ] {
      let reader = FileStreamReader::new(fixtures_dir().join(fixture)).expect("open fixture");
      let tracks = MediaParser::new(reader).tracks().await.unwrap();

      let [TrackType::Audio(track)] = tracks.as_slice() else {
         panic!("expected one audio track for {fixture}");
      };
      assert_eq!(track.base.codec, "mp3");
      assert_eq!(track.channels, expected_channels);
      assert_eq!(track.sample_rate, expected_sample_rate);
   }
}

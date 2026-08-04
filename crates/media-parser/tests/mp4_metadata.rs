//! Integration tests for MP4 metadata extraction.

use media_parser::{
   FileStreamReader, MediaParser, PixelFormat, StreamReader, TrackType,
   format::mp4::{ThumbnailIndex, read_frames},
};
use std::io::Write;
use std::path::PathBuf;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

fn mp4_box(fourcc: &[u8; 4], payload: &[u8]) -> Vec<u8> {
   let size = 8 + payload.len();
   let mut data = Vec::with_capacity(size);
   data.extend_from_slice(&(size as u32).to_be_bytes());
   data.extend_from_slice(fourcc);
   data.extend_from_slice(payload);
   data
}

fn fixtures_dir() -> PathBuf {
   PathBuf::from(env!("CARGO_MANIFEST_DIR"))
      .join("tests")
      .join("fixtures")
}

/// FNV-1a digest used to pin the exact decoded JPEG bytes, fixing the
/// presentation-order contract against regressions. The pinned values depend
/// on the OpenH264 and jpeg-encoder versions and must be updated when those
/// dependencies change output bytes.
fn fnv1a(data: &[u8]) -> u64 {
   let mut hash = 0xcbf2_9ce4_8422_2325u64;
   for byte in data {
      hash ^= u64::from(*byte);
      hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
   }
   hash
}

struct CountingReader {
   inner: FileStreamReader,
   reads: AtomicUsize,
   bytes: AtomicUsize,
}

impl CountingReader {
   fn new(path: &std::path::Path) -> Self {
      Self {
         inner: FileStreamReader::new(path).expect("open counted file"),
         reads: AtomicUsize::new(0),
         bytes: AtomicUsize::new(0),
      }
   }

   fn reset(&self) {
      self.reads.store(0, Ordering::Relaxed);
      self.bytes.store(0, Ordering::Relaxed);
   }

   fn read_count(&self) -> usize {
      self.reads.load(Ordering::Relaxed)
   }

   fn read_bytes(&self) -> usize {
      self.bytes.load(Ordering::Relaxed)
   }
}

#[async_trait::async_trait]
impl StreamReader for CountingReader {
   async fn read_at(&self, offset: u64, buf: &mut [u8]) -> media_parser::Result<usize> {
      self.reads.fetch_add(1, Ordering::Relaxed);
      let read = self.inner.read_at(offset, buf).await?;
      self.bytes.fetch_add(read, Ordering::Relaxed);
      Ok(read)
   }

   async fn size(&self) -> media_parser::Result<u64> {
      self.inner.size().await
   }
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
async fn test_mp4_h264_thumbnail_extraction() {
   let path = fixtures_dir().join("multitrack_video.mp4");
   let reader = FileStreamReader::new(&path).expect("open MP4 fixture");

   let frames = read_frames(&reader, 0, &[Duration::ZERO])
      .await
      .expect("extract thumbnail");

   assert_eq!(frames.len(), 1);
   assert_eq!(frames[0].format, PixelFormat::Jpeg);
   assert!(frames[0].width > 0);
   assert!(frames[0].height > 0);
   assert!(frames[0].data.starts_with(&[0xff, 0xd8]));
   assert!(frames[0].data.ends_with(&[0xff, 0xd9]));
}

#[tokio::test]
async fn test_mp4_h264_thumbnails_follow_presentation_order() {
   let path = fixtures_dir().join("multitrack_video.mp4");
   let reader = FileStreamReader::new(&path).expect("open MP4 fixture");
   let timestamps = [
      Duration::ZERO,
      Duration::from_millis(100),
      Duration::from_millis(200),
   ];

   let frames = read_frames(&reader, 0, &timestamps)
      .await
      .expect("extract presentation-ordered thumbnails");

   assert_eq!(frames.len(), timestamps.len());
   assert_eq!(
      frames
         .iter()
         .map(|frame| frame.timestamp)
         .collect::<Vec<_>>(),
      timestamps
   );
   // The fixture holds an I/B/P GOP with real ctts reordering; pin the exact
   // decoded bytes so a frame swap cannot slip through silently.
   assert_eq!(
      frames
         .iter()
         .map(|frame| fnv1a(&frame.data))
         .collect::<Vec<_>>(),
      [
         0x60cf_af45_c4bb_b19d,
         0xfaaf_98fb_5875_7231,
         0x53af_95e8_f945_80f4
      ]
   );
}

#[tokio::test]
async fn test_mp4_h264_thumbnails_follow_presentation_order_with_deep_b_frames() {
   let path = fixtures_dir().join("bframes_video.mp4");
   let reader = FileStreamReader::new(&path).expect("open MP4 fixture");
   // 9 frames at 100 ms with three consecutive B-frames between P-frames.
   let timestamps = (0..9)
      .map(|index| Duration::from_millis(index * 100))
      .collect::<Vec<_>>();

   let frames = read_frames(&reader, 0, &timestamps)
      .await
      .expect("extract reordered thumbnails");

   assert_eq!(frames.len(), timestamps.len());
   assert_eq!(
      frames
         .iter()
         .map(|frame| frame.timestamp)
         .collect::<Vec<_>>(),
      timestamps
   );
   assert_eq!(
      frames
         .iter()
         .map(|frame| fnv1a(&frame.data))
         .collect::<Vec<_>>(),
      [
         0x9190_4ea4_16ce_2814,
         0xbf83_13c4_8b71_3e3b,
         0xa0bd_15ff_1423_543d,
         0xea1d_a3e4_e707_f08c,
         0xc978_b83f_79b9_b1d7,
         0xa9a3_7b8d_fc5e_a227,
         0xe472_d8a3_5b90_a1cb,
         0x299e_1536_6076_e3ff,
         0x112a_8c63_5120_2a12,
      ]
   );
}

#[tokio::test]
async fn test_mp4_thumbnail_index_can_be_reused_with_another_reader() {
   let path = fixtures_dir().join("multitrack_video.mp4");
   let reader = FileStreamReader::new(&path).expect("open MP4 fixture");
   let index = ThumbnailIndex::read(&reader, 0)
      .await
      .expect("build thumbnail index");
   drop(reader);

   let next_reader = FileStreamReader::new(&path).expect("reopen MP4 fixture");
   let frames = index
      .frames(&next_reader, &[Duration::from_millis(100)])
      .await
      .expect("extract frame with cached index");

   assert_eq!(frames.len(), 1);
   assert_eq!(frames[0].timestamp, Duration::from_millis(100));
}

#[tokio::test]
async fn test_mp4_fast_thumbnail_reports_the_keyframe_pts() {
   let path = fixtures_dir().join("multitrack_video.mp4");
   let reader = FileStreamReader::new(&path).expect("open MP4 fixture");
   let index = ThumbnailIndex::read(&reader, 0)
      .await
      .expect("build thumbnail index");

   let frames = index
      .keyframes(&reader, &[Duration::from_millis(200)])
      .await
      .expect("extract keyframe");

   assert_eq!(frames.len(), 1);
   assert_eq!(frames[0].timestamp, Duration::ZERO);
}

#[tokio::test]
async fn test_mp4_fast_thumbnails_read_each_keyframe_once() {
   let path = fixtures_dir().join("multitrack_video.mp4");
   let reader = CountingReader::new(&path);
   let index = ThumbnailIndex::read(&reader, 0)
      .await
      .expect("build thumbnail index");
   reader.reset();

   let frames = index
      .keyframes(
         &reader,
         &[
            Duration::ZERO,
            Duration::from_millis(100),
            Duration::from_millis(200),
         ],
      )
      .await
      .expect("extract keyframes");

   assert_eq!(frames.len(), 3);
   assert_eq!(reader.read_count(), 1);
}

#[tokio::test]
async fn test_mp4_exact_thumbnails_read_a_shared_gop_once() {
   let path = fixtures_dir().join("multitrack_video.mp4");
   let reader = CountingReader::new(&path);
   let index = ThumbnailIndex::read(&reader, 0)
      .await
      .expect("build thumbnail index");
   reader.reset();

   let frames = index
      .frames(
         &reader,
         &[
            Duration::ZERO,
            Duration::from_millis(100),
            Duration::from_millis(200),
         ],
      )
      .await
      .expect("extract exact frames");

   assert_eq!(frames.len(), 3);
   assert_eq!(reader.read_count(), 1);
}

#[tokio::test]
async fn test_mp4_exact_thumbnails_truncate_the_gop_at_the_last_target() {
   let path = fixtures_dir().join("bframes_video.mp4");
   let reader = CountingReader::new(&path);
   let index = ThumbnailIndex::read(&reader, 0)
      .await
      .expect("build thumbnail index");

   // The fixture is a single 9-frame GOP with deep B-frame reordering.
   // Asking only for the first two presentation timestamps must not decode
   // (or read) the whole GOP.
   reader.reset();
   let partial = index
      .frames(&reader, &[Duration::ZERO, Duration::from_millis(100)])
      .await
      .expect("extract early frames");
   let partial_bytes = reader.read_bytes();

   reader.reset();
   let full_timestamps = (0..9)
      .map(|index| Duration::from_millis(index * 100))
      .collect::<Vec<_>>();
   let full = index
      .frames(&reader, &full_timestamps)
      .await
      .expect("extract all frames");
   let full_bytes = reader.read_bytes();

   // Truncation must not change the decoded bytes: these are the first two
   // hashes pinned by the deep-B-frame presentation-order test above.
   assert_eq!(
      partial
         .iter()
         .map(|frame| fnv1a(&frame.data))
         .collect::<Vec<_>>(),
      [0x9190_4ea4_16ce_2814, 0xbf83_13c4_8b71_3e3b]
   );
   assert_eq!(full.len(), 9);
   assert!(
      partial_bytes < full_bytes,
      "expected the truncated GOP to read fewer bytes: {partial_bytes} vs {full_bytes}"
   );
}

#[tokio::test]
async fn test_mp4_thumbnail_batch_rejects_too_many_outputs() {
   let path = fixtures_dir().join("multitrack_video.mp4");
   let reader = FileStreamReader::new(&path).expect("open MP4 fixture");
   let index = ThumbnailIndex::read(&reader, 0)
      .await
      .expect("build thumbnail index");
   let timestamps = vec![Duration::ZERO; 4_097];

   let error = index
      .keyframes(&reader, &timestamps)
      .await
      .expect_err("an unbounded output batch must be rejected");

   assert!(matches!(
      error,
      media_parser::MediaParserError::InvalidFormat(_)
   ));
}

#[tokio::test]
async fn test_mp4_frames_rejects_any_timestamp_outside_track_duration() {
   let path = fixtures_dir().join("multitrack_video.mp4");
   let reader = FileStreamReader::new(&path).expect("open MP4 fixture");

   let error = read_frames(&reader, 0, &[Duration::ZERO, Duration::from_secs(10)])
      .await
      .expect_err("mixed valid and invalid timestamps must not change cardinality");

   assert!(matches!(
      error,
      media_parser::MediaParserError::InvalidFormat(_)
   ));
}

#[tokio::test]
async fn test_mp4_thumbnail_rejects_non_h264_video() {
   let mut tkhd = vec![0; 84];
   tkhd[12..16].copy_from_slice(&1u32.to_be_bytes());
   let mut mdhd = vec![0; 24];
   mdhd[12..16].copy_from_slice(&1_000u32.to_be_bytes());
   mdhd[16..20].copy_from_slice(&1_000u32.to_be_bytes());
   let mut hdlr = vec![0; 12];
   hdlr[8..12].copy_from_slice(b"vide");

   let mut stsd = vec![0; 8];
   stsd[4..8].copy_from_slice(&1u32.to_be_bytes());
   stsd.extend(mp4_box(b"mp4v", &[0; 78]));
   let mut stts = vec![0; 8];
   stts[4..8].copy_from_slice(&1u32.to_be_bytes());
   stts.extend_from_slice(&1u32.to_be_bytes());
   stts.extend_from_slice(&1_000u32.to_be_bytes());
   let mut stsz = vec![0; 12];
   stsz[4..8].copy_from_slice(&4u32.to_be_bytes());
   stsz[8..12].copy_from_slice(&1u32.to_be_bytes());
   let mut stsc = vec![0; 8];
   stsc[4..8].copy_from_slice(&1u32.to_be_bytes());
   stsc.extend_from_slice(&1u32.to_be_bytes());
   stsc.extend_from_slice(&1u32.to_be_bytes());
   stsc.extend_from_slice(&1u32.to_be_bytes());
   let mut stco = vec![0; 8];
   stco[4..8].copy_from_slice(&1u32.to_be_bytes());
   stco.extend_from_slice(&0u32.to_be_bytes());

   let stbl = mp4_box(
      b"stbl",
      &[
         mp4_box(b"stsd", &stsd),
         mp4_box(b"stts", &stts),
         mp4_box(b"stsz", &stsz),
         mp4_box(b"stsc", &stsc),
         mp4_box(b"stco", &stco),
      ]
      .concat(),
   );
   let minf = mp4_box(b"minf", &stbl);
   let mdia = mp4_box(
      b"mdia",
      &[mp4_box(b"mdhd", &mdhd), mp4_box(b"hdlr", &hdlr), minf].concat(),
   );
   let trak = mp4_box(b"trak", &[mp4_box(b"tkhd", &tkhd), mdia].concat());
   let moov = mp4_box(b"moov", &trak);
   let ftyp = mp4_box(b"ftyp", b"isom\0\0\0\0isom");

   let mut file = tempfile::NamedTempFile::new().expect("create temp mp4");
   file.write_all(&ftyp).expect("write ftyp");
   file.write_all(&moov).expect("write moov");
   file.flush().expect("flush temp mp4");

   let reader = FileStreamReader::new(file.path()).expect("open temp mp4");
   let error = read_frames(&reader, 0, &[Duration::ZERO])
      .await
      .expect_err("non-H.264 video should not produce a thumbnail");

   assert!(matches!(
      error,
      media_parser::MediaParserError::UnsupportedCodec(_)
   ));
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

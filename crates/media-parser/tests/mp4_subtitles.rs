use async_trait::async_trait;
use media_parser::format::mp4::{SubtitleIndex, read_subtitles, read_subtitles_in_range};
use media_parser::{
   MediaParser, MediaParserError, Result, StreamReader, TrackFilter, parse_subtitles,
};
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

struct BytesReader(Vec<u8>);

#[async_trait]
impl StreamReader for BytesReader {
   async fn read_at(&self, offset: u64, buf: &mut [u8]) -> Result<usize> {
      let start = usize::try_from(offset)
         .unwrap_or(usize::MAX)
         .min(self.0.len());
      let count = buf.len().min(self.0.len() - start);
      buf[..count].copy_from_slice(&self.0[start..start + count]);
      Ok(count)
   }

   async fn size(&self) -> Result<u64> {
      Ok(self.0.len() as u64)
   }
}

struct CountingReader {
   bytes: Vec<u8>,
   reads: AtomicUsize,
   ranges: Mutex<Vec<(u64, usize)>>,
}

struct FaultingReader {
   bytes: Vec<u8>,
   fault_offset: u64,
   short_read: bool,
}

#[async_trait]
impl StreamReader for FaultingReader {
   async fn read_at(&self, offset: u64, buf: &mut [u8]) -> Result<usize> {
      if offset == self.fault_offset {
         if !self.short_read {
            return Err(MediaParserError::Io(std::io::Error::other(
               "injected sample read failure",
            )));
         }
         let shortened = buf.len().saturating_sub(1);
         let start = usize::try_from(offset).unwrap();
         buf[..shortened].copy_from_slice(&self.bytes[start..start + shortened]);
         return Ok(shortened);
      }
      let start = usize::try_from(offset)
         .unwrap_or(usize::MAX)
         .min(self.bytes.len());
      let count = buf.len().min(self.bytes.len() - start);
      buf[..count].copy_from_slice(&self.bytes[start..start + count]);
      Ok(count)
   }

   async fn size(&self) -> Result<u64> {
      Ok(self.bytes.len() as u64)
   }
}

impl CountingReader {
   fn new(bytes: Vec<u8>) -> Self {
      Self {
         bytes,
         reads: AtomicUsize::new(0),
         ranges: Mutex::new(Vec::new()),
      }
   }

   fn reads(&self) -> usize {
      self.reads.load(Ordering::Relaxed)
   }

   fn ranges(&self) -> Vec<(u64, usize)> {
      self.ranges.lock().unwrap().clone()
   }
}

#[async_trait]
impl StreamReader for CountingReader {
   async fn read_at(&self, offset: u64, buf: &mut [u8]) -> Result<usize> {
      self.reads.fetch_add(1, Ordering::Relaxed);
      self.ranges.lock().unwrap().push((offset, buf.len()));
      let start = usize::try_from(offset)
         .unwrap_or(usize::MAX)
         .min(self.bytes.len());
      let count = buf.len().min(self.bytes.len() - start);
      buf[..count].copy_from_slice(&self.bytes[start..start + count]);
      Ok(count)
   }

   async fn size(&self) -> Result<u64> {
      Ok(self.bytes.len() as u64)
   }
}

fn mp4_box(fourcc: &[u8; 4], payload: &[u8]) -> Vec<u8> {
   let size = u32::try_from(payload.len() + 8).expect("test box fits u32");
   [size.to_be_bytes().as_slice(), fourcc.as_slice(), payload].concat()
}

fn full_box(fourcc: &[u8; 4], body: &[u8]) -> Vec<u8> {
   mp4_box(fourcc, &[&[0, 0, 0, 0], body].concat())
}

fn encoded_language(language: &[u8; 3]) -> u16 {
   language.iter().fold(0u16, |value, character| {
      (value << 5) | u16::from(character - b'`')
   })
}

fn tkhd(track_id: u32, duration: u32) -> Vec<u8> {
   let mut body = vec![0u8; 80];
   body[8..12].copy_from_slice(&track_id.to_be_bytes());
   body[16..20].copy_from_slice(&duration.to_be_bytes());
   full_box(b"tkhd", &body)
}

fn mdhd(language: &[u8; 3], duration: u32) -> Vec<u8> {
   let mut body = vec![0u8; 20];
   body[8..12].copy_from_slice(&1000u32.to_be_bytes());
   body[12..16].copy_from_slice(&duration.to_be_bytes());
   body[16..18].copy_from_slice(&encoded_language(language).to_be_bytes());
   full_box(b"mdhd", &body)
}

fn hdlr() -> Vec<u8> {
   let mut body = vec![0u8; 8];
   body[4..8].copy_from_slice(b"sbtl");
   full_box(b"hdlr", &body)
}

fn stsd(codec: &[u8; 4]) -> Vec<u8> {
   stsd_entries(&[*codec])
}

fn stsd_entries(codecs: &[[u8; 4]]) -> Vec<u8> {
   let entries = codecs
      .iter()
      .map(|codec| mp4_box(codec, &[0; 8]))
      .collect::<Vec<_>>()
      .concat();
   full_box(
      b"stsd",
      &[
         u32::try_from(codecs.len())
            .unwrap()
            .to_be_bytes()
            .as_slice(),
         entries.as_slice(),
      ]
      .concat(),
   )
}

fn stts(sample_count: u32) -> Vec<u8> {
   stts_entries(&[(sample_count, 1000)])
}

fn stts_entries(entries: &[(u32, u32)]) -> Vec<u8> {
   let mut body = Vec::from(u32::try_from(entries.len()).unwrap().to_be_bytes());
   for (sample_count, sample_delta) in entries {
      body.extend_from_slice(&sample_count.to_be_bytes());
      body.extend_from_slice(&sample_delta.to_be_bytes());
   }
   full_box(b"stts", &body)
}

fn stsc(sample_count: u32) -> Vec<u8> {
   stsc_runs(&[(1, sample_count, 1)])
}

fn stsc_runs(runs: &[(u32, u32, u32)]) -> Vec<u8> {
   let mut body = Vec::from(u32::try_from(runs.len()).unwrap().to_be_bytes());
   for (first_chunk, samples_per_chunk, description_index) in runs {
      body.extend_from_slice(&first_chunk.to_be_bytes());
      body.extend_from_slice(&samples_per_chunk.to_be_bytes());
      body.extend_from_slice(&description_index.to_be_bytes());
   }
   full_box(b"stsc", &body)
}

fn stsz(samples: &[Vec<u8>]) -> Vec<u8> {
   let mut body = Vec::from(0u32.to_be_bytes());
   body.extend_from_slice(&(samples.len() as u32).to_be_bytes());
   for sample in samples {
      body.extend_from_slice(&(sample.len() as u32).to_be_bytes());
   }
   full_box(b"stsz", &body)
}

fn fixed_stsz(sample_size: u32, sample_count: u32) -> Vec<u8> {
   full_box(
      b"stsz",
      &[
         sample_size.to_be_bytes().as_slice(),
         sample_count.to_be_bytes().as_slice(),
      ]
      .concat(),
   )
}

fn stco(marker: u32) -> Vec<u8> {
   stco_markers(&[marker])
}

fn stco_markers(markers: &[u32]) -> Vec<u8> {
   let mut body = Vec::from(u32::try_from(markers.len()).unwrap().to_be_bytes());
   for marker in markers {
      body.extend_from_slice(&marker.to_be_bytes());
   }
   full_box(b"stco", &body)
}

fn co64_markers(markers: &[u64]) -> Vec<u8> {
   let mut body = Vec::from(u32::try_from(markers.len()).unwrap().to_be_bytes());
   for marker in markers {
      body.extend_from_slice(&marker.to_be_bytes());
   }
   full_box(b"co64", &body)
}

fn subtitle_track(
   track_id: u32,
   language: &[u8; 3],
   samples: &[Vec<u8>],
   offset_marker: u32,
   presentation_offset: Option<i32>,
) -> Vec<u8> {
   subtitle_track_with_tables(
      track_id,
      language,
      samples,
      stsd(b"tx3g"),
      stsc(samples.len() as u32),
      stco(offset_marker),
      presentation_offset,
   )
}

fn subtitle_track_with_tables(
   track_id: u32,
   language: &[u8; 3],
   samples: &[Vec<u8>],
   stsd: Vec<u8>,
   stsc: Vec<u8>,
   stco: Vec<u8>,
   presentation_offset: Option<i32>,
) -> Vec<u8> {
   subtitle_track_with_timing(
      track_id,
      language,
      samples,
      stsd,
      stts(samples.len() as u32),
      stsc,
      stco,
      presentation_offset,
   )
}

#[expect(
   clippy::too_many_arguments,
   reason = "reviewable synthetic MP4 builder keeps each timing table explicit"
)]
fn subtitle_track_with_timing(
   track_id: u32,
   language: &[u8; 3],
   samples: &[Vec<u8>],
   stsd: Vec<u8>,
   stts: Vec<u8>,
   stsc: Vec<u8>,
   stco: Vec<u8>,
   presentation_offset: Option<i32>,
) -> Vec<u8> {
   let duration = u32::try_from(samples.len()).unwrap() * 1000;
   subtitle_track_with_timing_and_duration(
      track_id,
      language,
      samples,
      stsd,
      stts,
      stsc,
      stco,
      presentation_offset,
      duration,
   )
}

#[expect(
   clippy::too_many_arguments,
   reason = "reviewable synthetic MP4 builder keeps timing and duration explicit"
)]
fn subtitle_track_with_timing_and_duration(
   track_id: u32,
   language: &[u8; 3],
   samples: &[Vec<u8>],
   stsd: Vec<u8>,
   stts: Vec<u8>,
   stsc: Vec<u8>,
   stco: Vec<u8>,
   presentation_offset: Option<i32>,
   duration: u32,
) -> Vec<u8> {
   let stbl = mp4_box(b"stbl", &[stsd, stts, stsc, stsz(samples), stco].concat());
   let minf = mp4_box(b"minf", &stbl);
   let mdia = mp4_box(b"mdia", &[mdhd(language, duration), hdlr(), minf].concat());
   let edit = presentation_offset.map(|media_time| {
      let entry = [
         1u32.to_be_bytes().as_slice(),
         duration.to_be_bytes().as_slice(),
         media_time.to_be_bytes().as_slice(),
         0x0001_0000u32.to_be_bytes().as_slice(),
      ]
      .concat();
      mp4_box(b"edts", &full_box(b"elst", &entry))
   });
   let mut children = vec![tkhd(track_id, duration)];
   children.extend(edit);
   children.push(mdia);
   mp4_box(b"trak", &children.concat())
}

fn fixed_size_subtitle_track(
   track_id: u32,
   language: &[u8; 3],
   samples: &[Vec<u8>],
   declared_sample_count: u32,
   offset_marker: u32,
) -> Vec<u8> {
   let sample_size = u32::try_from(samples[0].len()).expect("test sample size fits u32");
   assert!(
      samples
         .iter()
         .all(|sample| sample.len() == sample_size as usize)
   );
   let duration = u32::try_from(samples.len()).unwrap() * 1000;
   let stbl = mp4_box(
      b"stbl",
      &[
         stsd(b"tx3g"),
         stts(samples.len() as u32),
         stsc(samples.len() as u32),
         fixed_stsz(sample_size, declared_sample_count),
         stco(offset_marker),
      ]
      .concat(),
   );
   let minf = mp4_box(b"minf", &stbl);
   let mdia = mp4_box(b"mdia", &[mdhd(language, duration), hdlr(), minf].concat());
   mp4_box(b"trak", &[tkhd(track_id, duration), mdia].concat())
}

fn tx3g(text: &str) -> Vec<u8> {
   let bytes = text.as_bytes();
   [
      u16::try_from(bytes.len()).unwrap().to_be_bytes().as_slice(),
      bytes,
   ]
   .concat()
}

fn patch_marker(bytes: &mut [u8], marker: u32, replacement: u32) {
   let marker = marker.to_be_bytes();
   let position = bytes
      .windows(marker.len())
      .position(|window| window == marker)
      .expect("unique stco marker");
   bytes[position..position + 4].copy_from_slice(&replacement.to_be_bytes());
}

fn patch_marker_u64(bytes: &mut [u8], marker: u64, replacement: u64) {
   let marker = marker.to_be_bytes();
   let position = bytes
      .windows(marker.len())
      .position(|window| window == marker)
      .expect("unique co64 marker");
   bytes[position..position + 8].copy_from_slice(&replacement.to_be_bytes());
}

fn subtitle_mp4() -> Vec<u8> {
   subtitle_mp4_with_edit(None)
}

fn single_codec_subtitle_mp4(codec: &[u8; 4], sample: Vec<u8>) -> Vec<u8> {
   const OFFSET: u32 = 0xc1c2_c3c4;
   let samples = [sample];
   let ftyp = mp4_box(b"ftyp", b"isom\0\0\0\0isom");
   let moov = mp4_box(
      b"moov",
      &subtitle_track_with_tables(
         1,
         b"eng",
         &samples,
         stsd(codec),
         stsc(1),
         stco(OFFSET),
         None,
      ),
   );
   let payload = samples.concat();
   let mdat = mp4_box(b"mdat", &payload);
   let mut file = [ftyp, moov, mdat].concat();
   let payload_offset = file.len() - payload.len();
   patch_marker(&mut file, OFFSET, payload_offset as u32);
   file
}

fn subtitle_mp4_with_edit(presentation_offset: Option<i32>) -> Vec<u8> {
   const ENGLISH_OFFSET: u32 = 0xf1f2_f3f4;
   const SPANISH_OFFSET: u32 = 0xe1e2_e3e4;
   let english = [tx3g("First"), tx3g("Second")];
   let spanish = [tx3g("Hola")];
   let ftyp = mp4_box(b"ftyp", b"isom\0\0\0\0isom");
   let moov = mp4_box(
      b"moov",
      &[
         subtitle_track(1, b"eng", &english, ENGLISH_OFFSET, presentation_offset),
         subtitle_track(2, b"spa", &spanish, SPANISH_OFFSET, None),
      ]
      .concat(),
   );
   let english_payload = english.concat();
   let spanish_payload = spanish.concat();
   let mdat = mp4_box(
      b"mdat",
      &[english_payload.as_slice(), spanish_payload.as_slice()].concat(),
   );
   let mut file = [ftyp, moov, mdat].concat();
   let mdat_payload = file.len() - english_payload.len() - spanish_payload.len();
   patch_marker(&mut file, ENGLISH_OFFSET, mdat_payload as u32);
   patch_marker(
      &mut file,
      SPANISH_OFFSET,
      (mdat_payload + english_payload.len()) as u32,
   );
   file
}

fn fixed_stsz_count_mismatch_mp4() -> Vec<u8> {
   const ENGLISH_OFFSET: u32 = 0xa7a8_a9aa;
   const SPANISH_OFFSET: u32 = 0xb7b8_b9ba;
   let english = [tx3g("One"), tx3g("Two")];
   let spanish = [tx3g("Hola")];
   let ftyp = mp4_box(b"ftyp", b"isom\0\0\0\0isom");
   let moov = mp4_box(
      b"moov",
      &[
         fixed_size_subtitle_track(1, b"eng", &english, u32::MAX, ENGLISH_OFFSET),
         subtitle_track(2, b"spa", &spanish, SPANISH_OFFSET, None),
      ]
      .concat(),
   );
   let english_payload = english.concat();
   let spanish_payload = spanish.concat();
   let mdat = mp4_box(
      b"mdat",
      &[english_payload.as_slice(), spanish_payload.as_slice()].concat(),
   );
   let mut file = [ftyp, moov, mdat].concat();
   let mdat_payload = file.len() - english_payload.len() - spanish_payload.len();
   patch_marker(&mut file, ENGLISH_OFFSET, mdat_payload as u32);
   patch_marker(
      &mut file,
      SPANISH_OFFSET,
      (mdat_payload + english_payload.len()) as u32,
   );
   file
}

fn same_language_subtitle_mp4() -> Vec<u8> {
   let mut bytes = subtitle_mp4();
   let spanish_language = encoded_language(b"spa").to_be_bytes();
   let language_offset = bytes
      .windows(spanish_language.len())
      .rposition(|window| window == spanish_language)
      .expect("Spanish mdhd language");
   bytes[language_offset..language_offset + 2]
      .copy_from_slice(&encoded_language(b"eng").to_be_bytes());
   bytes
}

fn description_policy_mp4(second_codec: [u8; 4], referenced: bool) -> Vec<u8> {
   const ENGLISH_FIRST: u32 = 0xa1a2_a3a4;
   const ENGLISH_SECOND: u32 = 0xb1b2_b3b4;
   const SPANISH_OFFSET: u32 = 0xc1c2_c3c4;
   let english = [tx3g("First"), tx3g("Second")];
   let spanish = [tx3g("Hola")];
   let (runs, markers) = if referenced {
      (
         vec![(1, 1, 1), (2, 1, 2)],
         vec![ENGLISH_FIRST, ENGLISH_SECOND],
      )
   } else {
      (vec![(1, 2, 1)], vec![ENGLISH_FIRST])
   };
   let english_track = subtitle_track_with_tables(
      1,
      b"eng",
      &english,
      stsd_entries(&[*b"tx3g", second_codec]),
      stsc_runs(&runs),
      stco_markers(&markers),
      None,
   );
   let spanish_track = subtitle_track(2, b"spa", &spanish, SPANISH_OFFSET, None);
   let ftyp = mp4_box(b"ftyp", b"isom\0\0\0\0isom");
   let moov = mp4_box(b"moov", &[english_track, spanish_track].concat());
   let english_payload = english.concat();
   let spanish_payload = spanish.concat();
   let mdat = mp4_box(
      b"mdat",
      &[english_payload.as_slice(), spanish_payload.as_slice()].concat(),
   );
   let mut file = [ftyp, moov, mdat].concat();
   let payload_offset = file.len() - english_payload.len() - spanish_payload.len();
   patch_marker(&mut file, ENGLISH_FIRST, payload_offset as u32);
   if referenced {
      patch_marker(
         &mut file,
         ENGLISH_SECOND,
         (payload_offset + english[0].len()) as u32,
      );
   }
   patch_marker(
      &mut file,
      SPANISH_OFFSET,
      (payload_offset + english_payload.len()) as u32,
   );
   file
}

fn zero_delta_subtitle_mp4() -> Vec<u8> {
   const OFFSET: u32 = 0xd1d2_d3d4;
   let samples = [tx3g("First"), tx3g("Gap"), tx3g("Third")];
   let track = subtitle_track_with_timing_and_duration(
      1,
      b"eng",
      &samples,
      stsd(b"tx3g"),
      stts_entries(&[(1, 1_000), (1, 0), (1, 1_000)]),
      stsc(3),
      stco(OFFSET),
      None,
      2_000,
   );
   let ftyp = mp4_box(b"ftyp", b"isom\0\0\0\0isom");
   let moov = mp4_box(b"moov", &track);
   let payload = samples.concat();
   let mdat = mp4_box(b"mdat", &payload);
   let mut file = [ftyp, moov, mdat].concat();
   let payload_offset = file.len() - payload.len();
   patch_marker(&mut file, OFFSET, payload_offset as u32);
   file
}

/// Two chunks holding one sample each, separated by real padding wider than the
/// shipped `max_coalesce_gap_bytes`, so the pair cannot merge into one read
/// batch. That ceiling is private to the subtitle module, hence the literal.
/// Returns the file plus the absolute offset of each sample.
fn multi_chunk_subtitle_mp4() -> (Vec<u8>, u64, u64) {
   const FIRST_OFFSET: u32 = 0x9192_9394;
   const SECOND_OFFSET: u32 = 0x8182_8384;
   const CHUNK_GAP: usize = 64 * 1024 + 1;
   let samples = [tx3g("First"), tx3g("Second")];
   let ftyp = mp4_box(b"ftyp", b"isom\0\0\0\0isom");
   let moov = mp4_box(
      b"moov",
      &subtitle_track_with_tables(
         1,
         b"eng",
         &samples,
         stsd(b"tx3g"),
         stsc_runs(&[(1, 1, 1), (2, 1, 1)]),
         stco_markers(&[FIRST_OFFSET, SECOND_OFFSET]),
         None,
      ),
   );
   let padding = vec![0u8; CHUNK_GAP];
   let payload = [
      samples[0].as_slice(),
      padding.as_slice(),
      samples[1].as_slice(),
   ]
   .concat();
   let mdat = mp4_box(b"mdat", &payload);
   let mut file = [ftyp, moov, mdat].concat();
   let first_sample = file.len() - payload.len();
   let second_sample = first_sample + samples[0].len() + CHUNK_GAP;
   patch_marker(&mut file, FIRST_OFFSET, first_sample as u32);
   patch_marker(&mut file, SECOND_OFFSET, second_sample as u32);
   (file, first_sample as u64, second_sample as u64)
}

/// Two chunks addressed by a `co64` table whose small, real offsets are encoded
/// as 64-bit entries, so the second entry is only reachable through the 8-byte
/// stride and the 64-bit read.
fn co64_subtitle_mp4() -> Vec<u8> {
   const FIRST_OFFSET: u64 = 0xf1f2_f3f4_f5f6_f7f8;
   const SECOND_OFFSET: u64 = 0xe1e2_e3e4_e5e6_e7e8;
   let samples = [tx3g("First"), tx3g("Second")];
   let ftyp = mp4_box(b"ftyp", b"isom\0\0\0\0isom");
   let moov = mp4_box(
      b"moov",
      &subtitle_track_with_tables(
         1,
         b"eng",
         &samples,
         stsd(b"tx3g"),
         stsc_runs(&[(1, 1, 1), (2, 1, 1)]),
         co64_markers(&[FIRST_OFFSET, SECOND_OFFSET]),
         None,
      ),
   );
   let payload = samples.concat();
   let mdat = mp4_box(b"mdat", &payload);
   let mut file = [ftyp, moov, mdat].concat();
   let first_sample = file.len() - payload.len();
   patch_marker_u64(&mut file, FIRST_OFFSET, first_sample as u64);
   patch_marker_u64(
      &mut file,
      SECOND_OFFSET,
      (first_sample + samples[0].len()) as u64,
   );
   file
}

fn patch_first_track_id(bytes: &mut [u8], track_id: u32) {
   let tkhd_fourcc = bytes
      .windows(4)
      .position(|window| window == b"tkhd")
      .expect("first tkhd");
   let track_id_offset = tkhd_fourcc + 16;
   bytes[track_id_offset..track_id_offset + 4].copy_from_slice(&track_id.to_be_bytes());
}

fn find_bytes(bytes: &[u8], needle: &[u8]) -> usize {
   bytes
      .windows(needle.len())
      .position(|window| window == needle)
      .expect("fixture byte sequence")
}

#[tokio::test]
async fn high_level_subtitles_dispatches_synthetic_mp4() {
   let parser = MediaParser::new(BytesReader(subtitle_mp4()));

   let tracks = parser
      .subtitles(None)
      .await
      .expect("extract all high-level subtitles");

   assert_eq!(tracks.len(), 2);
   assert_eq!(tracks[0].base.id, 1);
   assert_eq!(tracks[0].cues.len(), 2);
   assert_eq!(tracks[0].cues[0].text, "First");
   assert_eq!(tracks[1].base.id, 2);
   assert_eq!(tracks[1].cues[0].text, "Hola");
}

#[tokio::test]
async fn high_level_subtitles_decodes_wvtt_vttc_payload() {
   let sample = mp4_box(b"vttc", &mp4_box(b"payl", b"WebVTT cue"));
   let parser = MediaParser::new(BytesReader(single_codec_subtitle_mp4(b"wvtt", sample)));

   let tracks = parser.subtitles(None).await.expect("extract WebVTT track");

   assert_eq!(tracks.len(), 1);
   assert_eq!(tracks[0].base.codec, "wvtt");
   assert_eq!(tracks[0].cues.len(), 1);
   assert_eq!(tracks[0].cues[0].text, "WebVTT cue");
}

#[tokio::test]
async fn high_level_subtitles_decodes_length_prefixed_quicktime_text() {
   let parser = MediaParser::new(BytesReader(single_codec_subtitle_mp4(
      b"text",
      tx3g("QuickTime cue!"),
   )));

   let tracks = parser
      .subtitles(None)
      .await
      .expect("extract QuickTime text track");

   assert_eq!(tracks.len(), 1);
   assert_eq!(tracks[0].base.codec, "text");
   assert_eq!(tracks[0].cues.len(), 1);
   assert_eq!(tracks[0].cues[0].text, "QuickTime cue!");
}

#[tokio::test]
async fn high_level_subtitles_preserves_stpp_payload() {
   let parser = MediaParser::new(BytesReader(single_codec_subtitle_mp4(
      b"stpp",
      b"<p>cue</p>".to_vec(),
   )));

   let tracks = parser.subtitles(None).await.expect("extract stpp track");

   assert_eq!(tracks.len(), 1);
   assert_eq!(tracks[0].base.codec, "stpp");
   assert_eq!(tracks[0].cues.len(), 1);
   assert_eq!(tracks[0].cues[0].text, "<p>cue</p>");
}

#[tokio::test]
async fn high_level_subtitles_in_range_dispatches_synthetic_mp4() {
   let parser = MediaParser::new(BytesReader(subtitle_mp4()));

   let tracks = parser
      .subtitles_in_range(
         Some(TrackFilter::Language("eng".to_owned())),
         (Duration::from_secs(1), Duration::from_secs(2)),
      )
      .await
      .expect("extract high-level ranged subtitles");

   assert_eq!(tracks.len(), 1);
   assert_eq!(tracks[0].base.id, 1);
   assert_eq!(tracks[0].cues.len(), 1);
   assert_eq!(tracks[0].cues[0].cue_id, 2);
   assert_eq!(tracks[0].cues[0].text, "Second");
}

#[tokio::test]
async fn high_level_subtitles_returns_empty_for_mp3_without_mp4_parsing() {
   let mut bytes = vec![0; 32];
   bytes[..3].copy_from_slice(b"ID3");
   let reader = CountingReader::new(bytes);
   let parser = MediaParser::new(&reader);

   let tracks = parser
      .subtitles(None)
      .await
      .expect("MP3 has no subtitle tracks");

   assert!(tracks.is_empty());
   assert_eq!(reader.ranges(), vec![(0, 32)]);
}

#[tokio::test]
async fn high_level_invalid_subtitle_ranges_perform_no_reads() {
   for range in [
      (Duration::from_secs(1), Duration::from_secs(1)),
      (Duration::from_secs(2), Duration::from_secs(1)),
   ] {
      let reader = CountingReader::new(subtitle_mp4());
      let parser = MediaParser::new(&reader);

      parser
         .subtitles_in_range(None, range)
         .await
         .expect_err("invalid high-level subtitle range");

      assert_eq!(reader.reads(), 0);
   }
}

#[tokio::test]
async fn high_level_registry_invalid_subtitle_ranges_perform_no_reads() {
   for range in [
      (Duration::from_secs(1), Duration::from_secs(1)),
      (Duration::from_secs(2), Duration::from_secs(1)),
   ] {
      let reader = CountingReader::new(subtitle_mp4());

      let error = parse_subtitles(&reader, None, Some(range))
         .await
         .expect_err("invalid registry subtitle range");

      assert!(matches!(error, MediaParserError::SubtitleError(_)));
      assert_eq!(reader.reads(), 0);
   }
}

#[tokio::test]
async fn high_level_subtitles_unknown_format_reads_one_header() {
   let reader = CountingReader::new(vec![0; 32]);
   let parser = MediaParser::new(&reader);

   let error = parser
      .subtitles(None)
      .await
      .expect_err("unknown format must not dispatch subtitles");

   assert!(matches!(error, MediaParserError::InvalidFormat(_)));
   assert_eq!(reader.ranges(), vec![(0, 32)]);
}

#[tokio::test]
async fn public_mp4_subtitle_apis_extract_and_reuse_index() {
   let reader = BytesReader(subtitle_mp4());
   let index = Arc::new(SubtitleIndex::read(&reader).await.expect("build index"));

   let indexed = Arc::clone(&index)
      .subtitles(&reader, None, None)
      .await
      .expect("extract indexed subtitles");
   assert_eq!(indexed.len(), 2);
   assert_eq!(indexed[0].cues[0].cue_id, 1);
   assert_eq!(indexed[0].cues[0].text, "First");

   let ranged = read_subtitles_in_range(
      &reader,
      Some(TrackFilter::Language("ENG".to_owned())),
      Some((Duration::from_millis(900), Duration::from_millis(1100))),
   )
   .await
   .expect("extract ranged English subtitles");
   assert_eq!(ranged.len(), 1);
   assert_eq!(ranged[0].cues.len(), 2);
   assert_eq!(ranged[0].cues[0].start_time, Duration::ZERO);
   assert_eq!(ranged[0].cues[1].start_time, Duration::from_secs(1));

   let filtered = read_subtitles(&reader, Some(TrackFilter::TrackId(1)))
      .await
      .expect("extract track 1");
   assert_eq!(filtered.len(), 1);
   assert_eq!(filtered[0].cues.len(), 2);
   assert_eq!(filtered[0].base.properties.len(), 3);
   assert_eq!(filtered[0].base.properties["handler_type"], "sbtl");
   assert_eq!(filtered[0].base.properties["sample_count"], "2");
   assert_eq!(filtered[0].base.properties["cue_count"], "2");
}

#[tokio::test]
async fn hardening_invalid_ranges_perform_no_reads() {
   let reader = CountingReader::new(subtitle_mp4());

   for range in [
      (Duration::from_secs(1), Duration::from_secs(1)),
      (Duration::from_secs(2), Duration::from_secs(1)),
   ] {
      read_subtitles_in_range(&reader, None, Some(range))
         .await
         .expect_err("invalid thin-wrapper range");
   }
   assert_eq!(reader.reads(), 0);

   let index = Arc::new(SubtitleIndex::read(&reader).await.expect("build index"));
   let reads_after_index = reader.reads();
   Arc::clone(&index)
      .subtitles(
         &reader,
         None,
         Some((Duration::from_secs(2), Duration::from_secs(1))),
      )
      .await
      .expect_err("invalid reusable-index range");
   assert_eq!(reader.reads(), reads_after_index);
}

#[tokio::test]
async fn hardening_trak_limit_counts_irrelevant_and_malformed_boxes() {
   let malformed = mp4_box(b"trak", &[]);
   let accepted = BytesReader(mp4_box(b"moov", &malformed.repeat(1000)));
   SubtitleIndex::read(&accepted)
      .await
      .expect("exactly 1000 trak boxes are accepted");

   let rejected = BytesReader(mp4_box(b"moov", &malformed.repeat(1001)));
   let error = SubtitleIndex::read(&rejected)
      .await
      .expect_err("1001st trak must fail before relevance checks");
   assert!(error.to_string().contains("1000"), "{error}");
}

#[tokio::test]
async fn hardening_track_local_decode_failure_never_returns_a_partial_track() {
   let mut bytes = subtitle_mp4();
   let text = find_bytes(&bytes, b"First");
   bytes[text - 2..text].copy_from_slice(&u16::MAX.to_be_bytes());
   let reader = BytesReader(bytes);
   let index = Arc::new(SubtitleIndex::read(&reader).await.expect("build index"));

   let broad = Arc::clone(&index)
      .subtitles(&reader, None, None)
      .await
      .expect("broad request skips bad track");
   assert_eq!(broad.len(), 1);
   assert_eq!(broad[0].base.id, 2);

   Arc::clone(&index)
      .subtitles(&reader, Some(TrackFilter::TrackId(1)), None)
      .await
      .expect_err("explicit bad track reports its decode error");
}

#[tokio::test]
async fn hardening_invalid_first_track_offset_is_skipped_broadly_and_by_language() {
   let mut bytes = same_language_subtitle_mp4();
   let first_stco = find_bytes(&bytes, b"stco") + 12;
   bytes[first_stco..first_stco + 4].copy_from_slice(&u32::MAX.to_be_bytes());
   let reader = BytesReader(bytes);
   let index = Arc::new(SubtitleIndex::read(&reader).await.expect("build index"));

   for filter in [None, Some(TrackFilter::Language("eng".to_owned()))] {
      let tracks = Arc::clone(&index)
         .subtitles(&reader, filter, None)
         .await
         .expect("track-local offset failure is skipped");
      assert_eq!(tracks.len(), 1);
      assert_eq!(tracks[0].base.id, 2);
   }

   let error = Arc::clone(&index)
      .subtitles(&reader, Some(TrackFilter::TrackId(1)), None)
      .await
      .expect_err("explicit malformed track reports its sample offset error");
   assert!(
      error.to_string().contains("truncated sample batch"),
      "{error}"
   );
}

#[tokio::test]
async fn hardening_short_first_track_read_is_track_local_but_reader_error_is_fatal() {
   let bytes = same_language_subtitle_mp4();
   let first_sample = u64::try_from(find_bytes(&bytes, &[0, 5, b'F', b'i'])).unwrap();
   let short = FaultingReader {
      bytes: bytes.clone(),
      fault_offset: first_sample,
      short_read: true,
   };
   let index = Arc::new(SubtitleIndex::read(&short).await.expect("build index"));

   let tracks = Arc::clone(&index)
      .subtitles(&short, Some(TrackFilter::Language("eng".to_owned())), None)
      .await
      .expect("short sample data skips only the first track");
   assert_eq!(tracks.len(), 1);
   assert_eq!(tracks[0].base.id, 2);
   Arc::clone(&index)
      .subtitles(&short, Some(TrackFilter::TrackId(1)), None)
      .await
      .expect_err("explicit truncated track errors");

   let fatal = FaultingReader {
      bytes,
      fault_offset: first_sample,
      short_read: false,
   };
   let fatal_index = SubtitleIndex::read(&fatal).await.expect("build index");
   let error = Arc::new(fatal_index)
      .subtitles(&fatal, None, None)
      .await
      .expect_err("reader I/O failure aborts a broad request");
   assert!(matches!(error, MediaParserError::Io(_)));
}

#[tokio::test]
async fn hardening_track_id_zero_is_an_ordinary_exact_filter() {
   let mut bytes = subtitle_mp4();
   patch_first_track_id(&mut bytes, 0);
   let reader = BytesReader(bytes);

   let tracks = read_subtitles(&reader, Some(TrackFilter::TrackId(0)))
      .await
      .expect("extract exact ID zero");
   assert_eq!(tracks.len(), 1);
   assert_eq!(tracks[0].base.id, 0);
   assert!(
      read_subtitles(&reader, Some(TrackFilter::TrackId(99)))
         .await
         .unwrap()
         .is_empty()
   );
}

#[tokio::test]
async fn hardening_edit_offset_clamps_crossing_cue_and_drops_zero_length_gap() {
   let reader = BytesReader(subtitle_mp4_with_edit(Some(500)));
   let tracks = read_subtitles(&reader, Some(TrackFilter::TrackId(1)))
      .await
      .expect("extract edited track");
   assert_eq!(tracks[0].cues[0].start_time, Duration::ZERO);
   assert_eq!(tracks[0].cues[0].end_time, Duration::from_millis(500));
   assert_eq!(tracks[0].cues[1].start_time, Duration::from_millis(500));

   let reader = BytesReader(subtitle_mp4_with_edit(Some(1000)));
   let tracks = read_subtitles(&reader, Some(TrackFilter::TrackId(1)))
      .await
      .expect("extract edited track with zero-length first cue");
   assert_eq!(tracks[0].cues.len(), 1);
   assert_eq!(tracks[0].cues[0].cue_id, 2);
   assert_eq!(tracks[0].cues[0].start_time, Duration::ZERO);
}

#[tokio::test]
async fn hardening_range_selection_reads_only_overlapping_samples() {
   let bytes = subtitle_mp4();
   let first_sample = u64::try_from(find_bytes(&bytes, &[0, 5, b'F', b'i'])).unwrap();
   let second_sample = u64::try_from(find_bytes(&bytes, &[0, 6, b'S', b'e'])).unwrap();
   let reader = CountingReader::new(bytes);
   let index = Arc::new(SubtitleIndex::read(&reader).await.expect("build index"));
   let reads_after_index = reader.ranges().len();

   let tracks = Arc::clone(&index)
      .subtitles(
         &reader,
         Some(TrackFilter::TrackId(1)),
         Some((Duration::from_millis(1100), Duration::from_millis(1200))),
      )
      .await
      .expect("extract narrow range");
   assert_eq!(tracks[0].cues.len(), 1);
   assert_eq!(tracks[0].cues[0].cue_id, 2);
   let sample_reads = &reader.ranges()[reads_after_index..];
   assert!(
      sample_reads
         .iter()
         .any(|(offset, _)| *offset == second_sample)
   );
   assert!(
      sample_reads
         .iter()
         .all(|(offset, _)| *offset != first_sample)
   );
}

#[tokio::test]
async fn hardening_distant_chunks_are_read_as_separate_batches() {
   let (bytes, first_sample, second_sample) = multi_chunk_subtitle_mp4();
   let sizes = (tx3g("First").len(), tx3g("Second").len());
   let reader = CountingReader::new(bytes);
   let index = Arc::new(SubtitleIndex::read(&reader).await.expect("build index"));
   let reads_after_index = reader.ranges().len();

   let tracks = Arc::clone(&index)
      .subtitles(&reader, None, None)
      .await
      .expect("extract both chunks");

   assert_eq!(tracks.len(), 1);
   assert_eq!(tracks[0].cues.len(), 2);
   assert_eq!(tracks[0].cues[0].text, "First");
   assert_eq!(tracks[0].cues[1].text, "Second");

   // Batches complete out of order, so compare the reads by offset rather than
   // by arrival.
   let mut sample_reads = reader.ranges()[reads_after_index..].to_vec();
   sample_reads.sort_by_key(|(offset, _)| *offset);
   assert_eq!(
      sample_reads,
      vec![(first_sample, sizes.0), (second_sample, sizes.1)]
   );
}

#[tokio::test]
async fn hardening_co64_chunk_offsets_locate_every_chunk() {
   let reader = BytesReader(co64_subtitle_mp4());

   let tracks = read_subtitles(&reader, None)
      .await
      .expect("extract a co64-addressed track");

   assert_eq!(tracks.len(), 1);
   assert_eq!(tracks[0].cues.len(), 2);
   assert_eq!(tracks[0].cues[0].text, "First");
   assert_eq!(tracks[0].cues[1].text, "Second");
}

#[tokio::test]
async fn hardening_half_open_range_excludes_end_and_includes_start_boundary() {
   let reader = BytesReader(subtitle_mp4());
   let tracks = read_subtitles_in_range(
      &reader,
      Some(TrackFilter::TrackId(1)),
      Some((Duration::from_secs(1), Duration::from_secs(2))),
   )
   .await
   .expect("extract exact half-open range");

   assert_eq!(tracks[0].cues.len(), 1);
   assert_eq!(tracks[0].cues[0].cue_id, 2);
   assert_eq!(tracks[0].cues[0].start_time, Duration::from_secs(1));
}

#[tokio::test]
async fn hardening_zero_delta_sample_is_gap_with_stable_source_ids() {
   let reader = BytesReader(zero_delta_subtitle_mp4());
   let tracks = read_subtitles(&reader, Some(TrackFilter::TrackId(1)))
      .await
      .expect("extract track containing a genuine zero-delta timing gap");

   assert_eq!(tracks.len(), 1);
   assert_eq!(tracks[0].cues.len(), 2);
   assert_eq!(tracks[0].cues[0].cue_id, 1);
   assert_eq!(tracks[0].cues[0].start_time, Duration::ZERO);
   assert_eq!(tracks[0].cues[0].end_time, Duration::from_secs(1));
   assert_eq!(tracks[0].cues[1].cue_id, 3);
   assert_eq!(tracks[0].cues[1].start_time, Duration::from_secs(1));
   assert_eq!(tracks[0].cues[1].end_time, Duration::from_secs(2));
}

#[tokio::test]
async fn hardening_empty_and_whitespace_samples_are_timing_gaps() {
   let mut bytes = subtitle_mp4();
   let first = find_bytes(&bytes, b"First");
   bytes[first..first + 5].copy_from_slice(b"     ");
   let reader = BytesReader(bytes);

   let tracks = read_subtitles(&reader, Some(TrackFilter::TrackId(1)))
      .await
      .expect("extract track with whitespace gap");
   assert_eq!(tracks[0].cues.len(), 1);
   assert_eq!(tracks[0].cues[0].cue_id, 2);
}

#[tokio::test]
async fn hardening_unsupported_codec_is_skipped_broadly_and_errors_explicitly() {
   let mut bytes = subtitle_mp4();
   let codec = find_bytes(&bytes, b"tx3g");
   bytes[codec..codec + 4].copy_from_slice(b"junk");
   let reader = BytesReader(bytes);
   let index = Arc::new(
      SubtitleIndex::read(&reader)
         .await
         .expect("index rejected track"),
   );

   let broad = Arc::clone(&index)
      .subtitles(&reader, None, None)
      .await
      .unwrap();
   assert_eq!(broad.len(), 1);
   assert_eq!(broad[0].base.id, 2);
   Arc::clone(&index)
      .subtitles(&reader, Some(TrackFilter::TrackId(1)), None)
      .await
      .expect_err("explicit unsupported track errors");
}

#[tokio::test]
async fn subtitles_first_returns_only_the_first_track_in_physical_order() {
   let reader = BytesReader(subtitle_mp4());
   let index = Arc::new(SubtitleIndex::read(&reader).await.expect("index subtitles"));

   let tracks = Arc::clone(&index)
      .subtitles_first(&reader, None)
      .await
      .expect("extract first subtitle track");

   assert_eq!(tracks.len(), 1);
   assert_eq!(tracks[0].base.id, 1);
}

#[tokio::test]
async fn subtitles_first_skips_a_rejected_track_before_returning() {
   let mut bytes = subtitle_mp4();
   let codec = find_bytes(&bytes, b"tx3g");
   bytes[codec..codec + 4].copy_from_slice(b"junk");
   let reader = BytesReader(bytes);
   let index = Arc::new(
      SubtitleIndex::read(&reader)
         .await
         .expect("index rejected first track"),
   );

   let tracks = Arc::clone(&index)
      .subtitles_first(&reader, None)
      .await
      .expect("skip rejected track and extract next valid track");

   assert_eq!(tracks.len(), 1);
   assert_eq!(tracks[0].base.id, 2);
}

#[tokio::test]
async fn hardening_malformed_table_is_retained_as_a_track_rejection() {
   let mut bytes = subtitle_mp4();
   let stsz = find_bytes(&bytes, b"stsz");
   let sample_count = stsz + 12;
   bytes[sample_count..sample_count + 4].copy_from_slice(&3u32.to_be_bytes());
   let reader = BytesReader(bytes);
   let index = Arc::new(
      SubtitleIndex::read(&reader)
         .await
         .expect("index rejected track"),
   );

   let broad = Arc::clone(&index)
      .subtitles(&reader, None, None)
      .await
      .unwrap();
   assert_eq!(broad.len(), 1);
   assert_eq!(broad[0].base.id, 2);
   let error = Arc::clone(&index)
      .subtitles(&reader, Some(TrackFilter::TrackId(1)), None)
      .await
      .expect_err("explicit malformed track errors");
   assert!(error.to_string().contains("stsz"), "{error}");
}

#[tokio::test]
async fn hardening_variable_stsz_count_above_limit_is_a_track_rejection() {
   let mut bytes = subtitle_mp4();
   let stsz = find_bytes(&bytes, b"stsz");
   let sample_count = stsz + 12;
   bytes[sample_count..sample_count + 4].copy_from_slice(&u32::MAX.to_be_bytes());
   let reader = BytesReader(bytes);
   let index = Arc::new(
      SubtitleIndex::read(&reader)
         .await
         .expect("oversized malformed stsz should reject only its track"),
   );

   let broad = Arc::clone(&index)
      .subtitles(&reader, None, None)
      .await
      .unwrap();
   assert_eq!(broad.len(), 1);
   assert_eq!(broad[0].base.id, 2);
   Arc::clone(&index)
      .subtitles(&reader, Some(TrackFilter::TrackId(1)), None)
      .await
      .expect_err("explicit malformed variable-size track errors");
}

#[tokio::test]
async fn hardening_oversized_sample_preserves_sibling_unless_explicitly_selected() {
   let mut bytes = subtitle_mp4();
   let stsz = find_bytes(&bytes, b"stsz");
   let first_sample_size = stsz + 16;
   bytes[first_sample_size..first_sample_size + 4]
      .copy_from_slice(&(2 * 1024 * 1024u32).to_be_bytes());
   let reader = BytesReader(bytes);
   let index = Arc::new(SubtitleIndex::read(&reader).await.expect("index subtitles"));

   let broad = Arc::clone(&index)
      .subtitles(&reader, None, None)
      .await
      .expect("an oversized sample must reject only its track");
   assert_eq!(broad.len(), 1);
   assert_eq!(broad[0].base.id, 2);

   let error = Arc::clone(&index)
      .subtitles(&reader, Some(TrackFilter::TrackId(1)), None)
      .await
      .expect_err("an explicitly selected oversized track must fail");
   assert!(
      error
         .to_string()
         .contains("invalid sample size: 2097152 bytes"),
      "{error}"
   );
}

#[tokio::test]
async fn hardening_fixed_stsz_count_mismatch_is_a_track_rejection() {
   let reader = BytesReader(fixed_stsz_count_mismatch_mp4());
   let index = Arc::new(
      SubtitleIndex::read(&reader)
         .await
         .expect("inconsistent fixed stsz should reject only its track"),
   );

   let broad = Arc::clone(&index)
      .subtitles(&reader, None, None)
      .await
      .unwrap();
   assert_eq!(broad.len(), 1);
   assert_eq!(broad[0].base.id, 2);
   Arc::clone(&index)
      .subtitles(&reader, Some(TrackFilter::TrackId(1)), None)
      .await
      .expect_err("explicit malformed fixed-size track errors");
}

async fn assert_oversized_table_count_is_track_local(fourcc: [u8; 4]) {
   let mut bytes = subtitle_mp4();
   let table_fourcc = if fourcc == *b"co64" { *b"stco" } else { fourcc };
   let table = find_bytes(&bytes, &table_fourcc);
   bytes[table..table + 4].copy_from_slice(&fourcc);
   bytes[table + 8..table + 12].copy_from_slice(&u32::MAX.to_be_bytes());
   let reader = BytesReader(bytes);

   let index = Arc::new(
      SubtitleIndex::read(&reader)
         .await
         .expect("oversized malformed table should reject only its track"),
   );
   let broad = Arc::clone(&index)
      .subtitles(&reader, None, None)
      .await
      .unwrap();
   assert_eq!(broad.len(), 1);
   assert_eq!(broad[0].base.id, 2);
}

#[tokio::test]
async fn hardening_oversized_stsd_count_is_track_local() {
   assert_oversized_table_count_is_track_local(*b"stsd").await;
}

#[tokio::test]
async fn hardening_oversized_stts_count_is_track_local() {
   assert_oversized_table_count_is_track_local(*b"stts").await;
}

#[tokio::test]
async fn hardening_oversized_stsc_count_is_track_local() {
   assert_oversized_table_count_is_track_local(*b"stsc").await;
}

#[tokio::test]
async fn hardening_oversized_stco_count_is_track_local() {
   assert_oversized_table_count_is_track_local(*b"stco").await;
}

#[tokio::test]
async fn hardening_oversized_co64_count_is_track_local() {
   assert_oversized_table_count_is_track_local(*b"co64").await;
}

#[tokio::test]
async fn hardening_invalid_sample_description_reference_is_track_local() {
   let mut bytes = subtitle_mp4();
   let stsc = find_bytes(&bytes, b"stsc");
   let description_index = stsc + 20;
   bytes[description_index..description_index + 4].copy_from_slice(&2u32.to_be_bytes());
   let reader = BytesReader(bytes);
   let index = Arc::new(
      SubtitleIndex::read(&reader)
         .await
         .expect("index rejected track"),
   );

   let broad = Arc::clone(&index)
      .subtitles(&reader, None, None)
      .await
      .unwrap();
   assert_eq!(broad.len(), 1);
   assert_eq!(broad[0].base.id, 2);
   Arc::clone(&index)
      .subtitles(&reader, Some(TrackFilter::TrackId(1)), None)
      .await
      .expect_err("explicit invalid description reference errors");
}

async fn assert_description_policy_rejects_track(bytes: Vec<u8>) {
   let reader = BytesReader(bytes);
   let index = Arc::new(
      SubtitleIndex::read(&reader)
         .await
         .expect("index rejected description policy track"),
   );
   let broad = Arc::clone(&index)
      .subtitles(&reader, None, None)
      .await
      .unwrap();
   assert_eq!(broad.len(), 1);
   assert_eq!(broad[0].base.id, 2);
   Arc::clone(&index)
      .subtitles(&reader, Some(TrackFilter::TrackId(1)), None)
      .await
      .expect_err("explicit rejected description track errors");
}

#[tokio::test]
async fn hardening_mixed_referenced_supported_descriptions_reject_track() {
   assert_description_policy_rejects_track(description_policy_mp4(*b"wvtt", true)).await;
}

#[tokio::test]
async fn hardening_referenced_unsupported_description_rejects_track() {
   assert_description_policy_rejects_track(description_policy_mp4(*b"junk", true)).await;
}

#[tokio::test]
async fn hardening_unreferenced_unsupported_description_does_not_reject_track() {
   let reader = BytesReader(description_policy_mp4(*b"junk", false));
   let tracks = read_subtitles(&reader, Some(TrackFilter::TrackId(1)))
      .await
      .expect("only referenced descriptions control codec policy");

   assert_eq!(tracks.len(), 1);
   assert_eq!(tracks[0].base.codec, "tx3g");
   assert_eq!(tracks[0].cues.len(), 2);
}

#[test]
fn subtitle_fixture_matches_reviewable_builder() {
   assert_eq!(
      include_bytes!("fixtures/subtitles.mp4").as_slice(),
      subtitle_mp4()
   );
}

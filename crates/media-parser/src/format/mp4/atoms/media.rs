//! Media-oriented MP4 atom parsers shared by track-oriented features.

use super::budget::{RetainedBudget, TableParseError, TableResult, budgeted_vec};
use super::read_box;
use crate::helpers::{read_u16_be, read_u32_be, read_u64_be};

#[derive(Debug, Clone, Copy)]
pub struct TrackHeader {
   pub id: u32,
   pub track_enabled: bool,
   pub duration: u64,
   pub width: u32,
   pub height: u32,
}

#[derive(Debug, Clone, Copy)]
pub struct MediaHeader {
   pub timescale: u32,
   pub duration: u64,
   pub language: Option<[u8; 3]>,
}

#[derive(Debug, Clone)]
pub struct SampleDescription<T> {
   pub codec: String,
   pub entry_count: u32,
   pub entry: T,
}

#[derive(Debug, Clone)]
pub(in crate::format::mp4) struct SampleDescriptionEntry {
   pub codec: String,
}

const TKHD_V0_TRACK_ID_OFFSET: usize = 12;
const TKHD_V0_DURATION_OFFSET: usize = 20;
const TKHD_V0_WIDTH_OFFSET: usize = 76;
const TKHD_V0_HEIGHT_OFFSET: usize = 80;
const TKHD_V1_TRACK_ID_OFFSET: usize = 20;
const TKHD_V1_DURATION_OFFSET: usize = 28;
const TKHD_V1_WIDTH_OFFSET: usize = 88;
const TKHD_V1_HEIGHT_OFFSET: usize = 92;

const MDHD_V0_TIMESCALE_OFFSET: usize = 12;
const MDHD_V0_DURATION_OFFSET: usize = 16;
const MDHD_V0_LANGUAGE_OFFSET: usize = 20;
const MDHD_V1_TIMESCALE_OFFSET: usize = 20;
const MDHD_V1_DURATION_OFFSET: usize = 24;
const MDHD_V1_LANGUAGE_OFFSET: usize = 32;

const HDLR_HANDLER_TYPE_OFFSET: usize = 8;
const STSD_ENTRIES_OFFSET: usize = 8;
const VISUAL_WIDTH_OFFSET: usize = 24;
const VISUAL_HEIGHT_OFFSET: usize = 26;
const AUDIO_VERSION_OFFSET: usize = 8;
const AUDIO_CHANNELS_OFFSET: usize = 16;
const AUDIO_SAMPLE_RATE_OFFSET: usize = 24;
const AUDIO_V2_SAMPLE_RATE_OFFSET: usize = 32;
const AUDIO_V2_CHANNELS_OFFSET: usize = 40;

pub fn parse_tkhd(tkhd: &[u8]) -> Option<TrackHeader> {
   let version = *tkhd.first()?;
   let (track_id_offset, duration_offset, width_offset, height_offset) = match version {
      0 => (
         TKHD_V0_TRACK_ID_OFFSET,
         TKHD_V0_DURATION_OFFSET,
         TKHD_V0_WIDTH_OFFSET,
         TKHD_V0_HEIGHT_OFFSET,
      ),
      1 => (
         TKHD_V1_TRACK_ID_OFFSET,
         TKHD_V1_DURATION_OFFSET,
         TKHD_V1_WIDTH_OFFSET,
         TKHD_V1_HEIGHT_OFFSET,
      ),
      _ => return None,
   };

   let duration = if version == 0 {
      read_u32_be(tkhd, duration_offset)? as u64
   } else {
      read_u64_be(tkhd, duration_offset)?
   };

   Some(TrackHeader {
      id: read_u32_be(tkhd, track_id_offset)?,
      track_enabled: read_u32_be(tkhd, 0)? & 1 != 0,
      duration,
      width: read_fixed_16_16(tkhd, width_offset).unwrap_or(0),
      height: read_fixed_16_16(tkhd, height_offset).unwrap_or(0),
   })
}

pub fn parse_mdhd(mdhd: &[u8]) -> Option<MediaHeader> {
   let version = *mdhd.first()?;
   let (timescale_offset, duration_offset, language_offset) = match version {
      0 => (
         MDHD_V0_TIMESCALE_OFFSET,
         MDHD_V0_DURATION_OFFSET,
         MDHD_V0_LANGUAGE_OFFSET,
      ),
      1 => (
         MDHD_V1_TIMESCALE_OFFSET,
         MDHD_V1_DURATION_OFFSET,
         MDHD_V1_LANGUAGE_OFFSET,
      ),
      _ => return None,
   };

   let duration = if version == 0 {
      read_u32_be(mdhd, duration_offset)? as u64
   } else {
      read_u64_be(mdhd, duration_offset)?
   };

   Some(MediaHeader {
      timescale: read_u32_be(mdhd, timescale_offset)?,
      duration,
      language: read_u16_be(mdhd, language_offset).and_then(decode_language),
   })
}

pub fn parse_hdlr(hdlr: &[u8]) -> Option<[u8; 4]> {
   hdlr
      .get(HDLR_HANDLER_TYPE_OFFSET..HDLR_HANDLER_TYPE_OFFSET + 4)?
      .try_into()
      .ok()
}

pub fn parse_stsd<T>(
   stsd: &[u8],
   decode_entry: impl FnOnce(&[u8]) -> T,
) -> Option<SampleDescription<T>> {
   let (entry_count, entries) = stsd_header(stsd)?;
   let sample_entry = read_box(entries, 0)?;

   Some(SampleDescription {
      codec: fourcc_string(sample_entry.fourcc),
      entry_count,
      entry: decode_entry(sample_entry.payload),
   })
}

pub(in crate::format::mp4) fn parse_stsd_entries_bounded(
   stsd: &[u8],
   budget: &mut RetainedBudget,
) -> TableResult<Vec<SampleDescriptionEntry>> {
   let (raw_entry_count, entries_bytes) =
      stsd_header(stsd).ok_or(TableParseError::Invalid("malformed stsd table"))?;
   let entry_count =
      usize::try_from(raw_entry_count).map_err(|_| TableParseError::BudgetExceeded)?;
   if entry_count > entries_bytes.len() / 8 {
      return Err(TableParseError::Invalid("malformed stsd entry count"));
   }
   let mut entries = budgeted_vec(entry_count, budget)?;

   let mut offset = 0;
   for _ in 0..entry_count {
      let sample_entry =
         read_box(entries_bytes, offset).ok_or(TableParseError::Invalid("malformed stsd entry"))?;
      offset = offset
         .checked_add(sample_entry.total_size)
         .filter(|end| *end <= entries_bytes.len())
         .ok_or(TableParseError::Invalid("malformed stsd entry"))?;
      let codec = budgeted_fourcc(sample_entry.fourcc, budget)?;
      entries.push(SampleDescriptionEntry { codec });
   }
   if offset != entries_bytes.len() {
      return Err(TableParseError::Invalid("trailing bytes in stsd table"));
   }
   Ok(entries)
}

fn stsd_header(stsd: &[u8]) -> Option<(u32, &[u8])> {
   Some((read_u32_be(stsd, 4)?, stsd.get(STSD_ENTRIES_OFFSET..)?))
}

fn budgeted_fourcc(fourcc: [u8; 4], budget: &mut RetainedBudget) -> TableResult<String> {
   let lossy = String::from_utf8_lossy(&fourcc);
   budget.charge_bytes(lossy.len())?;
   let mut codec = String::new();
   codec
      .try_reserve_exact(lossy.len())
      .map_err(|_| TableParseError::AllocationFailed)?;
   budget.charge_bytes(codec.capacity().saturating_sub(lossy.len()))?;
   codec.push_str(&lossy);
   Ok(codec)
}

pub fn visual_dimensions(payload: &[u8]) -> (Option<u32>, Option<u32>) {
   (
      read_u16_be(payload, VISUAL_WIDTH_OFFSET).map(u32::from),
      read_u16_be(payload, VISUAL_HEIGHT_OFFSET).map(u32::from),
   )
}

pub fn audio_params(payload: &[u8]) -> (Option<u16>, Option<u32>) {
   match read_u16_be(payload, AUDIO_VERSION_OFFSET) {
      Some(0 | 1) => (
         read_u16_be(payload, AUDIO_CHANNELS_OFFSET),
         read_u32_be(payload, AUDIO_SAMPLE_RATE_OFFSET).map(|rate| rate >> 16),
      ),
      Some(2) => (
         read_u32_be(payload, AUDIO_V2_CHANNELS_OFFSET)
            .and_then(|channels| u16::try_from(channels).ok()),
         read_u64_be(payload, AUDIO_V2_SAMPLE_RATE_OFFSET).and_then(|bits| {
            let rate = f64::from_bits(bits);
            (rate.is_finite() && rate > 0.0 && rate <= u32::MAX as f64).then(|| rate.round() as u32)
         }),
      ),
      _ => (None, None),
   }
}

pub fn stts_sample_count(stts: &[u8]) -> Option<u32> {
   let entry_count = read_u32_be(stts, 4)?;
   let mut total_samples = 0u32;
   let mut offset = 8usize;
   for _ in 0..entry_count {
      total_samples = total_samples.checked_add(read_u32_be(stts, offset)?)?;
      offset = offset.checked_add(8)?;
   }
   Some(total_samples)
}

pub fn decode_language(code: u16) -> Option<[u8; 3]> {
   if code == 0 {
      return None;
   }

   let chars = [
      (((code >> 10) & 0x1f) as u8).checked_add(0x60)?,
      (((code >> 5) & 0x1f) as u8).checked_add(0x60)?,
      ((code & 0x1f) as u8).checked_add(0x60)?,
   ];

   if chars.iter().all(u8::is_ascii_lowercase) && &chars != b"und" {
      Some(chars)
   } else {
      None
   }
}

pub fn fourcc_string(fourcc: [u8; 4]) -> String {
   String::from_utf8_lossy(&fourcc).into_owned()
}

fn read_fixed_16_16(buf: &[u8], offset: usize) -> Option<u32> {
   read_u32_be(buf, offset).map(|value| value >> 16)
}

#[cfg(test)]
mod tests {
   use super::*;

   #[test]
   fn test_decode_language() {
      assert_eq!(decode_language(0x15c7), Some(*b"eng"));
      assert_eq!(decode_language(0x55c4), None);
   }

   #[test]
   fn test_parse_tkhd_v0() {
      // Offsets are spec-derived literals (NOT the module constants) so the
      // test is an independent oracle for the v0 field layout.
      let mut tkhd = vec![0u8; 84];
      tkhd[0] = 0; // version 0
      tkhd[3] = 1; // track_enabled flag
      tkhd[12..16].copy_from_slice(&3u32.to_be_bytes()); // track_ID
      tkhd[20..24].copy_from_slice(&1000u32.to_be_bytes()); // duration (32-bit)
      tkhd[76..80].copy_from_slice(&(640u32 << 16).to_be_bytes()); // width 16.16
      tkhd[80..84].copy_from_slice(&(480u32 << 16).to_be_bytes()); // height 16.16

      let parsed = parse_tkhd(&tkhd).unwrap();
      assert_eq!(parsed.id, 3);
      assert!(parsed.track_enabled);
      assert_eq!(parsed.duration, 1000);
      assert_eq!(parsed.width, 640);
      assert_eq!(parsed.height, 480);
   }

   #[test]
   fn test_parse_tkhd_v1_reads_64bit_duration() {
      // Offsets are spec-derived literals (NOT the module constants) so the
      // test is an independent oracle for the field layout.
      let mut tkhd = vec![0u8; 96];
      tkhd[0] = 1; // version 1
      tkhd[20..24].copy_from_slice(&7u32.to_be_bytes()); // track_ID
      let duration = 5_000_000_000u64; // > u32::MAX, distinguishes offset 28 vs 32
      tkhd[28..36].copy_from_slice(&duration.to_be_bytes());
      tkhd[88..92].copy_from_slice(&(1920u32 << 16).to_be_bytes()); // width 16.16
      tkhd[92..96].copy_from_slice(&(1080u32 << 16).to_be_bytes()); // height 16.16

      let parsed = parse_tkhd(&tkhd).unwrap();
      assert_eq!(parsed.id, 7);
      assert!(!parsed.track_enabled);
      assert_eq!(parsed.duration, 5_000_000_000);
      assert_eq!(parsed.width, 1920);
      assert_eq!(parsed.height, 1080);
   }

   #[test]
   fn test_parse_mdhd_v0() {
      let mut mdhd = vec![0u8; 24];
      mdhd[0] = 0;
      mdhd[12..16].copy_from_slice(&90_000u32.to_be_bytes());
      mdhd[16..20].copy_from_slice(&1000u32.to_be_bytes());
      mdhd[20..22].copy_from_slice(&0x15c7u16.to_be_bytes());

      let parsed = parse_mdhd(&mdhd).unwrap();
      assert_eq!(parsed.timescale, 90_000);
      assert_eq!(parsed.duration, 1000);
      assert_eq!(parsed.language, Some(*b"eng"));
   }

   #[test]
   fn test_parse_mdhd_v1_reads_64bit_duration() {
      let mut mdhd = vec![0u8; 36];
      mdhd[0] = 1;
      mdhd[20..24].copy_from_slice(&48_000u32.to_be_bytes());
      let duration = 5_000_000_000u64;
      mdhd[24..32].copy_from_slice(&duration.to_be_bytes());
      mdhd[32..34].copy_from_slice(&0x15c7u16.to_be_bytes());

      let parsed = parse_mdhd(&mdhd).unwrap();
      assert_eq!(parsed.timescale, 48_000);
      assert_eq!(parsed.duration, 5_000_000_000);
      assert_eq!(parsed.language, Some(*b"eng"));
   }

   #[test]
   fn test_parse_stsd_video_entry() {
      let mut visual_payload = vec![0u8; 78];
      // Spec-derived literals keep the test independent from parser constants.
      visual_payload[24..26].copy_from_slice(&320u16.to_be_bytes());
      visual_payload[26..28].copy_from_slice(&180u16.to_be_bytes());

      let mut stsd = vec![0u8; 8];
      stsd[4..8].copy_from_slice(&1u32.to_be_bytes());
      stsd.extend(make_box(b"avc1", &visual_payload));

      let parsed = parse_stsd(&stsd, visual_dimensions).unwrap();
      assert_eq!(parsed.codec, "avc1");
      assert_eq!(parsed.entry, (Some(320), Some(180)));
      assert_eq!(parsed.entry_count, 1);
   }

   #[test]
   fn test_parse_stsd_audio_v0_entry() {
      let mut audio_payload = vec![0u8; 28];
      // Spec-derived literals keep the test independent from parser constants.
      audio_payload[8..10].copy_from_slice(&0u16.to_be_bytes());
      audio_payload[16..18].copy_from_slice(&2u16.to_be_bytes());
      audio_payload[24..28].copy_from_slice(&(44_100u32 << 16).to_be_bytes());

      let mut stsd = vec![0u8; 8];
      stsd[4..8].copy_from_slice(&1u32.to_be_bytes());
      stsd.extend(make_box(b"mp4a", &audio_payload));

      let parsed = parse_stsd(&stsd, audio_params).unwrap();
      assert_eq!(parsed.codec, "mp4a");
      assert_eq!(parsed.entry, (Some(2), Some(44_100)));
   }

   #[test]
   fn test_parse_stsd_audio_v2_entry() {
      let mut audio_payload = vec![0u8; 44];
      // QuickTime v2 stores the rate as an IEEE-754 f64 and channels as u32.
      audio_payload[8..10].copy_from_slice(&2u16.to_be_bytes());
      audio_payload[32..40].copy_from_slice(&192_000f64.to_bits().to_be_bytes());
      audio_payload[40..44].copy_from_slice(&6u32.to_be_bytes());

      let mut stsd = vec![0u8; 8];
      stsd[4..8].copy_from_slice(&1u32.to_be_bytes());
      stsd.extend(make_box(b"lpcm", &audio_payload));

      let parsed = parse_stsd(&stsd, audio_params).unwrap();
      assert_eq!(parsed.codec, "lpcm");
      assert_eq!(parsed.entry, (Some(6), Some(192_000)));
   }

   #[test]
   fn test_audio_params_rejects_unknown_version() {
      let mut audio_payload = vec![0u8; 28];
      audio_payload[8..10].copy_from_slice(&3u16.to_be_bytes());
      audio_payload[16..18].copy_from_slice(&2u16.to_be_bytes());
      audio_payload[24..28].copy_from_slice(&(48_000u32 << 16).to_be_bytes());

      assert_eq!(audio_params(&audio_payload), (None, None));
   }

   #[test]
   fn bounded_stsd_validates_framing_before_charging_declared_entries() {
      let mut stsd = vec![0u8; 8];
      stsd[4..8].copy_from_slice(&u32::MAX.to_be_bytes());
      let mut budget = RetainedBudget::new(8);

      assert_eq!(
         parse_stsd_entries_bounded(&stsd, &mut budget).unwrap_err(),
         TableParseError::Invalid("malformed stsd entry count")
      );
      assert_eq!(budget.used_bytes(), 0);
   }

   #[test]
   fn bounded_stsd_rejects_trailing_framing_bytes() {
      let mut stsd = vec![0u8; 8];
      stsd[4..8].copy_from_slice(&1u32.to_be_bytes());
      stsd.extend(make_box(b"tx3g", &[0; 8]));
      stsd.push(0);
      let mut budget = RetainedBudget::new(1024);

      assert!(matches!(
         parse_stsd_entries_bounded(&stsd, &mut budget),
         Err(TableParseError::Invalid(_))
      ));
   }

   fn make_box(fourcc: &[u8; 4], payload: &[u8]) -> Vec<u8> {
      let size = 8 + payload.len();
      let mut data = Vec::with_capacity(size);
      data.extend_from_slice(&(size as u32).to_be_bytes());
      data.extend_from_slice(fourcc);
      data.extend_from_slice(payload);
      data
   }
}

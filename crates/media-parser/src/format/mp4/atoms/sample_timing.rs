//! MP4 sample timing and presentation-order calculations.

use super::budget::{RetainedBudget, TableParseError, TableResult, budgeted_vec};
use super::nav::Mp4Nav;
use super::samples::table_entries;
use crate::helpers::{read_u32_be, read_u64_be};
use std::time::Duration;

#[derive(Clone, Copy, Debug)]
pub(in crate::format::mp4) struct TimeToSampleEntry {
   pub sample_count: u32,
   pub sample_delta: u32,
}

#[derive(Clone, Debug)]
pub(in crate::format::mp4) struct SampleTimingTable {
   entries: Vec<TimeToSampleEntry>,
   sample_count: u32,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(in crate::format::mp4) struct SampleTiming {
   pub sample_index: u32,
   pub start_tick: u64,
   pub duration_ticks: u64,
}

impl SampleTimingTable {
   pub(in crate::format::mp4) fn parse(
      stts: &[u8],
      budget: &mut RetainedBudget,
   ) -> TableResult<Self> {
      let entry_count = usize::try_from(
         read_u32_be(stts, 4).ok_or(TableParseError::Invalid("malformed stts table"))?,
      )
      .map_err(|_| TableParseError::BudgetExceeded)?;

      let mut entries = budgeted_vec(entry_count, budget)?;

      let expected_len = entry_count
         .checked_mul(8)
         .and_then(|bytes| bytes.checked_add(8))
         .ok_or(TableParseError::BudgetExceeded)?;
      if expected_len != stts.len() {
         return Err(TableParseError::Invalid("malformed stts table"));
      }

      let mut sample_count = 0u32;
      for index in 0..entry_count {
         let offset = 8 + index * 8;
         let entry = TimeToSampleEntry {
            sample_count: read_u32_be(stts, offset)
               .ok_or(TableParseError::Invalid("malformed stts entry"))?,
            sample_delta: read_u32_be(stts, offset + 4)
               .ok_or(TableParseError::Invalid("malformed stts entry"))?,
         };
         if entry.sample_count == 0 {
            return Err(TableParseError::Invalid("zero stts sample count"));
         }
         // Capping the running sample count at `u32::MAX` also bounds any total
         // duration this table can express: the largest reachable sum is
         // `u32::MAX * u32::MAX`, which still fits `u64`. A separate duration
         // accumulator would therefore be arithmetic that can never fail.
         sample_count = sample_count
            .checked_add(entry.sample_count)
            .ok_or(TableParseError::Invalid("stts sample count overflow"))?;
         entries.push(entry);
      }

      Ok(Self {
         entries,
         sample_count,
      })
   }

   #[cfg(test)]
   pub(in crate::format::mp4) fn entries(&self) -> &[TimeToSampleEntry] {
      &self.entries
   }

   #[cfg(test)]
   pub(in crate::format::mp4) fn entries_capacity(&self) -> usize {
      self.entries.capacity()
   }

   pub(in crate::format::mp4) fn sample_count(&self) -> u32 {
      self.sample_count
   }

   pub(in crate::format::mp4) fn iter(&self) -> SampleTimingIter<'_> {
      SampleTimingIter {
         entries: self.entries.iter(),
         current_entry: None,
         remaining_in_entry: 0,
         remaining_samples: self.sample_count,
         next_sample_index: 1,
         next_start_tick: 0,
      }
   }
}

pub(in crate::format::mp4) struct SampleTimingIter<'a> {
   entries: std::slice::Iter<'a, TimeToSampleEntry>,
   current_entry: Option<TimeToSampleEntry>,
   remaining_in_entry: u32,
   remaining_samples: u32,
   next_sample_index: u32,
   next_start_tick: u64,
}

impl Iterator for SampleTimingIter<'_> {
   type Item = SampleTiming;

   fn next(&mut self) -> Option<Self::Item> {
      if self.remaining_in_entry == 0 {
         self.current_entry = self.entries.next().copied();
         self.remaining_in_entry = self.current_entry?.sample_count;
      }
      let entry = self.current_entry?;
      let timing = SampleTiming {
         sample_index: self.next_sample_index,
         start_tick: self.next_start_tick,
         duration_ticks: u64::from(entry.sample_delta),
      };
      self.remaining_in_entry -= 1;
      self.remaining_samples -= 1;
      self.next_sample_index = self
         .next_sample_index
         .checked_add(1)
         .unwrap_or(self.next_sample_index);
      self.next_start_tick = self
         .next_start_tick
         .checked_add(u64::from(entry.sample_delta))
         .expect("validated stts duration fits u64");
      Some(timing)
   }

   fn size_hint(&self) -> (usize, Option<usize>) {
      let remaining = usize::try_from(self.remaining_samples).unwrap_or(usize::MAX);
      (remaining, Some(remaining))
   }
}

impl ExactSizeIterator for SampleTimingIter<'_> {}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CompositionOffset {
   pub sample_count: u32,
   pub sample_offset: i64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SampleSelection {
   pub sample_index: u32,
   pub presentation_tick: u64,
}

const MAX_PRESENTATION_TIMELINE_BYTES: usize = 128 * 1024 * 1024;

fn presentation_timeline_sample_count_fits(sample_count: usize) -> bool {
   let Some(bytes_per_sample) = std::mem::size_of::<i128>().checked_add(std::mem::size_of::<u32>())
   else {
      return false;
   };
   sample_count != 0
      && sample_count
         .checked_mul(bytes_per_sample)
         .is_some_and(|bytes| bytes <= MAX_PRESENTATION_TIMELINE_BYTES)
}

/// Reusable presentation timestamps for a validated MP4 video sample table.
#[derive(Debug)]
pub struct PresentationTimeline {
   /// Signed presentation ticks in one-based MP4 sample order.
   ticks_by_sample: Vec<i128>,
   /// One-based sample indices ordered by `(presentation_tick, sample_index)`.
   samples_by_time: Vec<u32>,
}

impl PresentationTimeline {
   pub fn new(
      stts: &[u8],
      composition_offsets: Option<&[CompositionOffset]>,
      presentation_offset: i64,
      sample_count: u32,
   ) -> Option<Self> {
      let sample_count = usize::try_from(sample_count).ok()?;
      if !presentation_timeline_sample_count_fits(sample_count)
         || composition_offsets
            .is_some_and(|offsets| offsets.iter().any(|entry| entry.sample_count == 0))
      {
         return None;
      }

      let mut ticks_by_sample = Vec::new();
      ticks_by_sample.try_reserve_exact(sample_count).ok()?;
      let mut samples_by_time = Vec::new();
      samples_by_time.try_reserve_exact(sample_count).ok()?;
      let mut walker = TimingWalker::new(stts, composition_offsets)?;

      loop {
         match walker.next_segment() {
            TimingStep::Segment(segment) => {
               for position in 0..segment.sample_count {
                  if ticks_by_sample.len() == sample_count {
                     return None;
                  }
                  let sample_index = segment.first_sample.checked_add(position)?;
                  let decode_tick = segment
                     .decode_tick
                     .checked_add(u64::from(position).checked_mul(segment.sample_delta)?)?;
                  let presentation_tick = i128::from(decode_tick)
                     .checked_add(i128::from(segment.composition_offset))?
                     .checked_sub(i128::from(presentation_offset))?;
                  ticks_by_sample.push(presentation_tick);
                  if u64::try_from(presentation_tick).is_ok() {
                     samples_by_time.push(sample_index);
                  } else if presentation_tick >= 0 {
                     return None;
                  }
               }
            }
            TimingStep::Exhausted => break,
            TimingStep::Invalid => return None,
         }
      }

      if walker.has_unconsumed_ctts() || ticks_by_sample.len() != sample_count {
         return None;
      }
      samples_by_time.sort_unstable_by_key(|sample_index| {
         let index = usize::try_from(*sample_index).expect("u32 fits usize") - 1;
         (ticks_by_sample[index], *sample_index)
      });
      Some(Self {
         ticks_by_sample,
         samples_by_time,
      })
   }

   pub fn select(&self, target_tick: u64) -> Option<SampleSelection> {
      let target = (i128::from(target_tick), u32::MAX);
      let upper = self.samples_by_time.partition_point(|sample_index| {
         let index = usize::try_from(*sample_index).expect("u32 fits usize") - 1;
         (self.ticks_by_sample[index], *sample_index) <= target
      });
      let sample_index = if upper == 0 {
         *self.samples_by_time.first()?
      } else {
         self.samples_by_time[upper - 1]
      };
      Some(SampleSelection {
         sample_index,
         presentation_tick: u64::try_from(self.tick(sample_index)?).ok()?,
      })
   }

   pub fn tick(&self, sample_index: u32) -> Option<i128> {
      let index = usize::try_from(sample_index).ok()?.checked_sub(1)?;
      self.ticks_by_sample.get(index).copied()
   }

   pub fn ticks_for_range(&self, start_sample: u32, end_sample: u32) -> Option<Vec<(u32, i128)>> {
      if start_sample > end_sample {
         return None;
      }
      let start = usize::try_from(start_sample).ok()?.checked_sub(1)?;
      let end = usize::try_from(end_sample).ok()?;
      let ticks = self.ticks_by_sample.get(start..end)?;
      let mut result = Vec::new();
      result.try_reserve_exact(ticks.len()).ok()?;
      for (offset, tick) in ticks.iter().copied().enumerate() {
         let sample_index = start_sample.checked_add(u32::try_from(offset).ok()?)?;
         result.push((sample_index, tick));
      }
      Some(result)
   }
}

pub fn parse_ctts(ctts: &[u8]) -> Option<Vec<CompositionOffset>> {
   let version = *ctts.first()?;
   if version > 1 {
      return None;
   }
   let entry_count = table_entries(ctts, 8)?;

   let mut offsets = Vec::new();
   offsets.try_reserve(entry_count).ok()?;
   for index in 0..entry_count {
      let offset = 8 + index * 8;
      let sample_count = read_u32_be(ctts, offset)?;
      if sample_count == 0 {
         return None;
      }
      let raw_offset = read_u32_be(ctts, offset + 4)?;
      offsets.push(CompositionOffset {
         sample_count,
         sample_offset: if version == 0 {
            i64::from(raw_offset)
         } else {
            i64::from(i32::from_be_bytes(raw_offset.to_be_bytes()))
         },
      });
   }
   Some(offsets)
}

/// One stts/ctts segment in decode order: `sample_count` samples starting at
/// `first_sample` that share one sample delta and one composition offset.
#[derive(Debug, Clone, Copy)]
struct TimingSegment {
   first_sample: u32,
   decode_tick: u64,
   sample_count: u32,
   sample_delta: u64,
   composition_offset: i64,
}

/// Result of advancing a [`TimingWalker`].
enum TimingStep {
   Segment(TimingSegment),
   /// All stts entries were consumed.
   Exhausted,
   /// The timing tables are malformed (zero count/delta, missing ctts entry,
   /// or tick overflow).
   Invalid,
}

/// Walks the stts/ctts sample timing tables segment by segment in decode
/// order, keeping the running sample index and decode tick.
struct TimingWalker<'a> {
   stts: &'a [u8],
   composition_offsets: Option<&'a [CompositionOffset]>,
   entry_count: usize,
   sample_index: u32,
   decode_tick: u64,
   stts_index: usize,
   stts_remaining: u32,
   sample_delta: u64,
   ctts_index: usize,
   ctts_remaining: u32,
   composition_offset: i64,
}

impl<'a> TimingWalker<'a> {
   fn new(stts: &'a [u8], composition_offsets: Option<&'a [CompositionOffset]>) -> Option<Self> {
      let entry_count = table_entries(stts, 8)?;
      Some(Self {
         stts,
         composition_offsets,
         entry_count,
         sample_index: 1,
         decode_tick: 0,
         stts_index: 0,
         stts_remaining: 0,
         sample_delta: 0,
         ctts_index: 0,
         ctts_remaining: 0,
         composition_offset: 0,
      })
   }

   fn next_segment(&mut self) -> TimingStep {
      if self.stts_remaining == 0 {
         if self.stts_index == self.entry_count {
            return TimingStep::Exhausted;
         }
         let offset = 8 + self.stts_index * 8;
         let (Some(remaining), Some(delta)) = (
            read_u32_be(self.stts, offset),
            read_u32_be(self.stts, offset + 4),
         ) else {
            return TimingStep::Invalid;
         };
         if remaining == 0 || delta == 0 {
            return TimingStep::Invalid;
         }
         self.stts_remaining = remaining;
         self.sample_delta = u64::from(delta);
         self.stts_index += 1;
      }

      if let Some(offsets) = self.composition_offsets {
         if self.ctts_remaining == 0 {
            let Some(entry) = offsets.get(self.ctts_index) else {
               return TimingStep::Invalid;
            };
            self.ctts_remaining = entry.sample_count;
            self.composition_offset = entry.sample_offset;
            self.ctts_index += 1;
         }
      } else {
         self.ctts_remaining = self.stts_remaining;
         self.composition_offset = 0;
      }

      let segment_count = self.stts_remaining.min(self.ctts_remaining);
      let segment = TimingSegment {
         first_sample: self.sample_index,
         decode_tick: self.decode_tick,
         sample_count: segment_count,
         sample_delta: self.sample_delta,
         composition_offset: self.composition_offset,
      };
      let advance = u64::from(segment_count)
         .checked_mul(self.sample_delta)
         .and_then(|duration| self.decode_tick.checked_add(duration))
         .and_then(|decode_tick| {
            self
               .sample_index
               .checked_add(segment_count)
               .map(|sample_index| (decode_tick, sample_index))
         });
      let Some((decode_tick, next_sample_index)) = advance else {
         return TimingStep::Invalid;
      };
      self.decode_tick = decode_tick;
      self.sample_index = next_sample_index;
      self.stts_remaining -= segment_count;
      self.ctts_remaining -= segment_count;
      TimingStep::Segment(segment)
   }

   /// Whether any ctts-described samples remain after the stts table ended.
   fn has_unconsumed_ctts(&self) -> bool {
      self.ctts_remaining != 0
         || self
            .composition_offsets
            .is_some_and(|offsets| self.ctts_index != offsets.len())
   }
}

/// Resolves the presentation offset an `edts`/`elst` applies to a track,
/// degrading to none for edit lists this parser does not model.
///
/// Only a single normal-rate segment maps to a scalar offset. A list with
/// several real segments can repeat, reorder or retime media, which one
/// offset cannot express — applying the first segment's `media_time` to the
/// whole timeline would silently misplace every later segment. Falling back
/// to zero instead leaves such a track behaving exactly like one carrying no
/// `edts` at all, which is the same degrade-rather-than-fail policy
/// `resolve_gop_color` applies to the colour hint.
pub fn track_presentation_offset(trak: &[u8]) -> i64 {
   trak
      .nav(&[*b"edts", *b"elst"])
      .and_then(parse_elst_media_time)
      .unwrap_or(0)
}

/// Reads the media time of an edit list, or `None` when the list is not a
/// single normal-rate segment. See [`track_presentation_offset`].
fn parse_elst_media_time(elst: &[u8]) -> Option<i64> {
   let version = *elst.first()?;
   let entry_size = match version {
      0 => 12usize,
      1 => 20usize,
      _ => return None,
   };
   if table_entries(elst, entry_size)? != 1 {
      return None;
   }

   let offset = 8;
   let (segment_duration, media_time, rate_offset) = if version == 0 {
      (
         u64::from(read_u32_be(elst, offset)?),
         i64::from(i32::from_be_bytes(
            read_u32_be(elst, offset + 4)?.to_be_bytes(),
         )),
         offset + 8,
      )
   } else {
      (
         read_u64_be(elst, offset)?,
         i64::from_be_bytes(read_u64_be(elst, offset + 8)?.to_be_bytes()),
         offset + 16,
      )
   };
   let media_rate = read_u32_be(elst, rate_offset)?;
   if segment_duration == 0 || media_rate != 0x0001_0000 || media_time < -1 {
      return None;
   }
   // An empty edit (media_time -1) only delays presentation; it does not shift
   // media timestamps, so it maps to no presentation offset.
   Some(media_time.max(0))
}

pub fn duration_to_ticks(duration: Duration, timescale: u32) -> u64 {
   let ticks = duration.as_nanos().saturating_mul(u128::from(timescale)) / 1_000_000_000;
   u64::try_from(ticks).unwrap_or(u64::MAX)
}

pub(super) fn stts_sample_count(stts: &[u8]) -> Option<u32> {
   let entry_count = table_entries(stts, 8)?;
   (0..entry_count).try_fold(0u32, |total, index| {
      total.checked_add(read_u32_be(stts, 8 + index * 8)?)
   })
}

/// Total duration in media ticks described by the stts table.
pub fn stts_duration_ticks(stts: &[u8]) -> Option<u64> {
   let entry_count = table_entries(stts, 8)?;
   (0..entry_count).try_fold(0u64, |total, index| {
      let count = u64::from(read_u32_be(stts, 8 + index * 8)?);
      let delta = u64::from(read_u32_be(stts, 8 + index * 8 + 4)?);
      total.checked_add(count.checked_mul(delta)?)
   })
}

pub fn ticks_to_duration(ticks: u64, timescale: u32) -> Duration {
   if timescale == 0 {
      return Duration::ZERO;
   }
   let nanos = u128::from(ticks).saturating_mul(1_000_000_000) / u128::from(timescale);
   Duration::from_nanos(u64::try_from(nanos).unwrap_or(u64::MAX))
}

#[cfg(test)]
mod tests {
   use super::*;
   use crate::format::mp4::atoms::budget::RetainedBudget;

   fn stts_entries(entries: &[(u32, u32)]) -> Vec<u8> {
      let mut bytes = vec![0; 8];
      bytes[4..8].copy_from_slice(&u32::try_from(entries.len()).unwrap().to_be_bytes());
      for (count, delta) in entries {
         bytes.extend_from_slice(&count.to_be_bytes());
         bytes.extend_from_slice(&delta.to_be_bytes());
      }
      bytes
   }

   fn stts(count: u32, delta: u32) -> Vec<u8> {
      stts_entries(&[(count, delta)])
   }

   fn mp4_box(fourcc: &[u8; 4], payload: &[u8]) -> Vec<u8> {
      let mut data = u32::try_from(payload.len() + 8)
         .unwrap()
         .to_be_bytes()
         .to_vec();
      data.extend_from_slice(fourcc);
      data.extend_from_slice(payload);
      data
   }

   /// Builds a version 0 `elst` payload from `(segment_duration, media_time)`
   /// pairs, all at normal rate.
   fn elst_payload(segments: &[(u32, i32)]) -> Vec<u8> {
      let mut payload = vec![0; 4];
      payload.extend_from_slice(&u32::try_from(segments.len()).unwrap().to_be_bytes());
      for (segment_duration, media_time) in segments {
         payload.extend_from_slice(&segment_duration.to_be_bytes());
         payload.extend_from_slice(&media_time.to_be_bytes());
         payload.extend_from_slice(&0x0001_0000u32.to_be_bytes());
      }
      payload
   }

   fn trak_with_edit_list(segments: &[(u32, i32)]) -> Vec<u8> {
      mp4_box(b"edts", &mp4_box(b"elst", &elst_payload(segments)))
   }

   #[test]
   fn compact_timing_retains_runs_and_expands_samples_lazily() {
      let mut budget = RetainedBudget::new(usize::MAX);
      let table = SampleTimingTable::parse(&stts(100_000, 1_000), &mut budget).unwrap();

      assert_eq!(table.entries().len(), 1);
      assert_eq!(
         budget.used_bytes(),
         table.entries.capacity() * std::mem::size_of::<TimeToSampleEntry>()
      );
      assert_eq!(table.sample_count(), 100_000);
      assert_eq!(
         table.iter().nth(2),
         Some(SampleTiming {
            sample_index: 3,
            start_tick: 2_000,
            duration_ticks: 1_000,
         })
      );
   }

   #[test]
   fn compact_timing_rejects_aggregate_sample_count_overflow() {
      let mut budget = RetainedBudget::new(usize::MAX);

      assert!(matches!(
         SampleTimingTable::parse(&stts_entries(&[(u32::MAX, 1), (1, 1)]), &mut budget),
         Err(TableParseError::Invalid(_))
      ));
   }

   #[test]
   fn compact_timing_keeps_committed_budget_between_parses() {
      let entry_bytes = std::mem::size_of::<TimeToSampleEntry>();
      let second_entry_count = 1_024usize;
      let mut budget = RetainedBudget::new(second_entry_count * entry_bytes);

      SampleTimingTable::parse(&stts(1, 1), &mut budget).unwrap();
      let first_charge = budget.used_bytes();
      let second_entries = vec![(1, 1); second_entry_count];
      assert!(matches!(
         SampleTimingTable::parse(&stts_entries(&second_entries), &mut budget),
         Err(TableParseError::BudgetExceeded)
      ));
      assert_eq!(budget.used_bytes(), first_charge);
   }

   #[test]
   fn compact_timing_distinguishes_malformed_input_from_budget_failure() {
      let mut budget = RetainedBudget::new(usize::MAX);
      let malformed = [0, 0, 0, 0, 0, 0, 0, 1];

      assert!(matches!(
         SampleTimingTable::parse(&malformed, &mut budget),
         Err(TableParseError::Invalid(_))
      ));
   }

   #[test]
   fn compact_timing_does_not_refund_after_allocating_for_an_invalid_entry() {
      let entry_bytes = std::mem::size_of::<TimeToSampleEntry>();
      let mut budget = RetainedBudget::new(usize::MAX);

      assert!(matches!(
         SampleTimingTable::parse(&stts(0, 1), &mut budget),
         Err(TableParseError::Invalid(_))
      ));
      assert!(budget.used_bytes() >= entry_bytes);
   }

   #[test]
   fn compact_timing_allows_zero_sample_delta() {
      let mut budget = RetainedBudget::new(usize::MAX);
      let table = SampleTimingTable::parse(
         &stts_entries(&[(1, 1_000), (1, 0), (1, 1_000)]),
         &mut budget,
      )
      .expect("zero-duration subtitle samples are timing gaps");

      assert_eq!(table.sample_count(), 3);
      assert_eq!(
         table.iter().collect::<Vec<_>>(),
         vec![
            SampleTiming {
               sample_index: 1,
               start_tick: 0,
               duration_ticks: 1_000,
            },
            SampleTiming {
               sample_index: 2,
               start_tick: 1_000,
               duration_ticks: 0,
            },
            SampleTiming {
               sample_index: 3,
               start_tick: 1_000,
               duration_ticks: 1_000,
            },
         ]
      );
   }

   #[test]
   fn accepts_empty_edit_and_rejects_multi_segment_edit_lists() {
      let empty_edit = elst_payload(&[(1_000, -1)]);
      let multiple_edits = elst_payload(&[(1_000, 0), (1_000, 1_000)]);

      assert_eq!(parse_elst_media_time(&empty_edit), Some(0));
      assert_eq!(parse_elst_media_time(&multiple_edits), None);
   }

   #[test]
   fn falls_back_to_no_presentation_offset_for_unmodeled_edit_lists() {
      let single_segment = trak_with_edit_list(&[(1_000, 512)]);
      let several_segments = trak_with_edit_list(&[(1_000, 0), (1_000, 1_000)]);
      let malformed = mp4_box(b"edts", &mp4_box(b"elst", &[0, 0]));

      assert_eq!(track_presentation_offset(&single_segment), 512);
      assert_eq!(track_presentation_offset(&several_segments), 0);
      assert_eq!(track_presentation_offset(&malformed), 0);
      assert_eq!(track_presentation_offset(&[]), 0);
   }

   #[test]
   fn edit_list_media_time_defines_the_shared_cue_origin() {
      let trak = trak_with_edit_list(&[(1_000, 250)]);
      let presentation_offset = track_presentation_offset(&trak);
      let mut budget = RetainedBudget::new(usize::MAX);
      let timing = SampleTimingTable::parse(&stts(4, 100), &mut budget).unwrap();
      let fourth_sample = timing.iter().nth(3).unwrap();

      assert_eq!(presentation_offset, 250);
      assert_eq!(
         fourth_sample.start_tick - u64::try_from(presentation_offset).unwrap(),
         50
      );
      let thumbnail_timeline =
         PresentationTimeline::new(&stts(4, 100), None, presentation_offset, 4).unwrap();
      assert_eq!(thumbnail_timeline.tick(4), Some(50));
   }

   #[test]
   fn selects_sample_without_expanding_stts() {
      let timeline = PresentationTimeline::new(&stts(4, 1_000), None, 0, 4).unwrap();

      assert_eq!(
         timeline.select(2_500),
         Some(SampleSelection {
            sample_index: 3,
            presentation_tick: 2_000,
         })
      );
   }

   #[test]
   fn selects_sample_by_ctts_presentation_time() {
      let mut ctts = vec![0; 8];
      ctts[4..8].copy_from_slice(&3u32.to_be_bytes());
      for offset in [2_000u32, 3_000, 1_000] {
         ctts.extend_from_slice(&1u32.to_be_bytes());
         ctts.extend_from_slice(&offset.to_be_bytes());
      }
      let composition_offsets = parse_ctts(&ctts).unwrap();
      let timeline =
         PresentationTimeline::new(&stts(3, 1_000), Some(&composition_offsets), 2_000, 3).unwrap();

      assert_eq!(
         timeline.select(1_000),
         Some(SampleSelection {
            sample_index: 3,
            presentation_tick: 1_000,
         })
      );
   }

   #[test]
   fn presentation_timeline_handles_reordered_and_equal_ticks() {
      let composition_offsets = vec![
         CompositionOffset {
            sample_count: 1,
            sample_offset: 2_000,
         },
         CompositionOffset {
            sample_count: 1,
            sample_offset: 0,
         },
         CompositionOffset {
            sample_count: 1,
            sample_offset: -2_000,
         },
         CompositionOffset {
            sample_count: 1,
            sample_offset: 0,
         },
         CompositionOffset {
            sample_count: 1,
            sample_offset: -1_000,
         },
      ];
      let timeline =
         PresentationTimeline::new(&stts(5, 1_000), Some(&composition_offsets), 1_000, 5).unwrap();

      for (target, sample_index, presentation_tick) in [
         (0, 2, 0),
         (500, 2, 0),
         (1_000, 1, 1_000),
         (1_500, 1, 1_000),
         (2_000, 5, 2_000),
         (2_500, 5, 2_000),
         (10_000, 5, 2_000),
      ] {
         assert_eq!(
            timeline.select(target),
            Some(SampleSelection {
               sample_index,
               presentation_tick,
            }),
            "target {target}"
         );
      }

      let shifted = PresentationTimeline::new(&stts(5, 1_000), None, -1_000, 5).unwrap();
      assert_eq!(
         shifted.select(0),
         Some(SampleSelection {
            sample_index: 1,
            presentation_tick: 1_000,
         })
      );
   }

   #[test]
   fn presentation_timeline_preserves_signed_ticks_in_decode_order_ranges() {
      let timeline = PresentationTimeline::new(&stts(3, 1_000), None, 1_500, 3).unwrap();

      assert_eq!(
         timeline.ticks_for_range(1, 3),
         Some(vec![(1, -1_500), (2, -500), (3, 500)])
      );
      assert_eq!(timeline.tick(2), Some(-500));
      assert_eq!(timeline.ticks_for_range(0, 1), None);
      assert_eq!(timeline.ticks_for_range(2, 1), None);
      assert_eq!(timeline.ticks_for_range(1, 4), None);
   }

   #[test]
   fn presentation_timeline_rejects_malformed_timing_tables() {
      assert!(PresentationTimeline::new(&stts(0, 1), None, 0, 1).is_none());
      assert!(PresentationTimeline::new(&[], None, 0, 0).is_none());
      assert!(PresentationTimeline::new(&[], None, 0, 1).is_none());
      assert!(PresentationTimeline::new(&stts(1, 0), None, 0, 1).is_none());
      assert!(
         PresentationTimeline::new(
            &stts(2, 1),
            Some(&[CompositionOffset {
               sample_count: 1,
               sample_offset: 0,
            }]),
            0,
            2,
         )
         .is_none()
      );
      assert!(
         PresentationTimeline::new(
            &stts(1, 1),
            Some(&[CompositionOffset {
               sample_count: 2,
               sample_offset: 0,
            }]),
            0,
            1,
         )
         .is_none()
      );
      assert!(PresentationTimeline::new(&stts(1, 1), None, 0, 2).is_none());
      assert!(PresentationTimeline::new(&stts(2, u32::MAX), None, 0, 2).is_some());
      assert!(PresentationTimeline::new(&stts(u32::MAX, u32::MAX), None, 0, u32::MAX).is_none());
   }

   #[test]
   fn presentation_timeline_pins_the_storage_budget_boundary() {
      let bytes_per_sample = std::mem::size_of::<i128>() + std::mem::size_of::<u32>();
      let max_samples = MAX_PRESENTATION_TIMELINE_BYTES / bytes_per_sample;

      assert!(!presentation_timeline_sample_count_fits(0));
      assert!(presentation_timeline_sample_count_fits(max_samples));
      assert!(!presentation_timeline_sample_count_fits(max_samples + 1));
   }

   #[test]
   fn presentation_timeline_still_rejects_zero_sample_delta() {
      assert!(PresentationTimeline::new(&stts(2, 0), None, 0, 2).is_none());
   }

   #[test]
   fn sums_stts_duration_with_checked_arithmetic() {
      let mut bytes = vec![0; 8];
      bytes[4..8].copy_from_slice(&2u32.to_be_bytes());
      bytes.extend_from_slice(&3u32.to_be_bytes());
      bytes.extend_from_slice(&1_000u32.to_be_bytes());
      bytes.extend_from_slice(&2u32.to_be_bytes());
      bytes.extend_from_slice(&500u32.to_be_bytes());

      assert_eq!(stts_duration_ticks(&bytes), Some(4_000));

      let mut large = vec![0; 8];
      large[4..8].copy_from_slice(&1u32.to_be_bytes());
      large.extend_from_slice(&u32::MAX.to_be_bytes());
      large.extend_from_slice(&u32::MAX.to_be_bytes());

      assert_eq!(
         stts_duration_ticks(&large),
         Some(u64::from(u32::MAX) * u64::from(u32::MAX))
      );
   }

   #[test]
   fn parses_signed_ctts_v1_offsets() {
      let mut ctts = vec![1, 0, 0, 0, 0, 0, 0, 1];
      ctts.extend_from_slice(&2u32.to_be_bytes());
      ctts.extend_from_slice(&(-500i32).to_be_bytes());

      assert_eq!(
         parse_ctts(&ctts).unwrap(),
         vec![CompositionOffset {
            sample_count: 2,
            sample_offset: -500,
         }]
      );
   }
}

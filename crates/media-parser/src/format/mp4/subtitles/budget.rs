use crate::errors::{MediaParserError, Result};
use crate::format::mp4::atoms::RetainedBudget;

pub(super) struct IndexBudget {
   max_samples: usize,
   samples: usize,
   pub(super) retained: RetainedBudget,
}

#[derive(Debug)]
pub(super) enum IndexSampleChargeError {
   TrackTooLarge,
   BudgetExhausted(MediaParserError),
}

impl IndexBudget {
   pub(super) fn new(max_samples: usize, max_retained_bytes: usize) -> Self {
      Self {
         max_samples,
         samples: 0,
         retained: RetainedBudget::new(max_retained_bytes),
      }
   }

   /// Charges indexed samples against the request-wide ceiling, distinguishing
   /// a track that cannot fit by itself from cumulative budget exhaustion.
   pub(super) fn charge_samples(
      &mut self,
      count: usize,
   ) -> std::result::Result<(), IndexSampleChargeError> {
      if count > self.max_samples {
         return Err(IndexSampleChargeError::TrackTooLarge);
      }
      self.samples = checked_charge(
         self.samples,
         count,
         self.max_samples,
         "too many indexed subtitle samples",
      )
      .map_err(IndexSampleChargeError::BudgetExhausted)?;
      Ok(())
   }

   pub(super) fn samples(&self) -> usize {
      self.samples
   }
}

#[derive(Clone, Copy)]
pub(super) struct RequestLimits {
   pub(super) max_samples: usize,
   pub(super) max_cues: usize,
   pub(super) max_decoded_text_bytes: usize,
}

pub(super) struct RequestBudget {
   limits: RequestLimits,
   selected_samples: usize,
   cues: usize,
   decoded_text_bytes: usize,
}

impl RequestBudget {
   pub(super) fn new(limits: RequestLimits) -> Self {
      Self {
         limits,
         selected_samples: 0,
         cues: 0,
         decoded_text_bytes: 0,
      }
   }

   pub(super) fn charge_selected_samples(&mut self, count: usize) -> Result<()> {
      self.selected_samples = checked_charge(
         self.selected_samples,
         count,
         self.limits.max_samples,
         "too many selected subtitle samples",
      )?;
      Ok(())
   }

   pub(super) fn charge_cue(&mut self, count: usize) -> Result<()> {
      self.cues = checked_charge(
         self.cues,
         count,
         self.limits.max_cues,
         "too many subtitle cues",
      )?;
      Ok(())
   }

   pub(super) fn charge_decoded_text(&mut self, bytes: usize) -> Result<()> {
      self.decoded_text_bytes = checked_charge(
         self.decoded_text_bytes,
         bytes,
         self.limits.max_decoded_text_bytes,
         "MP4 subtitle decoded text budget exceeded",
      )?;
      Ok(())
   }
}

fn checked_charge(used: usize, amount: usize, max: usize, message: &'static str) -> Result<usize> {
   used
      .checked_add(amount)
      .filter(|total| *total <= max)
      .ok_or_else(|| MediaParserError::Other(message.to_owned()))
}

#[cfg(test)]
mod tests {
   use super::*;

   #[test]
   fn index_sample_limit_accepts_exact_and_rejects_plus_one_monotonically() {
      let mut budget = IndexBudget::new(2, 16);
      budget.charge_samples(2).unwrap();
      let error = budget.charge_samples(1).unwrap_err();

      assert!(matches!(
         error,
         IndexSampleChargeError::BudgetExhausted(MediaParserError::Other(_))
      ));
      assert_eq!(budget.samples(), 2);
   }

   #[test]
   fn individually_oversized_track_is_distinct_and_not_charged() {
      let mut budget = IndexBudget::new(2, 16);

      let error = budget.charge_samples(3).unwrap_err();

      assert!(matches!(error, IndexSampleChargeError::TrackTooLarge));
      assert_eq!(budget.samples(), 0);
   }

   #[test]
   fn rejected_track_work_charge_is_not_refunded() {
      let mut budget = IndexBudget::new(2, 16);
      budget.charge_samples(1).unwrap();

      assert!(budget.charge_samples(2).is_err());
      assert_eq!(budget.samples(), 1);
      budget.charge_samples(1).unwrap();
      assert_eq!(budget.samples(), 2);
   }

   #[test]
   fn retained_limit_accepts_exact_and_rejects_plus_one() {
      let mut budget = IndexBudget::new(0, 4);
      budget.retained.charge_bytes(4).unwrap();

      assert!(budget.retained.charge_bytes(1).is_err());
      assert_eq!(budget.retained.used_bytes(), 4);
   }

   #[test]
   fn request_limits_accept_exact_and_reject_plus_one_monotonically() {
      let limits = RequestLimits {
         max_samples: 2,
         max_cues: 2,
         max_decoded_text_bytes: 4,
      };
      let mut budget = RequestBudget::new(limits);
      budget.charge_selected_samples(2).unwrap();
      budget.charge_decoded_text(4).unwrap();
      budget.charge_cue(2).unwrap();

      assert!(budget.charge_selected_samples(1).is_err());
      assert!(budget.charge_decoded_text(1).is_err());
      assert!(budget.charge_cue(1).is_err());
      assert_eq!(budget.selected_samples, 2);
      assert_eq!(budget.decoded_text_bytes, 4);
      assert_eq!(budget.cues, 2);
   }
}

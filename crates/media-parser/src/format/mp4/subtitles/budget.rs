use crate::errors::{MediaParserError, Result};
use crate::format::mp4::atoms::RetainedBudget;

#[cfg(test)]
use super::SUBTITLE_ENVELOPE_PREFIX_BYTES;
use super::SUBTITLE_ENVELOPE_PROJECTED_BASE_BYTES;

pub(super) struct IndexBudget {
   max_samples: usize,
   samples: usize,
   pub(super) retained: RetainedBudget,
}

impl IndexBudget {
   pub(super) fn new(max_samples: usize, max_retained_bytes: usize) -> Self {
      Self {
         max_samples,
         samples: 0,
         retained: RetainedBudget::new(max_retained_bytes),
      }
   }

   /// Charges indexed samples against the request-wide ceiling.
   ///
   /// Exhausting a budget is a request-fatal resource failure, not malformed
   /// input, so it reports [`MediaParserError::Other`] like every other
   /// overflow in this subsystem — `checked_charge` below and `table_fatal`'s
   /// `BudgetExceeded` arm. `InvalidFormat` stays reserved for tracks whose
   /// bytes are actually wrong, which callers may skip.
   pub(super) fn charge_samples(&mut self, count: usize) -> Result<()> {
      self.samples = checked_charge(
         self.samples,
         count,
         self.max_samples,
         "too many indexed subtitle samples",
      )?;
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
   pub(super) max_output_bytes: usize,
}

pub(super) struct RequestBudget {
   limits: RequestLimits,
   selected_samples: usize,
   cues: usize,
   decoded_text_bytes: usize,
   projected_output_bytes: usize,
}

impl RequestBudget {
   pub(super) fn new(limits: RequestLimits) -> Result<Self> {
      let projected_output_bytes = checked_charge(
         0,
         SUBTITLE_ENVELOPE_PROJECTED_BASE_BYTES,
         limits.max_output_bytes,
         "MP4 subtitle output budget exceeded",
      )?;
      Ok(Self {
         limits,
         selected_samples: 0,
         cues: 0,
         decoded_text_bytes: 0,
         projected_output_bytes,
      })
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

   pub(super) fn charge_projection(&mut self, bytes: usize) -> Result<()> {
      self.projected_output_bytes = checked_charge(
         self.projected_output_bytes,
         bytes,
         self.limits.max_output_bytes,
         "MP4 subtitle output budget exceeded",
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

      // A budget ceiling is a resource failure, not malformed input, so it
      // must not be reported as a skippable track defect.
      assert!(matches!(error, MediaParserError::Other(_)));
      assert_eq!(budget.samples(), 2);
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
         max_output_bytes: SUBTITLE_ENVELOPE_PROJECTED_BASE_BYTES + 4,
      };
      let mut budget = RequestBudget::new(limits).unwrap();
      budget.charge_selected_samples(2).unwrap();
      budget.charge_decoded_text(4).unwrap();
      budget.charge_cue(2).unwrap();
      budget.charge_projection(4).unwrap();

      assert!(budget.charge_selected_samples(1).is_err());
      assert!(budget.charge_decoded_text(1).is_err());
      assert!(budget.charge_cue(1).is_err());
      assert!(budget.charge_projection(1).is_err());
      assert_eq!(budget.selected_samples, 2);
      assert_eq!(budget.decoded_text_bytes, 4);
      assert_eq!(budget.cues, 2);
      assert_eq!(budget.projected_output_bytes, limits.max_output_bytes);
   }

   #[test]
   fn request_projection_includes_the_complete_empty_envelope_base() {
      const REMAINING_BYTES: usize = 4;
      assert_eq!(SUBTITLE_ENVELOPE_PREFIX_BYTES, 4);
      assert_eq!(SUBTITLE_ENVELOPE_PROJECTED_BASE_BYTES, 30);
      assert_eq!(
         SUBTITLE_ENVELOPE_PROJECTED_BASE_BYTES - SUBTITLE_ENVELOPE_PREFIX_BYTES,
         26
      );
      let limits = RequestLimits {
         max_samples: 0,
         max_cues: 0,
         max_decoded_text_bytes: 0,
         max_output_bytes: SUBTITLE_ENVELOPE_PROJECTED_BASE_BYTES + REMAINING_BYTES,
      };
      let mut budget = RequestBudget::new(limits).unwrap();

      assert_eq!(
         budget.projected_output_bytes,
         SUBTITLE_ENVELOPE_PROJECTED_BASE_BYTES
      );
      budget.charge_projection(REMAINING_BYTES).unwrap();
      assert_eq!(budget.projected_output_bytes, limits.max_output_bytes);
      assert!(budget.charge_projection(1).is_err());
      assert_eq!(budget.projected_output_bytes, limits.max_output_bytes);
   }

   #[test]
   fn request_projection_rejects_a_cap_below_the_empty_envelope_base() {
      let limits = RequestLimits {
         max_samples: 0,
         max_cues: 0,
         max_decoded_text_bytes: 0,
         max_output_bytes: SUBTITLE_ENVELOPE_PROJECTED_BASE_BYTES - 1,
      };
      assert!(RequestBudget::new(limits).is_err());
   }
}

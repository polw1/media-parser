//! Monotonic accounting for retained MP4 table allocations.

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(in crate::format::mp4) enum TableParseError {
   Invalid(&'static str),
   BudgetExceeded,
   AllocationFailed,
}

pub(in crate::format::mp4) type TableResult<T> = std::result::Result<T, TableParseError>;

#[derive(Debug)]
pub(in crate::format::mp4) struct RetainedBudget {
   max_bytes: usize,
   used_bytes: usize,
}

impl RetainedBudget {
   pub(in crate::format::mp4) fn new(max_bytes: usize) -> Self {
      Self {
         max_bytes,
         used_bytes: 0,
      }
   }

   pub(in crate::format::mp4) fn used_bytes(&self) -> usize {
      self.used_bytes
   }

   pub(in crate::format::mp4) fn charge_bytes(&mut self, bytes: usize) -> TableResult<()> {
      let used_bytes = self
         .used_bytes
         .checked_add(bytes)
         .ok_or(TableParseError::BudgetExceeded)?;
      if used_bytes > self.max_bytes {
         return Err(TableParseError::BudgetExceeded);
      }
      self.used_bytes = used_bytes;
      Ok(())
   }

   pub(in crate::format::mp4) fn charge_capacity<T>(&mut self, capacity: usize) -> TableResult<()> {
      let bytes = capacity
         .checked_mul(std::mem::size_of::<T>())
         .ok_or(TableParseError::BudgetExceeded)?;
      self.charge_bytes(bytes)
   }

   /// Reserves room for `additional` elements on a vector whose existing
   /// capacity has already been charged to this budget.
   pub(in crate::format::mp4) fn try_reserve_vec_exact<T>(
      &mut self,
      values: &mut Vec<T>,
      additional: usize,
   ) -> TableResult<()> {
      let required_capacity = values
         .len()
         .checked_add(additional)
         .ok_or(TableParseError::BudgetExceeded)?;
      let old_capacity = values.capacity();
      let minimum_growth = if required_capacity > old_capacity {
         required_capacity
            .checked_sub(old_capacity)
            .ok_or(TableParseError::BudgetExceeded)?
      } else {
         0
      };
      self.charge_capacity::<T>(minimum_growth)?;
      values
         .try_reserve_exact(additional)
         .map_err(|_| TableParseError::AllocationFailed)?;
      let actual_growth = values
         .capacity()
         .checked_sub(old_capacity)
         .ok_or(TableParseError::BudgetExceeded)?;
      let extra_growth = actual_growth
         .checked_sub(minimum_growth)
         .ok_or(TableParseError::BudgetExceeded)?;
      self.charge_capacity::<T>(extra_growth)
   }
}

pub(in crate::format::mp4) fn budgeted_vec<T>(
   capacity: usize,
   budget: &mut RetainedBudget,
) -> TableResult<Vec<T>> {
   let mut values = Vec::new();
   budget.try_reserve_vec_exact(&mut values, capacity)?;
   Ok(values)
}

#[cfg(test)]
mod tests {
   use super::*;

   #[test]
   fn checked_add_overflow_is_budget_exceeded_without_changing_usage() {
      let mut budget = RetainedBudget::new(usize::MAX);
      budget.charge_bytes(usize::MAX).unwrap();

      assert_eq!(budget.charge_bytes(1), Err(TableParseError::BudgetExceeded));
      assert_eq!(budget.used_bytes(), usize::MAX);
   }

   #[test]
   fn checked_multiply_overflow_is_budget_exceeded_without_changing_usage() {
      let mut budget = RetainedBudget::new(usize::MAX);
      let overflowing_capacity = usize::MAX / std::mem::size_of::<u16>() + 1;

      assert_eq!(
         budget.charge_capacity::<u16>(overflowing_capacity),
         Err(TableParseError::BudgetExceeded)
      );
      assert_eq!(budget.used_bytes(), 0);
   }

   #[test]
   fn rejected_limit_charge_does_not_change_usage() {
      let mut budget = RetainedBudget::new(10);
      budget.charge_bytes(6).unwrap();

      assert_eq!(budget.charge_bytes(5), Err(TableParseError::BudgetExceeded));
      assert_eq!(budget.used_bytes(), 6);
   }

   #[test]
   fn vec_reservation_reconciles_from_already_accounted_capacity() {
      let mut values = Vec::with_capacity(2);
      values.extend([1u32, 2]);
      let mut budget = RetainedBudget::new(usize::MAX);
      budget.charge_capacity::<u32>(values.capacity()).unwrap();

      budget.try_reserve_vec_exact(&mut values, 3).unwrap();

      assert!(values.capacity() >= 5);
      assert_eq!(
         budget.used_bytes(),
         values.capacity() * std::mem::size_of::<u32>()
      );
   }

   #[test]
   fn vec_reservation_precharges_minimum_before_allocation() {
      let mut values = vec![1u16];
      let old_capacity = values.capacity();
      let old_bytes = old_capacity * std::mem::size_of::<u16>();
      let mut budget = RetainedBudget::new(old_bytes);
      budget.charge_bytes(old_bytes).unwrap();

      assert_eq!(
         budget.try_reserve_vec_exact(&mut values, 1),
         Err(TableParseError::BudgetExceeded)
      );
      assert_eq!(values.capacity(), old_capacity);
      assert_eq!(budget.used_bytes(), old_bytes);
   }

   #[test]
   fn failed_vec_allocation_keeps_its_precharge() {
      let mut values = Vec::<u8>::new();
      let mut budget = RetainedBudget::new(usize::MAX);

      assert_eq!(
         budget.try_reserve_vec_exact(&mut values, usize::MAX),
         Err(TableParseError::AllocationFailed)
      );
      assert_eq!(budget.used_bytes(), usize::MAX);
   }
}

//! Projection of indexed subtitle data into the public output types.
//!
//! Nothing here parses MP4. It exists because every string this crate hands
//! back is built through a fallible allocation: a malformed file must not be
//! able to abort the process by asking for a string the allocator cannot
//! serve, so `String::from`/`to_string` are unavailable to us.

use crate::errors::{MediaParserError, Result};
use std::collections::HashMap;

/// Copies a string through a fallible allocation.
pub(super) fn output_string(value: &str) -> Result<String> {
   let mut output = String::new();
   output
      .try_reserve_exact(value.len())
      .map_err(|_| MediaParserError::Other("MP4 subtitle output allocation failed".to_owned()))?;
   output.push_str(value);
   Ok(output)
}

/// Formats a decimal without the intermediate allocation `to_string` would
/// make, so the only allocation is the fallible one in [`output_string`].
fn decimal_string(mut value: u64) -> Result<String> {
   let mut digits = [0u8; 20];
   let mut start = digits.len();
   loop {
      start -= 1;
      digits[start] = b'0' + u8::try_from(value % 10).expect("one decimal digit fits u8");
      value /= 10;
      if value == 0 {
         break;
      }
   }
   output_string(std::str::from_utf8(&digits[start..]).expect("decimal digits are UTF-8"))
}

pub(super) fn handler_name(handler: [u8; 4]) -> &'static str {
   match &handler {
      b"sbtl" => "sbtl",
      b"subt" => "subt",
      b"text" => "text",
      b"clcp" => "clcp",
      _ => unreachable!("only recognized subtitle handlers are indexed"),
   }
}

/// Builds the property map reported for one subtitle track.
pub(super) fn track_properties(
   handler: [u8; 4],
   sample_count: u32,
   cue_count: usize,
) -> Result<HashMap<String, String>> {
   let mut properties = HashMap::new();
   properties
      .try_reserve(3)
      .map_err(|_| MediaParserError::Other("MP4 subtitle property allocation failed".to_owned()))?;
   properties.insert(
      output_string("handler_type")?,
      output_string(handler_name(handler))?,
   );
   properties.insert(
      output_string("sample_count")?,
      decimal_string(u64::from(sample_count))?,
   );
   properties.insert(
      output_string("cue_count")?,
      decimal_string(u64::try_from(cue_count).map_err(|_| {
         MediaParserError::Other("MP4 subtitle cue count is too large".to_owned())
      })?)?,
   );
   Ok(properties)
}

#[cfg(test)]
mod tests {
   use super::*;

   #[test]
   fn decimal_string_matches_the_standard_formatting() {
      for value in [0u64, 7, 10, 1_000, u64::MAX] {
         assert_eq!(decimal_string(value).unwrap(), value.to_string());
      }
   }

   #[test]
   fn track_properties_reports_the_handler_and_both_counts() {
      let properties = track_properties(*b"sbtl", 42, 7).unwrap();

      assert_eq!(properties["handler_type"], "sbtl");
      assert_eq!(properties["sample_count"], "42");
      assert_eq!(properties["cue_count"], "7");
   }
}

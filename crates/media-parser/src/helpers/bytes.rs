//! Byte reading functions for big-endian and little-endian data.

/// Reads a big-endian `u16` from `buf` at `offset`.
///
/// Returns `None` if `offset + 2 > buf.len()`.
#[inline]
pub fn read_u16_be(buf: &[u8], offset: usize) -> Option<u16> {
   let end = offset.checked_add(2)?;
   let bytes: [u8; 2] = buf.get(offset..end)?.try_into().ok()?;
   Some(u16::from_be_bytes(bytes))
}

/// Reads a little-endian `u16` from `buf` at `offset`.
///
/// Returns `None` if `offset + 2 > buf.len()`.
#[inline]
pub fn read_u16_le(buf: &[u8], offset: usize) -> Option<u16> {
   let end = offset.checked_add(2)?;
   let bytes: [u8; 2] = buf.get(offset..end)?.try_into().ok()?;
   Some(u16::from_le_bytes(bytes))
}

/// Reads a big-endian `u32` from `buf` at `offset`.
///
/// Returns `None` if `offset + 4 > buf.len()`.
#[inline]
pub fn read_u32_be(buf: &[u8], offset: usize) -> Option<u32> {
   let end = offset.checked_add(4)?;
   let bytes: [u8; 4] = buf.get(offset..end)?.try_into().ok()?;
   Some(u32::from_be_bytes(bytes))
}

/// Reads a big-endian `u64` from `buf` at `offset`.
///
/// Returns `None` if `offset + 8 > buf.len()`.
#[inline]
pub fn read_u64_be(buf: &[u8], offset: usize) -> Option<u64> {
   let end = offset.checked_add(8)?;
   let bytes: [u8; 8] = buf.get(offset..end)?.try_into().ok()?;
   Some(u64::from_be_bytes(bytes))
}

#[cfg(test)]
mod tests {
   use super::*;

   #[test]
   fn test_read_u16_be() {
      let buf = [0x12, 0x34];
      assert_eq!(read_u16_be(&buf, 0), Some(0x1234));
   }

   #[test]
   fn test_read_u16_le() {
      let buf = [0x34, 0x12];
      assert_eq!(read_u16_le(&buf, 0), Some(0x1234));
   }

   #[test]
   fn test_read_u32_be() {
      let buf = [0x12, 0x34, 0x56, 0x78];
      assert_eq!(read_u32_be(&buf, 0), Some(0x12345678));
   }

   #[test]
   fn test_read_u64_be() {
      let buf = [0x12, 0x34, 0x56, 0x78, 0x9A, 0xBC, 0xDE, 0xF0];
      assert_eq!(read_u64_be(&buf, 0), Some(0x123456789ABCDEF0));
   }

   #[test]
   fn test_read_with_offset() {
      let buf = [0x00, 0x00, 0x12, 0x34, 0x56, 0x78];
      assert_eq!(read_u32_be(&buf, 2), Some(0x12345678));
   }

   #[test]
   fn test_read_out_of_bounds() {
      let buf = [0x12, 0x34];
      assert_eq!(read_u16_be(&buf, 1), None);
      assert_eq!(read_u32_be(&buf, 0), None);
      assert_eq!(read_u64_be(&buf, 0), None);
   }

   #[test]
   fn test_read_with_overflowing_offset_returns_none() {
      let buf = [0u8; 8];

      assert_eq!(read_u16_be(&buf, usize::MAX), None);
      assert_eq!(read_u16_le(&buf, usize::MAX), None);
      assert_eq!(read_u32_be(&buf, usize::MAX), None);
      assert_eq!(read_u64_be(&buf, usize::MAX), None);
   }

   #[test]
   fn test_read_empty_buffer() {
      let buf: [u8; 0] = [];
      assert_eq!(read_u16_be(&buf, 0), None);
      assert_eq!(read_u16_le(&buf, 0), None);
      assert_eq!(read_u32_be(&buf, 0), None);
      assert_eq!(read_u64_be(&buf, 0), None);
   }
}

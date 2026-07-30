//! Common parsing utilities.
//!
//! This module provides generic functions for reading bytes and text encoding/decoding.
//! Format-specific utilities (MP4 atoms, MP3 frames) are in their respective format modules.
//!
//! # Modules
//!
//! - [`bytes`] - Big-endian and little-endian byte reading functions
//! - [`image`] - Image format detection from magic bytes
//! - [`text`] - Text encoding/decoding (UTF-8, UTF-16, Latin-1)

mod bytes;
mod image;
mod text;

pub use bytes::{read_u16_be, read_u16_le, read_u32_be, read_u64_be};
pub use image::detect_image_format;
pub use text::{
   TextEncoding, decode_latin1, decode_text, decode_utf8, decode_utf16_be, decode_utf16_le,
   decode_utf16_with_bom, trim_null_and_whitespace,
};

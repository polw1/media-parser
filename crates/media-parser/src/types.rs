use serde::Serialize;
use std::collections::HashMap;
use std::time::Duration;

/// Single extracted metadata item.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Meta {
   /// Raw metadata key (e.g., "@nam" for MP4, "TIT2" for MP3).
   pub key: String,
   /// Friendly mapped name (e.g., "Title", or "Unknown").
   pub name: String,
   /// Extracted value (UTF‑8, trimmed of null padding).
   pub value: String,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Metadata {
   /// Detected format name (e.g., "MP4/M4A/MOV", "MP3").
   pub format: String,
   /// All metadata items found.
   pub values: Vec<Meta>,
   /// Time units per second for `duration`.
   pub timescale: u32,
   /// Total raw duration in `timescale` units.
   pub duration: u64,
}

impl Metadata {
   /// Get a metadata value by friendly name (e.g., "title", "artist", "album").
   /// Returns the first matching value if found.
   pub fn get(&self, name: &str) -> Option<&str> {
      self
         .values
         .iter()
         .find(|meta| meta.name.to_lowercase() == name.to_lowercase())
         .map(|meta| meta.value.as_str())
   }
}

impl<'a> IntoIterator for &'a Metadata {
   type Item = &'a Meta;
   type IntoIter = std::slice::Iter<'a, Meta>;

   fn into_iter(self) -> Self::IntoIter {
      self.values.iter()
   }
}

/// Base metadata.
#[derive(Debug, Clone, PartialEq)]
pub struct BaseTrackMeta {
   pub id: u32,
   pub codec: String,
   pub language: Option<String>,
   pub timescale: u32, // time units per second
   pub duration: u64,  // raw duration in timescale units
   pub properties: HashMap<String, String>,
}

/// Video track metadata.
#[derive(Debug, Clone, PartialEq)]
pub struct VideoTrackMeta {
   pub base: BaseTrackMeta,
   pub width: u32,
   pub height: u32,
}

/// Audio track metadata.
#[derive(Debug, Clone, PartialEq)]
pub struct AudioTrackMeta {
   pub base: BaseTrackMeta,
   pub channels: u16,
   pub sample_rate: u32,
}

/// Subtitle track metadata.
#[derive(Debug, Clone, PartialEq)]
pub struct SubtitleTrackMeta {
   pub base: BaseTrackMeta,
}

/// Unknown/other track metadata.
#[derive(Debug, Clone, PartialEq)]
pub struct UnknownTrackMeta {
   pub base: BaseTrackMeta,
}

/// Discriminated union of track kinds.
#[derive(Debug, Clone, PartialEq)]
pub enum TrackType {
   Video(VideoTrackMeta),
   Audio(AudioTrackMeta),
   Subtitle(SubtitleTrackMeta),
   Unknown(UnknownTrackMeta),
}

/// Filter for selecting tracks (subtitles).
#[derive(Debug, Clone)]
pub enum TrackFilter {
   /// Filter by track ID
   TrackId(u32),
   /// Filter by language (e.g., "eng", "spa")
   Language(String),
}

/// Timed subtitle cue.
#[derive(Debug, Clone, PartialEq)]
pub struct SubtitleCue {
   pub cue_id: u32,
   pub start_time: Duration,
   pub end_time: Duration,
   pub text: String,
}

/// Subtitle track and its cues.
#[derive(Debug, Clone, PartialEq)]
pub struct SubtitleTrack {
   pub base: BaseTrackMeta,
   pub cues: Vec<SubtitleCue>,
}

/// Supported pixel formats.
#[derive(Debug, Clone, PartialEq)]
pub enum PixelFormat {
   Yuv420p,
   Yuv422p,
   Yuv444p,
   Rgb24,
   Rgba,
}

/// Preview image (Frame) extracted at a timestamp.
#[derive(Debug, Clone, PartialEq)]
pub struct Frame {
   pub track_id: u32,
   pub width: u32,
   pub height: u32,
   pub timestamp: Duration,
   pub format: PixelFormat,
   /// Pixel buffer; layout depends on `format`.
   pub data: Vec<u8>,
   pub strides: Option<Vec<usize>>, // Line sizes for each plane
}

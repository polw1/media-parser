//! # MP3 Format Implementation
//!
//! Parser for MP3 audio files with ID3v2 metadata and duration calculation.
//!
//! ## Module Structure
//!
//! ```text
//! mp3/
//! ├── mod.rs          # Format registration and public API
//! ├── metadata.rs     # ID3v2 tag parsing
//! ├── duration.rs     # Duration calculation (CBR/VBR strategies)
//! ├── frame.rs        # MPEG frame header parsing
//! └── tables.rs       # MPEG lookup tables
//! ```
//!
//! ## ID3v2 Structure
//!
//! ```text
//! [ID3 Header] - 10 bytes
//!   ├── "ID3" marker (3 bytes)
//!   ├── Version (2 bytes)
//!   ├── Flags (1 byte)
//!   └── Size (4 bytes, syncsafe)
//! [ID3 Frames] - variable
//!   ├── TIT2 - Title
//!   ├── TPE1 - Artist
//!   ├── TALB - Album
//!   ├── TYER - Year
//!   ├── TRCK - Track number
//!   └── ...
//! [Audio Data] - MP3 frames
//! ```
//!
//! ## Duration Calculation
//!
//! Duration is calculated using one of these options:
//! - VBR: Parses Xing/VBRI header for exact frame count
//! - CBR: Calculates from file size and bitrate

pub mod duration;
pub mod frame;
pub mod metadata;
pub mod tables;
pub mod tags;

use crate::Result;
use crate::format::{AsyncParser, AsyncTrackParser, Format};
use crate::stream::StreamReader;
use crate::types::{AudioTrackMeta, BaseTrackMeta, Metadata, TrackType};
use std::collections::HashMap;
use std::future::Future;
use std::pin::Pin;

/// MP3 format signature for detection.
pub use crate::format::signatures::MP3 as SIGNATURE;

/// Parser entry point for the registry.
fn parse(reader: &dyn StreamReader) -> Pin<Box<dyn Future<Output = Result<Metadata>> + Send + '_>> {
   Box::pin(parse_mp3(reader))
}

fn parse_tracks(
   reader: &dyn StreamReader,
) -> Pin<Box<dyn Future<Output = Result<Vec<TrackType>>> + Send + '_>> {
   Box::pin(read_tracks(reader))
}

/// MP3 format definition registered in the global table.
pub static FORMAT: Format = Format::new(
   SIGNATURE,
   parse as AsyncParser,
   parse_tracks as AsyncTrackParser,
);

/// Main parsing function.
async fn parse_mp3(reader: &dyn StreamReader) -> Result<Metadata> {
   metadata::read_metadata(reader).await
}

async fn read_tracks(reader: &dyn StreamReader) -> Result<Vec<TrackType>> {
   let (header, offset) = match frame::find_first_frame(reader, 0, frame::MAX_SYNC_SEARCH).await {
      frame::FrameParseResult::Found { header, offset } => (header, offset),
      frame::FrameParseResult::NotFound | frame::FrameParseResult::EndOfData => {
         return Ok(Vec::new());
      }
      frame::FrameParseResult::InvalidHeader { offset } => {
         return Err(crate::errors::MediaParserError::InvalidFormat(format!(
            "invalid MP3 frame header at offset {}",
            offset
         )));
      }
   };

   let duration = duration::calculate_duration(reader, 0).await?;
   let mut properties = HashMap::new();
   properties.insert("offset".to_string(), offset.to_string());
   properties.insert("bitrate_kbps".to_string(), header.bitrate_kbps.to_string());
   properties.insert("mpeg_version".to_string(), format!("{:?}", header.version));
   properties.insert("mpeg_layer".to_string(), format!("{:?}", header.layer));
   properties.insert("channel_mode".to_string(), header.channel_mode.to_string());
   properties.insert(
      "duration_method".to_string(),
      format!("{:?}", duration.method),
   );

   Ok(vec![TrackType::Audio(AudioTrackMeta {
      base: BaseTrackMeta {
         id: 1,
         codec: "mp3".to_string(),
         language: None,
         timescale: 1000,
         duration: duration.millis,
         properties,
      },
      channels: if header.channel_mode == 3 { 1 } else { 2 },
      sample_rate: header.sample_rate_hz,
   })])
}

// Re-export public types
pub use duration::{
   AutoStrategy, CbrStrategy, Duration, DurationMethod, DurationStrategy, VbrHeaderType, VbrInfo,
   VbrStrategy, calculate_duration, calculate_duration_with_strategy, parse_vbr_header,
};
pub use frame::{FrameHeader, FrameParseResult, MAX_SYNC_SEARCH, find_first_frame};
pub use metadata::read_metadata;
pub use tables::{MpegLayer, MpegVersion};
pub use tags::{frame_id_to_key, frame_name};

#[cfg(test)]
mod tests {
   use super::*;
   use async_trait::async_trait;

   struct BytesReader(Vec<u8>);

   #[async_trait]
   impl StreamReader for BytesReader {
      async fn read_at(&self, offset: u64, buf: &mut [u8]) -> Result<usize> {
         let start = usize::try_from(offset)
            .unwrap_or(usize::MAX)
            .min(self.0.len());
         let read = buf.len().min(self.0.len() - start);
         buf[..read].copy_from_slice(&self.0[start..start + read]);
         Ok(read)
      }

      async fn size(&self) -> Result<u64> {
         Ok(self.0.len() as u64)
      }
   }

   fn mp3_frames(channel_mode: u8) -> Vec<u8> {
      const FRAME_SIZE: usize = 417;
      let mut data = vec![0u8; FRAME_SIZE * 2];
      let header = [0xFF, 0xFB, 0x90, channel_mode << 6];
      data[..4].copy_from_slice(&header);
      data[FRAME_SIZE..FRAME_SIZE + 4].copy_from_slice(&header);
      data
   }

   #[tokio::test]
   async fn read_tracks_maps_stereo_channel_mode_to_two_channels() {
      let tracks = read_tracks(&BytesReader(mp3_frames(0))).await.unwrap();

      assert_eq!(tracks.len(), 1);
      let TrackType::Audio(track) = &tracks[0] else {
         panic!("expected audio track");
      };
      assert_eq!(track.base.codec, "mp3");
      assert_eq!(track.channels, 2);
      assert_eq!(track.sample_rate, 44_100);
      assert_eq!(
         track.base.properties.get("channel_mode"),
         Some(&"0".to_string())
      );
   }

   #[tokio::test]
   async fn read_tracks_maps_mono_channel_mode_to_one_channel() {
      let tracks = read_tracks(&BytesReader(mp3_frames(3))).await.unwrap();

      assert_eq!(tracks.len(), 1);
      let TrackType::Audio(track) = &tracks[0] else {
         panic!("expected audio track");
      };
      assert_eq!(track.channels, 1);
      assert_eq!(track.sample_rate, 44_100);
      assert_eq!(
         track.base.properties.get("channel_mode"),
         Some(&"3".to_string())
      );
   }

   #[tokio::test]
   async fn read_tracks_returns_empty_for_empty_input() {
      assert!(
         read_tracks(&BytesReader(Vec::new()))
            .await
            .unwrap()
            .is_empty()
      );
   }

   #[tokio::test]
   async fn read_tracks_returns_empty_for_invalid_frame_header() {
      let data = vec![0xFF, 0xE0, 0, 0, 0, 0, 0, 0];

      assert!(read_tracks(&BytesReader(data)).await.unwrap().is_empty());
   }
}

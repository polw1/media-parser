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
use crate::format::{
   AsyncCoverParser, AsyncFrameParser, AsyncFramesParser, AsyncParser, AsyncTrackParser, Format,
};
use crate::stream::StreamReader;
use crate::types::{AudioTrackMeta, BaseTrackMeta, CoverArt, Frame, Metadata, TrackType};
use std::collections::HashMap;
use std::future::Future;
use std::pin::Pin;
use std::time::Duration as StdDuration;

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

fn parse_cover(
   reader: &dyn StreamReader,
) -> Pin<Box<dyn Future<Output = Result<Option<CoverArt>>> + Send + '_>> {
   Box::pin(metadata::read_cover(reader))
}

fn parse_frame(
   reader: &dyn StreamReader,
   track_id: u32,
   timestamp: StdDuration,
) -> Pin<Box<dyn Future<Output = Result<Frame>> + Send + '_>> {
   Box::pin(read_frame(reader, track_id, timestamp))
}

fn parse_frames<'a>(
   reader: &'a dyn StreamReader,
   track_id: u32,
   timestamps: &'a [StdDuration],
) -> Pin<Box<dyn Future<Output = Result<Vec<Frame>>> + Send + 'a>> {
   Box::pin(read_frames(reader, track_id, timestamps))
}

/// MP3 format definition registered in the global table.
pub static FORMAT: Format = Format::new(
   SIGNATURE,
   parse as AsyncParser,
   parse_tracks as AsyncTrackParser,
   parse_cover as AsyncCoverParser,
   parse_frame as AsyncFrameParser,
   parse_frames as AsyncFramesParser,
);

/// Main parsing function.
async fn parse_mp3(reader: &dyn StreamReader) -> Result<Metadata> {
   metadata::read_metadata(reader).await
}

async fn read_tracks(reader: &dyn StreamReader) -> Result<Vec<TrackType>> {
   let id3_end = metadata::read_id3_end(reader).await?;
   let (header, offset) =
      match frame::find_first_frame(reader, id3_end, frame::MAX_SYNC_SEARCH).await? {
         frame::FrameParseResult::Found { header, offset } => (header, offset),
         frame::FrameParseResult::NotFound | frame::FrameParseResult::EndOfData => {
            return Ok(Vec::new());
         }
      };

   let duration = duration::calculate_duration_from_frame(reader, &header, offset).await?;
   let mut properties = HashMap::new();
   properties.insert("offset".to_string(), offset.to_string());
   properties.insert("bitrate_kbps".to_string(), header.bitrate_kbps.to_string());
   properties.insert("mpeg_version".to_string(), header.version.to_string());
   properties.insert("mpeg_layer".to_string(), header.layer.to_string());
   properties.insert("channel_mode".to_string(), header.channel_mode.to_string());
   properties.insert("duration_method".to_string(), duration.method.to_string());

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

async fn read_frame(
   _reader: &dyn StreamReader,
   _track_id: u32,
   _timestamp: StdDuration,
) -> Result<Frame> {
   Err(crate::errors::MediaParserError::UnsupportedCodec(
      "MP3 does not contain video frames".to_string(),
   ))
}

async fn read_frames(
   reader: &dyn StreamReader,
   track_id: u32,
   timestamps: &[StdDuration],
) -> Result<Vec<Frame>> {
   let Some(timestamp) = timestamps.first() else {
      return Ok(Vec::new());
   };
   read_frame(reader, track_id, *timestamp)
      .await
      .map(|frame| vec![frame])
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

   struct FailingReader;

   #[async_trait]
   impl StreamReader for BytesReader {
      async fn read_at(&self, offset: u64, buf: &mut [u8]) -> crate::Result<usize> {
         let start = usize::try_from(offset)
            .unwrap_or(usize::MAX)
            .min(self.0.len());
         let read = buf.len().min(self.0.len() - start);
         buf[..read].copy_from_slice(&self.0[start..start + read]);
         Ok(read)
      }

      async fn size(&self) -> crate::Result<u64> {
         Ok(self.0.len() as u64)
      }
   }

   #[async_trait]
   impl StreamReader for FailingReader {
      async fn read_at(&self, _: u64, _: &mut [u8]) -> crate::Result<usize> {
         Err(crate::errors::MediaParserError::Other(
            "read failure".into(),
         ))
      }

      async fn size(&self) -> crate::Result<u64> {
         Ok(0)
      }
   }

   fn mp3_with_id3(tag_size: usize, frame_count: usize) -> Vec<u8> {
      const FRAME_SIZE: usize = 417;
      let audio_start = 10 + tag_size;
      let mut data = vec![0; audio_start + FRAME_SIZE * frame_count];
      let syncsafe_size = tag_size as u32;

      data[..10].copy_from_slice(&[
         b'I',
         b'D',
         b'3',
         4,
         0,
         0,
         ((syncsafe_size >> 21) & 0x7f) as u8,
         ((syncsafe_size >> 14) & 0x7f) as u8,
         ((syncsafe_size >> 7) & 0x7f) as u8,
         (syncsafe_size & 0x7f) as u8,
      ]);

      for index in 0..frame_count {
         let offset = audio_start + FRAME_SIZE * index;
         data[offset..offset + 4].copy_from_slice(&[0xff, 0xfb, 0x90, 0x00]);
      }

      data
   }

   #[tokio::test]
   async fn read_tracks_returns_empty_for_end_of_data() {
      assert!(
         read_tracks(&BytesReader(Vec::new()))
            .await
            .unwrap()
            .is_empty()
      );
   }

   #[tokio::test]
   async fn read_tracks_returns_empty_when_no_frame_is_found() {
      let data = vec![0; frame::MAX_SYNC_SEARCH as usize];

      assert!(read_tracks(&BytesReader(data)).await.unwrap().is_empty());
   }

   #[tokio::test]
   async fn read_tracks_skips_large_id3v2_tag() {
      let tag_size = frame::MAX_SYNC_SEARCH as usize + 1;
      let audio_start = 10 + tag_size;

      let tracks = read_tracks(&BytesReader(mp3_with_id3(tag_size, 2)))
         .await
         .unwrap();

      assert_eq!(tracks.len(), 1);
      let TrackType::Audio(track) = &tracks[0] else {
         panic!("expected an audio track");
      };
      assert_eq!(
         track.base.properties.get("offset"),
         Some(&audio_start.to_string())
      );
      assert_eq!(track.base.duration, 52);
      assert_eq!(track.channels, 2);
      assert_eq!(track.sample_rate, 44_100);
   }

   #[tokio::test]
   async fn read_tracks_and_metadata_reject_single_unconfirmed_frame_after_id3v2_tag() {
      let tag_size = 2 * 1024;
      let reader = BytesReader(mp3_with_id3(tag_size, 1));

      let tracks = read_tracks(&reader).await.unwrap();
      let metadata = metadata::read_metadata(&reader).await.unwrap();

      assert!(tracks.is_empty());
      assert_eq!(metadata.duration, 0);
   }

   #[tokio::test]
   async fn read_tracks_propagates_reader_errors() {
      assert!(matches!(
         read_tracks(&FailingReader).await,
         Err(crate::errors::MediaParserError::Other(message)) if message == "read failure"
      ));
   }
}

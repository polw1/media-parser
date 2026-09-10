use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;
use tauri::{State, command};

use media_parser::{
   BaseTrackMeta, Frame, JpegQuality, MediaParser, Metadata, StreamReader, TrackType,
   format::mp4::{MAX_THUMBNAIL_OUTPUTS, ThumbnailIndex, ThumbnailOptions, ThumbnailSize},
};

use crate::Result;
use crate::envelope::{cover_envelope, encode_thumbnail_envelope, run_envelope_task};
use crate::session_cache::SessionPool;
use crate::source::{
   DefaultHeaders, MediaSourceKey, SESSION_REAPER_INTERVAL, open_reader, session_expiration,
   source_key,
};

const MAX_THUMBNAIL_SESSIONS: usize = 8;
const MAX_THUMBNAIL_OUTPUT_BYTES: usize = 256 * 1024 * 1024;

#[derive(Clone, PartialEq, Eq, Hash)]
struct ThumbnailSessionKey {
   source: MediaSourceKey,
   track_id: u32,
}

struct ThumbnailSession {
   reader: Arc<dyn StreamReader>,
   index: Arc<ThumbnailIndex>,
}

pub(crate) struct ThumbnailSessions {
   pool: SessionPool<ThumbnailSessionKey, ThumbnailSession>,
}

impl Default for ThumbnailSessions {
   fn default() -> Self {
      Self {
         pool: SessionPool::new(MAX_THUMBNAIL_SESSIONS, SESSION_REAPER_INTERVAL),
      }
   }
}

async fn thumbnail_session(
   sessions: &ThumbnailSessions,
   source: &str,
   headers: Option<&HashMap<String, String>>,
   track_id: u32,
) -> Result<Arc<ThumbnailSession>> {
   let media_source = source_key(source, headers).await;
   let expiration = session_expiration(&media_source);
   let key = ThumbnailSessionKey {
      source: media_source,
      track_id,
   };
   sessions
      .pool
      .get_or_try_build(key, expiration, || async {
         let reader = open_reader(source, headers).await?;
         let index = Arc::new(ThumbnailIndex::read(reader.as_ref(), track_id).await?);
         Ok(ThumbnailSession { reader, index })
      })
      .await
}

async fn thumbnail_frames(
   sessions: &ThumbnailSessions,
   source: &str,
   timestamps: &[Duration],
   track_id: u32,
   accurate: bool,
   headers: Option<&HashMap<String, String>>,
   options: ThumbnailOptions,
) -> Result<Vec<Frame>> {
   if timestamps.is_empty() {
      return Ok(Vec::new());
   }
   let session = thumbnail_session(sessions, source, headers, track_id).await?;
   if accurate {
      session
         .index
         .frames(session.reader.as_ref(), timestamps, options)
         .await
         .map_err(Into::into)
   } else {
      session
         .index
         .keyframes(session.reader.as_ref(), timestamps, options)
         .await
         .map_err(Into::into)
   }
}

/// Extract metadata from a media file (local path or URL).
///
/// # Arguments
/// * `source` - Absolute path to a local file or URL of a remote media file
/// * `headers` - Optional custom HTTP headers (only used for URLs, e.g., for authentication)
///
/// # Returns
/// Metadata containing duration, timescale, and tags (title, artist, etc.)
#[command]
pub(crate) async fn get_metadata(
   source: String,
   headers: Option<HashMap<String, String>>,
   defaults: State<'_, DefaultHeaders>,
) -> Result<Metadata> {
   let headers = defaults.merge(&source, headers);
   let reader = open_reader(&source, headers.as_ref()).await?;
   MediaParser::new(reader.as_ref())
      .metadata()
      .await
      .map_err(Into::into)
}

/// Extract track information from a media file (local path or URL).
#[command]
pub(crate) async fn get_tracks(
   source: String,
   headers: Option<HashMap<String, String>>,
   defaults: State<'_, DefaultHeaders>,
) -> Result<Vec<TrackInfo>> {
   let headers = defaults.merge(&source, headers);
   let reader = open_reader(&source, headers.as_ref()).await?;
   let tracks = MediaParser::new(reader.as_ref())
      .tracks()
      .await
      .map_err(crate::Error::from)?;

   Ok(tracks.into_iter().map(TrackInfo::from).collect())
}

/// Extract embedded cover artwork from a media file (local path or URL).
#[command]
pub(crate) async fn get_cover(
   source: String,
   headers: Option<HashMap<String, String>>,
   defaults: State<'_, DefaultHeaders>,
) -> Result<tauri::ipc::Response> {
   let headers = defaults.merge(&source, headers);
   let reader = open_reader(&source, headers.as_ref()).await?;
   let cover = MediaParser::new(reader.as_ref())
      .cover()
      .await
      .map_err(crate::Error::from)?;

   Ok(tauri::ipc::Response::new(cover_envelope(cover)?))
}

/// Extract thumbnails from a video track at millisecond timestamps.
#[command]
#[allow(clippy::too_many_arguments)] // Tauri exposes each command field as a top-level IPC argument.
pub(crate) async fn get_thumbnails(
   source: String,
   timestamps: Vec<u64>,
   track_id: Option<u32>,
   accurate: Option<bool>,
   quality: Option<u8>,
   max_width: Option<u32>,
   max_height: Option<u32>,
   headers: Option<HashMap<String, String>>,
   sessions: State<'_, ThumbnailSessions>,
   defaults: State<'_, DefaultHeaders>,
) -> Result<tauri::ipc::Response> {
   let headers = defaults.merge(&source, headers);
   let (unique_timestamps, order) = prepare_thumbnail_timestamps(&timestamps)?;
   let options = thumbnail_options(quality, max_width, max_height)?;
   let frames = thumbnail_frames(
      &sessions,
      &source,
      &unique_timestamps,
      track_id.unwrap_or(0),
      accurate.unwrap_or(false),
      headers.as_ref(),
      options,
   )
   .await?;
   let envelope = run_envelope_task("thumbnail", move || {
      encode_thumbnail_envelope(&frames, &order, MAX_THUMBNAIL_OUTPUT_BYTES)
   })
   .await?;
   Ok(tauri::ipc::Response::new(envelope))
}

/// Validates the caller-supplied JPEG quality, if any, against the encoder's
/// 1-100 range. `None` keeps the thumbnail-grade default.
fn thumbnail_options(
   quality: Option<u8>,
   max_width: Option<u32>,
   max_height: Option<u32>,
) -> Result<ThumbnailOptions> {
   let quality = quality
      .map(|quality| {
         JpegQuality::new(quality).ok_or_else(|| {
            crate::Error::Custom(format!(
               "thumbnail quality must be between 1 and 100, got {quality}"
            ))
         })
      })
      .transpose()?
      .unwrap_or_default();
   let size = match (max_width, max_height) {
      (None, None) => ThumbnailSize::default(),
      (max_width, max_height) => ThumbnailSize::new(
         max_width.unwrap_or(u16::MAX.into()),
         max_height.unwrap_or(u16::MAX.into()),
      )
      .ok_or_else(|| {
         crate::Error::Custom(
            "thumbnail dimensions must be integers between 1 and 65535".to_string(),
         )
      })?,
   };
   Ok(ThumbnailOptions {
      quality,
      size,
      max_output_bytes: Some(MAX_THUMBNAIL_OUTPUT_BYTES),
   })
}

fn thumbnail_durations(timestamps_ms: &[u64]) -> Vec<Duration> {
   timestamps_ms
      .iter()
      .copied()
      .map(Duration::from_millis)
      .collect()
}

/// Validates the requested output count before allocating converted or
/// deduplicated collections, then preserves first-seen timestamp order.
fn prepare_thumbnail_timestamps(timestamps_ms: &[u64]) -> Result<(Vec<Duration>, Vec<usize>)> {
   if timestamps_ms.len() > MAX_THUMBNAIL_OUTPUTS {
      return Err(crate::Error::Custom(format!(
         "too many thumbnail timestamps: {}",
         timestamps_ms.len()
      )));
   }

   let mut unique_timestamps = Vec::new();
   let mut index_by_timestamp = HashMap::new();
   let mut order = Vec::new();
   unique_timestamps
      .try_reserve_exact(timestamps_ms.len())
      .map_err(|_| crate::Error::Custom("too many thumbnail timestamps".to_string()))?;
   index_by_timestamp
      .try_reserve(timestamps_ms.len())
      .map_err(|_| crate::Error::Custom("too many thumbnail timestamps".to_string()))?;
   order
      .try_reserve_exact(timestamps_ms.len())
      .map_err(|_| crate::Error::Custom("too many thumbnail timestamps".to_string()))?;

   for timestamp in thumbnail_durations(timestamps_ms) {
      let next_index = unique_timestamps.len();
      let index = *index_by_timestamp.entry(timestamp).or_insert(next_index);
      if index == next_index {
         unique_timestamps.push(timestamp);
      }
      order.push(index);
   }
   Ok((unique_timestamps, order))
}

#[derive(serde::Serialize, Clone, Debug)]
#[serde(rename_all = "camelCase")]
pub struct TrackInfo {
   pub kind: String,
   pub id: u32,
   pub codec: String,
   #[serde(skip_serializing_if = "Option::is_none")]
   pub language: Option<String>,
   pub timescale: u32,
   pub duration: u64,
   pub properties: HashMap<String, String>,
   #[serde(skip_serializing_if = "Option::is_none")]
   pub width: Option<u32>,
   #[serde(skip_serializing_if = "Option::is_none")]
   pub height: Option<u32>,
   #[serde(skip_serializing_if = "Option::is_none")]
   pub channels: Option<u16>,
   #[serde(skip_serializing_if = "Option::is_none")]
   pub sample_rate: Option<u32>,
}

impl TrackInfo {
   fn from_base(kind: &'static str, base: BaseTrackMeta) -> Self {
      Self {
         kind: kind.to_string(),
         id: base.id,
         codec: base.codec,
         language: base.language,
         timescale: base.timescale,
         duration: base.duration,
         properties: base.properties,
         width: None,
         height: None,
         channels: None,
         sample_rate: None,
      }
   }
}

impl From<TrackType> for TrackInfo {
   fn from(track: TrackType) -> Self {
      match track {
         TrackType::Video(video) => Self {
            width: Some(video.width),
            height: Some(video.height),
            ..Self::from_base("video", video.base)
         },
         TrackType::Audio(audio) => Self {
            channels: Some(audio.channels),
            sample_rate: Some(audio.sample_rate),
            ..Self::from_base("audio", audio.base)
         },
         TrackType::Subtitle(subtitle) => Self::from_base("subtitle", subtitle.base),
         TrackType::Unknown(unknown) => Self::from_base("unknown", unknown.base),
      }
   }
}

#[cfg(test)]
mod tests {
   use super::*;
   use media_parser::{AudioTrackMeta, SubtitleTrackMeta, UnknownTrackMeta, VideoTrackMeta};

   fn base_track(id: u32, codec: &str) -> BaseTrackMeta {
      BaseTrackMeta {
         id,
         codec: codec.to_string(),
         language: None,
         timescale: 1_000,
         duration: 2_000,
         properties: HashMap::new(),
      }
   }

   fn video_fixture_source() -> String {
      std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
         .join("crates/media-parser/tests/fixtures/multitrack_video.mp4")
         .to_string_lossy()
         .into_owned()
   }

   #[tokio::test]
   async fn thumbnail_session_key_distinguishes_track_ids() {
      // The fixture's track 1 is video and track 2 is audio, so the second
      // request must build its own index and fail. Dropping `track_id` from the
      // key would hand it the cached video session instead.
      let sessions = ThumbnailSessions::default();
      let source = video_fixture_source();

      thumbnail_session(&sessions, &source, None, 1)
         .await
         .expect("the video track should build a session");
      let Err(error) = thumbnail_session(&sessions, &source, None, 2).await else {
         panic!("the audio track must not reuse the video session");
      };

      assert!(
         matches!(
            error,
            crate::Error::MediaParser(media_parser::MediaParserError::TrackNotFound(2))
         ),
         "unexpected error for the audio track: {error:?}"
      );
   }

   #[test]
   fn omitted_thumbnail_quality_keeps_the_default() {
      let options = thumbnail_options(None, None, None).expect("omitted options are valid");

      assert_eq!(options.quality, JpegQuality::DEFAULT);
      assert_eq!(options.size, ThumbnailSize::default());
      assert_eq!(options.max_output_bytes, Some(MAX_THUMBNAIL_OUTPUT_BYTES));
   }

   #[test]
   fn thumbnail_dimensions_are_validated_and_default_independently() {
      assert_eq!(
         thumbnail_options(None, Some(640), Some(360))
            .expect("valid dimensions")
            .size,
         ThumbnailSize::new(640, 360).expect("valid size")
      );
      assert_eq!(
         thumbnail_options(None, Some(640), None)
            .expect("one dimension leaves the other unconstrained")
            .size,
         ThumbnailSize::new(640, u16::MAX.into()).expect("valid one-axis bounds")
      );
      assert!(thumbnail_options(None, Some(0), None).is_err());
      assert!(thumbnail_options(None, None, Some(65_536)).is_err());
   }

   #[test]
   fn thumbnail_quality_is_rejected_outside_the_encoder_range() {
      assert_eq!(
         thumbnail_options(Some(80), None, None)
            .expect("80 is in range")
            .quality
            .get(),
         80
      );

      for quality in [0u8, 101, 255] {
         let error = thumbnail_options(Some(quality), None, None)
            .expect_err("quality outside 1-100 must not reach the encoder")
            .to_string();

         assert!(
            error.contains("between 1 and 100"),
            "unexpected error for quality {quality}: {error}"
         );
      }
   }

   #[test]
   fn thumbnail_durations_use_milliseconds() {
      assert_eq!(
         thumbnail_durations(&[0, 250, 1_000]),
         vec![
            std::time::Duration::ZERO,
            std::time::Duration::from_millis(250),
            std::time::Duration::from_secs(1),
         ]
      );
   }

   #[test]
   fn prepares_unique_thumbnail_timestamps_and_request_order() {
      let (timestamps, order) = prepare_thumbnail_timestamps(&[0, 250, 0])
         .expect("three thumbnail outputs are within the limit");

      assert_eq!(timestamps, vec![Duration::ZERO, Duration::from_millis(250)]);
      assert_eq!(order, vec![0, 1, 0]);
   }

   #[test]
   fn thumbnail_request_count_is_checked_before_deduplication() {
      let timestamps = vec![0; MAX_THUMBNAIL_OUTPUTS + 1];

      let error = prepare_thumbnail_timestamps(&timestamps)
         .expect_err("repeated timestamps still represent distinct outputs")
         .to_string();

      assert!(error.contains("too many thumbnail timestamps"));
   }

   #[test]
   fn thumbnail_request_accepts_the_output_count_boundary() {
      let timestamps = vec![0; MAX_THUMBNAIL_OUTPUTS];
      let (_, order) = prepare_thumbnail_timestamps(&timestamps)
         .expect("the documented output boundary should be accepted");

      assert_eq!(order.len(), MAX_THUMBNAIL_OUTPUTS);
   }

   #[tokio::test]
   async fn accurate_thumbnail_mode_returns_the_requested_frame_timestamp() {
      let sessions = ThumbnailSessions::default();
      let frames = thumbnail_frames(
         &sessions,
         &video_fixture_source(),
         &[Duration::from_millis(100)],
         0,
         true,
         None,
         ThumbnailOptions::default(),
      )
      .await
      .expect("accurate thumbnail should decode");

      assert_eq!(frames.len(), 1);
      assert_eq!(frames[0].timestamp, Duration::from_millis(100));
   }

   #[tokio::test]
   async fn fast_thumbnail_mode_returns_the_actual_keyframe_timestamp() {
      let sessions = ThumbnailSessions::default();
      let frames = thumbnail_frames(
         &sessions,
         &video_fixture_source(),
         &[Duration::from_millis(200)],
         0,
         false,
         None,
         ThumbnailOptions::default(),
      )
      .await
      .expect("fast thumbnail should decode");

      assert_eq!(frames.len(), 1);
      assert_eq!(frames[0].timestamp, Duration::ZERO);
   }

   #[tokio::test]
   async fn repeated_thumbnail_requests_reuse_the_same_session() {
      let sessions = ThumbnailSessions::default();
      let source = video_fixture_source();

      let first = thumbnail_session(&sessions, &source, None, 0)
         .await
         .expect("first session should build");
      let second = thumbnail_session(&sessions, &source, None, 0)
         .await
         .expect("second session should reuse the cache");

      assert!(Arc::ptr_eq(&first, &second));
   }

   #[tokio::test]
   async fn concurrent_requests_for_a_cold_source_build_a_single_session() {
      let sessions = Arc::new(ThumbnailSessions::default());
      let source = video_fixture_source();

      let first_sessions = Arc::clone(&sessions);
      let first_source = source.clone();
      let first =
         tokio::spawn(
            async move { thumbnail_session(&first_sessions, &first_source, None, 0).await },
         );
      let second_sessions = Arc::clone(&sessions);
      let second_source = source.clone();
      let second =
         tokio::spawn(
            async move { thumbnail_session(&second_sessions, &second_source, None, 0).await },
         );
      let third_sessions = Arc::clone(&sessions);
      let third =
         tokio::spawn(async move { thumbnail_session(&third_sessions, &source, None, 0).await });

      let (first, second, third) = tokio::time::timeout(Duration::from_secs(10), async {
         tokio::join!(first, second, third)
      })
      .await
      .expect("concurrent thumbnail requests should not hang");
      let first = first
         .expect("first task should complete")
         .expect("first concurrent session should build");
      let second = second
         .expect("second task should complete")
         .expect("second concurrent session should reuse the build");
      let third = third
         .expect("third task should complete")
         .expect("third concurrent session should reuse the build");

      assert!(Arc::ptr_eq(&first, &second));
      assert!(Arc::ptr_eq(&first, &third));
   }

   #[tokio::test]
   async fn empty_thumbnail_request_does_not_open_the_source() {
      let sessions = ThumbnailSessions::default();
      let frames = thumbnail_frames(
         &sessions,
         "/file/that/does/not/exist.mp4",
         &[],
         0,
         false,
         None,
         ThumbnailOptions::default(),
      )
      .await
      .expect("empty thumbnail request should not need a source");

      assert!(frames.is_empty());
   }

   #[test]
   fn serializes_track_type_contract() {
      let tracks = [
         TrackType::Video(VideoTrackMeta {
            base: base_track(1, "avc1"),
            width: 1_920,
            height: 1_080,
         }),
         TrackType::Audio(AudioTrackMeta {
            base: base_track(2, "mp4a"),
            channels: 2,
            sample_rate: 48_000,
         }),
         TrackType::Subtitle(SubtitleTrackMeta {
            base: base_track(3, "tx3g"),
         }),
         TrackType::Unknown(UnknownTrackMeta {
            base: base_track(4, "meta"),
         }),
      ];

      let serialized = tracks
         .into_iter()
         .map(|track| serde_json::to_value(TrackInfo::from(track)).expect("track should serialize"))
         .collect::<Vec<_>>();

      assert_eq!(
         serialized,
         vec![
            serde_json::json!({
               "kind": "video",
               "id": 1,
               "codec": "avc1",
               "timescale": 1_000,
               "duration": 2_000,
               "properties": {},
               "width": 1_920,
               "height": 1_080,
            }),
            serde_json::json!({
               "kind": "audio",
               "id": 2,
               "codec": "mp4a",
               "timescale": 1_000,
               "duration": 2_000,
               "properties": {},
               "channels": 2,
               "sampleRate": 48_000,
            }),
            serde_json::json!({
               "kind": "subtitle",
               "id": 3,
               "codec": "tx3g",
               "timescale": 1_000,
               "duration": 2_000,
               "properties": {},
            }),
            serde_json::json!({
               "kind": "unknown",
               "id": 4,
               "codec": "meta",
               "timescale": 1_000,
               "duration": 2_000,
               "properties": {},
            }),
         ]
      );
   }

   #[test]
   fn serializes_track_info_optional_fields_as_omitted() {
      let track = TrackInfo {
         kind: "subtitle".to_string(),
         id: 1,
         codec: "tx3g".to_string(),
         language: None,
         timescale: 1_000,
         duration: 2_000,
         properties: HashMap::new(),
         width: None,
         height: None,
         channels: None,
         sample_rate: None,
      };

      let value = serde_json::to_value(track).expect("track should serialize");
      let object = value.as_object().expect("track should serialize as object");

      assert!(!object.contains_key("language"));
      assert!(!object.contains_key("width"));
      assert!(!object.contains_key("height"));
      assert!(!object.contains_key("channels"));
      assert!(!object.contains_key("sampleRate"));
   }
}

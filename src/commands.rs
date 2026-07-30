use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use tauri::{State, command};
use url::Url;

use media_parser::{
   BaseTrackMeta, CoverArt, FileStreamReader, Frame, HttpStreamReader, MediaParser, Metadata,
   StreamReader, TrackType, format::mp4::ThumbnailIndex,
};

use crate::Result;

const MAX_THUMBNAIL_SESSIONS: usize = 8;
const REMOTE_THUMBNAIL_SESSION_TTL: Duration = Duration::from_secs(5 * 60);

struct SessionCache<K, V> {
   capacity: usize,
   entries: Vec<SessionCacheEntry<K, V>>,
}

struct SessionCacheEntry<K, V> {
   key: K,
   value: V,
   expires_at: Option<Instant>,
}

impl<K: PartialEq, V: Clone> SessionCache<K, V> {
   fn new(capacity: usize) -> Self {
      Self {
         capacity: capacity.max(1),
         entries: Vec::new(),
      }
   }

   fn get(&mut self, key: &K, now: Instant) -> Option<V> {
      self
         .entries
         .retain(|entry| entry.expires_at.is_none_or(|deadline| deadline > now));
      let index = self.entries.iter().position(|entry| &entry.key == key)?;
      let entry = self.entries.remove(index);
      let value = entry.value.clone();
      self.entries.push(entry);
      Some(value)
   }

   fn insert(&mut self, key: K, value: V, expires_at: Option<Instant>) {
      self.entries.retain(|entry| entry.key != key);
      if self.entries.len() >= self.capacity {
         self.entries.remove(0);
      }
      self.entries.push(SessionCacheEntry {
         key,
         value,
         expires_at,
      });
   }
}

#[derive(Clone, PartialEq, Eq)]
struct ThumbnailSessionKey {
   source: String,
   headers: Vec<(String, String)>,
   track_id: u32,
   local_version: Option<LocalSourceVersion>,
}

#[derive(Clone, PartialEq, Eq)]
struct LocalSourceVersion {
   length: u64,
   modified_nanos: Option<u128>,
}

struct ThumbnailSession {
   reader: Arc<dyn StreamReader>,
   index: Arc<ThumbnailIndex>,
}

pub(crate) struct ThumbnailSessions {
   cache: Mutex<SessionCache<ThumbnailSessionKey, Arc<ThumbnailSession>>>,
}

impl Default for ThumbnailSessions {
   fn default() -> Self {
      Self {
         cache: Mutex::new(SessionCache::new(MAX_THUMBNAIL_SESSIONS)),
      }
   }
}

fn is_http_source(source: &str) -> bool {
   Url::parse(source)
      .map(|url| matches!(url.scheme(), "http" | "https"))
      .unwrap_or(false)
}

fn thumbnail_session_key(
   source: &str,
   headers: Option<&HashMap<String, String>>,
   track_id: u32,
) -> ThumbnailSessionKey {
   let is_remote = is_http_source(source);
   let mut headers = if is_remote {
      headers
         .into_iter()
         .flat_map(HashMap::iter)
         .map(|(name, value)| (name.to_ascii_lowercase(), value.clone()))
         .collect::<Vec<_>>()
   } else {
      Vec::new()
   };
   headers.sort_unstable();
   let local_version = (!is_remote)
      .then(|| std::fs::metadata(source).ok())
      .flatten()
      .map(|metadata| LocalSourceVersion {
         length: metadata.len(),
         modified_nanos: metadata
            .modified()
            .ok()
            .and_then(|modified| modified.duration_since(std::time::UNIX_EPOCH).ok())
            .map(|elapsed| elapsed.as_nanos()),
      });
   ThumbnailSessionKey {
      source: source.to_string(),
      headers,
      track_id,
      local_version,
   }
}

fn thumbnail_session_expiration(source: &str, now: Instant) -> Option<Instant> {
   is_http_source(source)
      .then(|| now.checked_add(REMOTE_THUMBNAIL_SESSION_TTL))
      .flatten()
}

async fn thumbnail_session(
   sessions: &ThumbnailSessions,
   source: &str,
   headers: Option<&HashMap<String, String>>,
   track_id: u32,
) -> Result<Arc<ThumbnailSession>> {
   let key = thumbnail_session_key(source, headers, track_id);
   let now = Instant::now();
   if let Some(session) = sessions
      .cache
      .lock()
      .map_err(|_| crate::Error::Custom("thumbnail session cache is unavailable".to_string()))?
      .get(&key, now)
   {
      return Ok(session);
   }

   let reader: Arc<dyn StreamReader> = if is_http_source(source) {
      match headers {
         Some(headers) => Arc::new(HttpStreamReader::with_headers(source, headers.clone()).await?),
         None => Arc::new(HttpStreamReader::new(source).await?),
      }
   } else {
      Arc::new(FileStreamReader::new(source)?)
   };
   let index = Arc::new(ThumbnailIndex::read(reader.as_ref(), track_id).await?);
   let session = Arc::new(ThumbnailSession { reader, index });
   let expires_at = thumbnail_session_expiration(source, now);
   sessions
      .cache
      .lock()
      .map_err(|_| crate::Error::Custom("thumbnail session cache is unavailable".to_string()))?
      .insert(key, Arc::clone(&session), expires_at);
   Ok(session)
}

async fn thumbnail_frames(
   sessions: &ThumbnailSessions,
   source: &str,
   timestamps: &[Duration],
   track_id: u32,
   accurate: bool,
   headers: Option<&HashMap<String, String>>,
) -> Result<Vec<Frame>> {
   if timestamps.is_empty() {
      return Ok(Vec::new());
   }
   let session = thumbnail_session(sessions, source, headers, track_id).await?;
   if accurate {
      session
         .index
         .frames(session.reader.as_ref(), timestamps)
         .await
         .map_err(Into::into)
   } else {
      session
         .index
         .keyframes(session.reader.as_ref(), timestamps)
         .await
         .map_err(Into::into)
   }
}

/// Helper macro to handle stream instantiation based on the source (URL or File).
macro_rules! with_reader {
   ($source:expr, $headers:expr, |$reader:ident| $body:expr) => {{
      if is_http_source(&$source) {
         let reader = match $headers {
            Some(h) => HttpStreamReader::with_headers(&$source, h).await?,
            None => HttpStreamReader::new(&$source).await?,
         };
         let $reader = reader;
         $body
      } else {
         let reader = FileStreamReader::new(&$source)?;
         let $reader = reader;
         $body
      }
   }};
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
) -> Result<Metadata> {
   with_reader!(source, headers, |reader| {
      let parser = MediaParser::new(reader);
      parser.metadata().await.map_err(Into::into)
   })
}

/// Extract track information from a media file (local path or URL).
#[command]
pub(crate) async fn get_tracks(
   source: String,
   headers: Option<HashMap<String, String>>,
) -> Result<Vec<TrackInfo>> {
   let tracks = with_reader!(source, headers, |reader| {
      let parser = MediaParser::new(reader);
      parser.tracks().await
   })?;

   Ok(tracks.into_iter().map(TrackInfo::from).collect())
}

/// Extract embedded cover artwork from a media file (local path or URL).
#[command]
pub(crate) async fn get_cover(
   source: String,
   headers: Option<HashMap<String, String>>,
) -> Result<Option<CoverInfo>> {
   let cover = with_reader!(source, headers, |reader| {
      let parser = MediaParser::new(reader);
      parser.cover().await
   })?;

   Ok(cover.map(CoverInfo::from))
}

/// Extract thumbnails from a video track at millisecond timestamps.
#[command]
pub(crate) async fn get_thumbnails(
   source: String,
   timestamps: Vec<u64>,
   track_id: Option<u32>,
   accurate: Option<bool>,
   headers: Option<HashMap<String, String>>,
   sessions: State<'_, ThumbnailSessions>,
) -> Result<tauri::ipc::Response> {
   let timestamps = thumbnail_durations(&timestamps);
   let frames = thumbnail_frames(
      &sessions,
      &source,
      &timestamps,
      track_id.unwrap_or(0),
      accurate.unwrap_or(false),
      headers.as_ref(),
   )
   .await?;
   Ok(tauri::ipc::Response::new(encode_thumbnail_envelope(
      frames,
   )?))
}

#[derive(serde::Serialize, Clone, Debug)]
#[serde(rename_all = "camelCase")]
pub struct CoverInfo {
   pub format: String,
   pub mime_type: String,
   pub data: Vec<u8>,
}

impl From<CoverArt> for CoverInfo {
   fn from(cover: CoverArt) -> Self {
      Self {
         format: cover.format.label().to_string(),
         mime_type: cover.mime_type,
         data: cover.data,
      }
   }
}

fn thumbnail_durations(timestamps_ms: &[u64]) -> Vec<Duration> {
   timestamps_ms
      .iter()
      .copied()
      .map(Duration::from_millis)
      .collect()
}

#[derive(serde::Serialize)]
#[serde(rename_all = "camelCase")]
struct ThumbnailEnvelopeEntry {
   track_id: u32,
   width: u32,
   height: u32,
   timestamp_sec: f64,
   format: String,
   mime_type: String,
   offset: usize,
   length: usize,
}

fn encode_thumbnail_envelope(frames: Vec<Frame>) -> Result<Vec<u8>> {
   let mut entries = Vec::new();
   entries
      .try_reserve_exact(frames.len())
      .map_err(|_| crate::Error::Custom("too many thumbnail entries".to_string()))?;
   let mut payload_len = 0usize;
   for frame in &frames {
      entries.push(ThumbnailEnvelopeEntry {
         track_id: frame.track_id,
         width: frame.width,
         height: frame.height,
         timestamp_sec: frame.timestamp.as_secs_f64(),
         format: frame.format.label().to_string(),
         mime_type: frame.format.mime_type().to_string(),
         offset: payload_len,
         length: frame.data.len(),
      });
      payload_len = payload_len
         .checked_add(frame.data.len())
         .ok_or_else(|| crate::Error::Custom("thumbnail payload is too large".to_string()))?;
   }

   let header = serde_json::to_vec(&entries)
      .map_err(|error| crate::Error::Custom(format!("could not encode thumbnails: {error}")))?;
   let header_len = u32::try_from(header.len())
      .map_err(|_| crate::Error::Custom("thumbnail header is too large".to_string()))?;
   let envelope_len = 4usize
      .checked_add(header.len())
      .and_then(|length| length.checked_add(payload_len))
      .ok_or_else(|| crate::Error::Custom("thumbnail response is too large".to_string()))?;
   let mut envelope = Vec::new();
   envelope
      .try_reserve_exact(envelope_len)
      .map_err(|_| crate::Error::Custom("thumbnail response is too large".to_string()))?;
   envelope.extend_from_slice(&header_len.to_le_bytes());
   envelope.extend_from_slice(&header);
   for frame in frames {
      envelope.extend_from_slice(&frame.data);
   }
   Ok(envelope)
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
   use media_parser::{
      AudioTrackMeta, CoverArt, Frame, PixelFormat, SubtitleTrackMeta, UnknownTrackMeta,
      VideoTrackMeta,
   };

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

   #[test]
   fn serializes_cover_info_contract() {
      let cover = CoverInfo::from(CoverArt {
         format: PixelFormat::Jpeg,
         mime_type: "image/jpeg".to_string(),
         data: vec![1, 2, 3],
      });

      assert_eq!(
         serde_json::to_value(cover).expect("cover should serialize"),
         serde_json::json!({
            "format": "jpeg",
            "mimeType": "image/jpeg",
            "data": [1, 2, 3],
         })
      );
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
   fn encodes_thumbnail_metadata_and_image_bytes_in_one_binary_envelope() {
      let frames = vec![
         Frame {
            track_id: 3,
            width: 320,
            height: 180,
            timestamp: Duration::from_millis(250),
            format: PixelFormat::Jpeg,
            data: vec![1, 2, 3],
            strides: None,
         },
         Frame {
            track_id: 3,
            width: 640,
            height: 360,
            timestamp: Duration::from_secs(1),
            format: PixelFormat::Png,
            data: vec![4, 5],
            strides: None,
         },
      ];

      let envelope = encode_thumbnail_envelope(frames).expect("envelope should encode");
      let header_len = u32::from_le_bytes(envelope[..4].try_into().unwrap()) as usize;
      let header_end = 4 + header_len;
      let header: serde_json::Value =
         serde_json::from_slice(&envelope[4..header_end]).expect("header should be JSON");

      assert_eq!(
         header,
         serde_json::json!([
            {
               "trackId": 3,
               "width": 320,
               "height": 180,
               "timestampSec": 0.25,
               "format": "jpeg",
               "mimeType": "image/jpeg",
               "offset": 0,
               "length": 3,
            },
            {
               "trackId": 3,
               "width": 640,
               "height": 360,
               "timestampSec": 1.0,
               "format": "png",
               "mimeType": "image/png",
               "offset": 3,
               "length": 2,
            },
         ])
      );
      assert_eq!(&envelope[header_end..], &[1, 2, 3, 4, 5]);
   }

   #[test]
   fn session_cache_evicts_the_least_recently_used_entry() {
      let now = std::time::Instant::now();
      let mut cache = SessionCache::new(2);
      cache.insert("first".to_string(), 1, None);
      cache.insert("second".to_string(), 2, None);

      assert_eq!(cache.get(&"first".to_string(), now), Some(1));
      cache.insert("third".to_string(), 3, None);

      assert_eq!(cache.get(&"second".to_string(), now), None);
      assert_eq!(cache.get(&"first".to_string(), now), Some(1));
      assert_eq!(cache.get(&"third".to_string(), now), Some(3));
   }

   #[test]
   fn session_cache_drops_an_entry_at_its_expiration_deadline() {
      let now = Instant::now();
      let deadline = now + Duration::from_secs(30);
      let mut cache = SessionCache::new(1);
      cache.insert("remote".to_string(), 1, Some(deadline));

      assert_eq!(cache.get(&"remote".to_string(), now), Some(1));
      assert_eq!(cache.get(&"remote".to_string(), deadline), None);
   }

   #[test]
   fn thumbnail_session_key_normalizes_http_header_names_and_order() {
      let first_headers = HashMap::from([
         ("X-Test".to_string(), "one".to_string()),
         ("Authorization".to_string(), "Bearer token".to_string()),
      ]);
      let second_headers = HashMap::from([
         ("authorization".to_string(), "Bearer token".to_string()),
         ("x-test".to_string(), "one".to_string()),
      ]);

      let first = thumbnail_session_key("https://example.com/video.mp4", Some(&first_headers), 7);
      let second = thumbnail_session_key("https://example.com/video.mp4", Some(&second_headers), 7);

      assert!(first == second);
   }

   #[test]
   fn thumbnail_session_key_ignores_headers_for_local_sources() {
      let source = video_fixture_source();
      let headers = HashMap::from([("Authorization".to_string(), "ignored".to_string())]);

      assert!(
         thumbnail_session_key(&source, None, 1)
            == thumbnail_session_key(&source, Some(&headers), 1)
      );
   }

   #[test]
   fn thumbnail_session_key_changes_when_a_local_file_changes() {
      let unique = std::time::SystemTime::now()
         .duration_since(std::time::UNIX_EPOCH)
         .unwrap()
         .as_nanos();
      let path = std::env::temp_dir().join(format!(
         "media-parser-thumbnail-session-{}-{unique}",
         std::process::id()
      ));
      std::fs::write(&path, [1]).unwrap();
      let source = path.to_string_lossy();
      let first = thumbnail_session_key(&source, None, 1);

      std::fs::write(&path, [1, 2]).unwrap();
      let second = thumbnail_session_key(&source, None, 1);
      std::fs::remove_file(&path).unwrap();

      assert!(first != second);
   }

   #[test]
   fn only_remote_thumbnail_sessions_receive_an_expiration_deadline() {
      let now = Instant::now();

      assert_eq!(
         thumbnail_session_expiration("https://example.com/video.mp4", now),
         now.checked_add(REMOTE_THUMBNAIL_SESSION_TTL)
      );
      assert_eq!(thumbnail_session_expiration("/video.mp4", now), None);
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
   async fn empty_thumbnail_request_does_not_open_the_source() {
      let sessions = ThumbnailSessions::default();
      let frames = thumbnail_frames(
         &sessions,
         "/file/that/does/not/exist.mp4",
         &[],
         0,
         false,
         None,
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

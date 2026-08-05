use std::collections::HashMap;
use std::sync::{Arc, Mutex, Weak};
use std::time::{Duration, Instant};
use tauri::{State, command};
use url::Url;

use media_parser::{
   BaseTrackMeta, CoverArt, FileStreamReader, Frame, HttpStreamReader, JpegQuality, MediaParser,
   Metadata, StreamReader, TrackType,
   format::mp4::{ThumbnailIndex, ThumbnailOptions},
};

use crate::Result;

const MAX_THUMBNAIL_SESSIONS: usize = 8;
const REMOTE_THUMBNAIL_SESSION_TTL: Duration = Duration::from_secs(5 * 60);
const LOCAL_THUMBNAIL_SESSION_TTL: Duration = Duration::from_secs(60);
const MAX_THUMBNAIL_OUTPUT_BYTES: usize = 256 * 1024 * 1024;

type EnvelopeMeta = serde_json::Map<String, serde_json::Value>;

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

#[derive(Clone, PartialEq, Eq, Hash)]
struct ThumbnailSessionKey {
   source: String,
   headers: Vec<(String, String)>,
   track_id: u32,
   local_version: Option<LocalSourceVersion>,
}

#[derive(Clone, PartialEq, Eq, Hash)]
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
   build_locks: Mutex<HashMap<ThumbnailSessionKey, Weak<tauri::async_runtime::Mutex<()>>>>,
}

impl Default for ThumbnailSessions {
   fn default() -> Self {
      Self {
         cache: Mutex::new(SessionCache::new(MAX_THUMBNAIL_SESSIONS)),
         build_locks: Mutex::new(HashMap::new()),
      }
   }
}

impl ThumbnailSessions {
   /// Returns the per-key lock used to serialize index construction,
   /// creating it if this is the first waiter for `key`.
   fn build_lock(&self, key: &ThumbnailSessionKey) -> Result<Arc<tauri::async_runtime::Mutex<()>>> {
      let mut locks = self.build_locks.lock().map_err(|_| {
         crate::Error::Custom("thumbnail session lock table is unavailable".to_string())
      })?;
      locks.retain(|_, lock| lock.strong_count() > 0);
      if let Some(lock) = locks.get(key).and_then(Weak::upgrade) {
         return Ok(lock);
      }
      let lock = Arc::new(tauri::async_runtime::Mutex::new(()));
      locks.insert(key.clone(), Arc::downgrade(&lock));
      Ok(lock)
   }
}

fn is_http_source(source: &str) -> bool {
   Url::parse(source)
      .map(|url| matches!(url.scheme(), "http" | "https"))
      .unwrap_or(false)
}

/// Builds the reader for a source, with optional HTTP headers for URLs.
async fn open_reader(
   source: &str,
   headers: Option<&HashMap<String, String>>,
   is_remote: bool,
) -> Result<Arc<dyn StreamReader>> {
   if is_remote {
      let reader = match headers {
         Some(headers) => HttpStreamReader::with_headers(source, headers.clone()).await?,
         None => HttpStreamReader::new(source).await?,
      };
      Ok(Arc::new(reader))
   } else {
      Ok(Arc::new(FileStreamReader::new(source)?))
   }
}

async fn thumbnail_session_key(
   source: &str,
   headers: Option<&HashMap<String, String>>,
   track_id: u32,
   is_remote: bool,
) -> ThumbnailSessionKey {
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
   let local_version = if is_remote {
      None
   } else {
      let path = source.to_string();
      tauri::async_runtime::spawn_blocking(move || std::fs::metadata(path).ok())
         .await
         .ok()
         .flatten()
         .map(|metadata| LocalSourceVersion {
            length: metadata.len(),
            modified_nanos: metadata
               .modified()
               .ok()
               .and_then(|modified| modified.duration_since(std::time::UNIX_EPOCH).ok())
               .map(|elapsed| elapsed.as_nanos()),
         })
   };
   ThumbnailSessionKey {
      source: source.to_string(),
      headers,
      track_id,
      local_version,
   }
}

fn thumbnail_session_expiration(is_remote: bool, now: Instant) -> Option<Instant> {
   let ttl = if is_remote {
      REMOTE_THUMBNAIL_SESSION_TTL
   } else {
      LOCAL_THUMBNAIL_SESSION_TTL
   };
   now.checked_add(ttl)
}

async fn thumbnail_session(
   sessions: &ThumbnailSessions,
   source: &str,
   headers: Option<&HashMap<String, String>>,
   track_id: u32,
) -> Result<Arc<ThumbnailSession>> {
   let is_remote = is_http_source(source);
   let key = thumbnail_session_key(source, headers, track_id, is_remote).await;
   if let Some(session) = sessions
      .cache
      .lock()
      .map_err(|_| crate::Error::Custom("thumbnail session cache is unavailable".to_string()))?
      .get(&key, Instant::now())
   {
      return Ok(session);
   }

   // Serialize index construction per key so concurrent requests for the
   // same cold source share one build instead of racing N index builds.
   let build_lock = sessions.build_lock(&key)?;
   let _build_guard = build_lock.lock().await;

   if let Some(session) = sessions
      .cache
      .lock()
      .map_err(|_| crate::Error::Custom("thumbnail session cache is unavailable".to_string()))?
      .get(&key, Instant::now())
   {
      return Ok(session);
   }

   let reader = open_reader(source, headers, is_remote).await?;
   let index = Arc::new(ThumbnailIndex::read(reader.as_ref(), track_id).await?);
   let session = Arc::new(ThumbnailSession { reader, index });
   let expires_at = thumbnail_session_expiration(is_remote, Instant::now());
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
) -> Result<Metadata> {
   let reader = open_reader(&source, headers.as_ref(), is_http_source(&source)).await?;
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
) -> Result<Vec<TrackInfo>> {
   let reader = open_reader(&source, headers.as_ref(), is_http_source(&source)).await?;
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
) -> Result<tauri::ipc::Response> {
   let reader = open_reader(&source, headers.as_ref(), is_http_source(&source)).await?;
   let cover = MediaParser::new(reader.as_ref())
      .cover()
      .await
      .map_err(crate::Error::from)?;

   Ok(tauri::ipc::Response::new(cover_envelope(cover)?))
}

/// Extract thumbnails from a video track at millisecond timestamps.
#[command]
pub(crate) async fn get_thumbnails(
   source: String,
   timestamps: Vec<u64>,
   track_id: Option<u32>,
   accurate: Option<bool>,
   quality: Option<u8>,
   headers: Option<HashMap<String, String>>,
   sessions: State<'_, ThumbnailSessions>,
) -> Result<tauri::ipc::Response> {
   let options = thumbnail_options(quality)?;
   let timestamps = thumbnail_durations(&timestamps);
   // Extraction is deterministic per timestamp: dedup repeats so each unique
   // frame is decoded and transferred only once.
   let mut unique_timestamps = Vec::new();
   let mut index_by_timestamp = HashMap::new();
   let mut order = Vec::new();
   for timestamp in timestamps {
      let next_index = unique_timestamps.len();
      let index = *index_by_timestamp.entry(timestamp).or_insert(next_index);
      if index == next_index {
         unique_timestamps.push(timestamp);
      }
      order.push(index);
   }
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
   Ok(tauri::ipc::Response::new(encode_thumbnail_envelope(
      &frames,
      &order,
      MAX_THUMBNAIL_OUTPUT_BYTES,
   )?))
}

/// Validates the caller-supplied JPEG quality, if any, against the encoder's
/// 1-100 range. `None` keeps the thumbnail-grade default.
fn thumbnail_options(quality: Option<u8>) -> Result<ThumbnailOptions> {
   let Some(quality) = quality else {
      return Ok(ThumbnailOptions::default());
   };
   let quality = JpegQuality::new(quality).ok_or_else(|| {
      crate::Error::Custom(format!(
         "thumbnail quality must be between 1 and 100, got {quality}"
      ))
   })?;
   Ok(ThumbnailOptions { quality })
}

fn thumbnail_durations(timestamps_ms: &[u64]) -> Vec<Duration> {
   timestamps_ms
      .iter()
      .copied()
      .map(Duration::from_millis)
      .collect()
}

fn cover_envelope(cover: Option<CoverArt>) -> Result<Vec<u8>> {
   let Some(cover) = cover else {
      return encode_binary_envelope(Vec::new(), &[]);
   };
   let mut meta = EnvelopeMeta::new();
   meta.insert("format".into(), cover.format.label().into());
   meta.insert("mimeType".into(), cover.mime_type.into());
   meta.insert("offset".into(), 0.into());
   meta.insert("length".into(), cover.data.len().into());
   let payloads = [cover.data.as_slice()];
   encode_binary_envelope(vec![meta], &payloads)
}

fn thumbnail_envelope_entry(frame: &Frame, offset: usize) -> EnvelopeMeta {
   let mut meta = EnvelopeMeta::new();
   meta.insert("trackId".into(), frame.track_id.into());
   meta.insert("width".into(), frame.width.into());
   meta.insert("height".into(), frame.height.into());
   meta.insert("timestampSec".into(), frame.timestamp.as_secs_f64().into());
   meta.insert("format".into(), frame.format.label().into());
   meta.insert("mimeType".into(), frame.format.mime_type().into());
   meta.insert("offset".into(), offset.into());
   meta.insert("length".into(), frame.data.len().into());
   meta
}

/// Encodes one metadata entry per requested timestamp into the binary
/// envelope. `order` maps each output entry to a frame in `frames`, so
/// duplicate timestamps share the same payload bytes.
fn encode_thumbnail_envelope(
   frames: &[Frame],
   order: &[usize],
   max_output_bytes: usize,
) -> Result<Vec<u8>> {
   let mut offsets = Vec::new();
   offsets
      .try_reserve_exact(frames.len())
      .map_err(|_| crate::Error::Custom("too many thumbnail entries".to_string()))?;
   let mut payload_len = 0usize;
   for frame in frames {
      offsets.push(payload_len);
      payload_len = payload_len
         .checked_add(frame.data.len())
         .filter(|total| *total <= max_output_bytes)
         .ok_or_else(|| crate::Error::Custom("thumbnail payload is too large".to_string()))?;
   }

   let mut entries = Vec::new();
   entries
      .try_reserve_exact(order.len())
      .map_err(|_| crate::Error::Custom("too many thumbnail entries".to_string()))?;
   for &index in order {
      let frame = frames
         .get(index)
         .ok_or_else(|| crate::Error::Custom("thumbnail frame index out of range".to_string()))?;
      entries.push(thumbnail_envelope_entry(frame, offsets[index]));
   }
   let payloads = frames
      .iter()
      .map(|frame| frame.data.as_slice())
      .collect::<Vec<_>>();
   encode_binary_envelope(entries, &payloads)
}

/// Binary envelope shared by covers and thumbnails: a little-endian u32
/// header length, a JSON array of entry metadata, then the concatenated
/// payloads. Each entry carries its own `offset`/`length` into the payload.
fn encode_binary_envelope(entries: Vec<EnvelopeMeta>, payloads: &[&[u8]]) -> Result<Vec<u8>> {
   let payload_len = payloads
      .iter()
      .try_fold(0usize, |total, payload| total.checked_add(payload.len()))
      .ok_or_else(|| crate::Error::Custom("envelope payload is too large".to_string()))?;
   let header = serde_json::to_vec(&entries)
      .map_err(|error| crate::Error::Custom(format!("could not encode envelope: {error}")))?;
   let header_len = u32::try_from(header.len())
      .map_err(|_| crate::Error::Custom("envelope header is too large".to_string()))?;
   let envelope_len = 4usize
      .checked_add(header.len())
      .and_then(|length| length.checked_add(payload_len))
      .ok_or_else(|| crate::Error::Custom("envelope is too large".to_string()))?;
   let mut envelope = Vec::new();
   envelope
      .try_reserve_exact(envelope_len)
      .map_err(|_| crate::Error::Custom("envelope is too large".to_string()))?;
   envelope.extend_from_slice(&header_len.to_le_bytes());
   envelope.extend_from_slice(&header);
   for payload in payloads {
      envelope.extend_from_slice(payload);
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

   fn test_frames() -> Vec<Frame> {
      vec![
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
      ]
   }

   fn envelope_parts(envelope: &[u8]) -> (serde_json::Value, &[u8]) {
      let header_len = u32::from_le_bytes(envelope[..4].try_into().unwrap()) as usize;
      let header_end = 4 + header_len;
      let header = serde_json::from_slice(&envelope[4..header_end]).expect("header should be JSON");
      (header, &envelope[header_end..])
   }

   #[test]
   fn encodes_cover_as_a_single_entry_binary_envelope() {
      let envelope = cover_envelope(Some(CoverArt {
         format: PixelFormat::Jpeg,
         mime_type: "image/jpeg".to_string(),
         data: vec![1, 2, 3],
      }))
      .expect("cover should encode");
      let (header, payload) = envelope_parts(&envelope);

      assert_eq!(
         header,
         serde_json::json!([{
            "format": "jpeg",
            "mimeType": "image/jpeg",
            "offset": 0,
            "length": 3,
         }])
      );
      assert_eq!(payload, &[1, 2, 3]);
   }

   #[test]
   fn encodes_missing_cover_as_an_empty_envelope() {
      let envelope = cover_envelope(None).expect("empty cover should encode");
      let (header, payload) = envelope_parts(&envelope);

      assert_eq!(header, serde_json::json!([]));
      assert!(payload.is_empty());
   }

   #[test]
   fn omitted_thumbnail_quality_keeps_the_default() {
      assert_eq!(
         thumbnail_options(None).expect("no quality is valid"),
         ThumbnailOptions::default()
      );
   }

   #[test]
   fn thumbnail_quality_is_rejected_outside_the_encoder_range() {
      assert_eq!(
         thumbnail_options(Some(80))
            .expect("80 is in range")
            .quality
            .get(),
         80
      );

      for quality in [0u8, 101, 255] {
         let error = thumbnail_options(Some(quality))
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
   fn encodes_thumbnail_metadata_and_image_bytes_in_one_binary_envelope() {
      let envelope = encode_thumbnail_envelope(&test_frames(), &[0, 1], usize::MAX)
         .expect("envelope should encode");
      let (header, payload) = envelope_parts(&envelope);

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
      assert_eq!(payload, &[1, 2, 3, 4, 5]);
   }

   #[test]
   fn duplicate_thumbnail_timestamps_share_the_same_payload_bytes() {
      let envelope = encode_thumbnail_envelope(&test_frames(), &[0, 1, 0], usize::MAX)
         .expect("envelope should encode");
      let (header, payload) = envelope_parts(&envelope);

      let entries = header.as_array().expect("header should be an array");
      assert_eq!(entries.len(), 3);
      assert_eq!(entries[0]["offset"], entries[2]["offset"]);
      assert_eq!(entries[0]["length"], entries[2]["length"]);
      assert_eq!(entries[1]["offset"], serde_json::json!(3));
      assert_eq!(payload, &[1, 2, 3, 4, 5]);
   }

   #[test]
   fn thumbnail_envelope_rejects_payloads_beyond_the_output_cap() {
      let result = encode_thumbnail_envelope(&test_frames(), &[0, 1], 4);

      assert!(result.is_err());
   }

   #[test]
   fn session_cache_evicts_the_least_recently_used_entry() {
      let now = Instant::now();
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

   #[tokio::test]
   async fn thumbnail_session_key_normalizes_http_header_names_and_order() {
      let first_headers = HashMap::from([
         ("X-Test".to_string(), "one".to_string()),
         ("Authorization".to_string(), "Bearer token".to_string()),
      ]);
      let second_headers = HashMap::from([
         ("authorization".to_string(), "Bearer token".to_string()),
         ("x-test".to_string(), "one".to_string()),
      ]);

      let first = thumbnail_session_key(
         "https://example.com/video.mp4",
         Some(&first_headers),
         7,
         true,
      )
      .await;
      let second = thumbnail_session_key(
         "https://example.com/video.mp4",
         Some(&second_headers),
         7,
         true,
      )
      .await;

      assert!(first == second);
   }

   #[tokio::test]
   async fn thumbnail_session_key_ignores_headers_for_local_sources() {
      let source = video_fixture_source();
      let headers = HashMap::from([("Authorization".to_string(), "ignored".to_string())]);

      assert!(
         thumbnail_session_key(&source, None, 1, false).await
            == thumbnail_session_key(&source, Some(&headers), 1, false).await
      );
   }

   #[tokio::test]
   async fn thumbnail_session_key_changes_when_a_local_file_changes() {
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
      let first = thumbnail_session_key(&source, None, 1, false).await;

      std::fs::write(&path, [1, 2]).unwrap();
      let second = thumbnail_session_key(&source, None, 1, false).await;
      std::fs::remove_file(&path).unwrap();

      assert!(first != second);
   }

   #[test]
   fn remote_and_local_thumbnail_sessions_both_receive_an_expiration_deadline() {
      let now = Instant::now();

      assert_eq!(
         thumbnail_session_expiration(true, now),
         now.checked_add(REMOTE_THUMBNAIL_SESSION_TTL)
      );
      assert_eq!(
         thumbnail_session_expiration(false, now),
         now.checked_add(LOCAL_THUMBNAIL_SESSION_TTL)
      );
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
      let key = thumbnail_session_key(&source, None, 0, false).await;
      let key_lock = sessions
         .build_lock(&key)
         .expect("build lock should be available");
      let guard = key_lock.lock().await;

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

      // This test and the three requests hold four strong references. Reaching
      // four proves every request observed the cold cache and joined this lock.
      tokio::time::timeout(Duration::from_secs(10), async {
         loop {
            if Arc::strong_count(&key_lock) >= 4 {
               break;
            }
            tokio::task::yield_now().await;
         }
      })
      .await
      .expect("all requests should reference the pre-acquired per-key lock");
      drop(guard);
      drop(key_lock);

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
      assert!(
         sessions
            .build_locks
            .lock()
            .expect("lock table should be reachable")
            .values()
            .all(|lock| lock.strong_count() == 0),
         "completed builds must not retain strong lock references"
      );
   }

   #[tokio::test]
   async fn failed_thumbnail_session_build_does_not_retain_its_build_lock() {
      let sessions = ThumbnailSessions::default();

      assert!(
         thumbnail_session(&sessions, "/file/that/does/not/exist.mp4", None, 0)
            .await
            .is_err()
      );
      assert!(
         sessions
            .build_locks
            .lock()
            .expect("lock table should be reachable")
            .values()
            .all(|lock| lock.strong_count() == 0),
         "failed session builds must not retain strong lock references"
      );
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

use std::collections::HashMap;
use tauri::command;
use url::Url;

use media_parser::{
   BaseTrackMeta, FileStreamReader, HttpStreamReader, MediaParser, Metadata, TrackType,
};

use crate::Result;

/// Helper macro to handle stream instantiation based on the source (URL or File).
macro_rules! with_reader {
   ($source:expr, $headers:expr, |$reader:ident| $body:expr) => {{
      let is_http_url = Url::parse(&$source)
         .map(|url| matches!(url.scheme(), "http" | "https"))
         .unwrap_or(false);

      if is_http_url {
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

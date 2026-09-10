use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;
use url::{Origin, Url};

use media_parser::{FileStreamReader, HttpStreamReader, StreamReader};

use crate::Result;
use crate::session_cache::ExpirationPolicy;

const REMOTE_SESSION_TTL: Duration = Duration::from_secs(5 * 60);
const LOCAL_SESSION_TTL: Duration = Duration::from_secs(60);
pub(crate) const SESSION_REAPER_INTERVAL: Duration = Duration::from_secs(1);

pub(crate) struct DefaultHeaders {
   headers: HashMap<String, String>,
   origins: Option<Vec<Origin>>,
}

impl DefaultHeaders {
   pub(crate) fn new(
      headers: HashMap<String, String>,
      origins: Option<Vec<String>>,
   ) -> Result<Self> {
      let origins = origins
         .map(|origins| {
            origins
               .into_iter()
               .map(|origin| {
                  let url = Url::parse(&origin).map_err(|error| {
                     crate::Error::Custom(format!("Invalid default headers origin: {error}"))
                  })?;
                  if !matches!(url.scheme(), "http" | "https") {
                     return Err(crate::Error::Custom(
                        "Default headers origins must use HTTP(S)".into(),
                     ));
                  }
                  Ok(url.origin())
               })
               .collect::<Result<Vec<_>>>()
         })
         .transpose()?;
      Ok(Self { headers, origins })
   }

   pub(crate) fn merge(
      &self,
      source: &str,
      headers: Option<HashMap<String, String>>,
   ) -> Option<HashMap<String, String>> {
      let Ok(url) = Url::parse(source) else {
         return None;
      };
      if !matches!(url.scheme(), "http" | "https") {
         return None;
      }
      if self.headers.is_empty()
         || self
            .origins
            .as_ref()
            .is_some_and(|origins| !origins.contains(&url.origin()))
      {
         return headers;
      }
      let mut headers = headers.unwrap_or_default();
      for (name, value) in &self.headers {
         if !headers.keys().any(|key| key.eq_ignore_ascii_case(name)) {
            headers.insert(name.clone(), value.clone());
         }
      }
      Some(headers)
   }
}

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub(crate) struct MediaSourceKey {
   source: String,
   headers: Vec<(String, String)>,
   local_version: Option<LocalSourceVersion>,
}

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
struct LocalSourceVersion {
   length: u64,
   modified_nanos: Option<u128>,
}

fn is_http_source(source: &str) -> bool {
   Url::parse(source)
      .map(|url| matches!(url.scheme(), "http" | "https"))
      .unwrap_or(false)
}

pub(crate) async fn source_key(
   source: &str,
   headers: Option<&HashMap<String, String>>,
) -> MediaSourceKey {
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

   MediaSourceKey {
      source: source.to_string(),
      headers,
      local_version,
   }
}

pub(crate) async fn open_reader(
   source: &str,
   headers: Option<&HashMap<String, String>>,
) -> Result<Arc<dyn StreamReader>> {
   if is_http_source(source) {
      let reader = match headers {
         Some(headers) => HttpStreamReader::with_headers(source, headers.clone()).await?,
         None => HttpStreamReader::new(source).await?,
      };
      Ok(Arc::new(reader))
   } else {
      Ok(Arc::new(FileStreamReader::new(source)?))
   }
}

/// Remote bytes can change behind an unchanged URL, so a remote session is
/// capped by age. A local session is keyed by size and modification time and
/// reused under the documented promise that the file stays unchanged, so it
/// expires on inactivity instead.
pub(crate) fn session_expiration(source: &MediaSourceKey) -> ExpirationPolicy {
   if is_http_source(&source.source) {
      ExpirationPolicy::Absolute(REMOTE_SESSION_TTL)
   } else {
      ExpirationPolicy::Sliding(LOCAL_SESSION_TTL)
   }
}

#[cfg(test)]
mod tests {
   use super::*;
   use std::collections::HashMap;
   use std::time::Duration;

   #[test]
   fn per_call_headers_override_defaults_ignoring_case() {
      for (default_name, request_name) in [
         ("x-app-version", "X-App-Version"),
         ("X-App-Version", "x-app-version"),
      ] {
         let defaults = DefaultHeaders::new(
            HashMap::from([
               (default_name.into(), "1.0".into()),
               ("x-other".into(), "keep".into()),
            ]),
            None,
         )
         .unwrap();
         let headers = Some(HashMap::from([(request_name.into(), "2.0".into())]));
         assert_eq!(
            defaults.merge("https://example.com/video.mp4", headers),
            Some(HashMap::from([
               (request_name.into(), "2.0".into()),
               ("x-other".into(), "keep".into()),
            ])),
         );
      }
   }

   #[test]
   fn default_headers_only_reach_normalized_allowed_origins() {
      let headers = HashMap::from([("authorization".into(), "Bearer test".into())]);
      let defaults = DefaultHeaders::new(
         headers.clone(),
         Some(vec!["https://API.EXAMPLE:443/path".into()]),
      )
      .unwrap();
      assert_eq!(
         defaults.merge("https://api.example/video.mp4", None),
         Some(headers)
      );
      for source in [
         "http://api.example/video.mp4",
         "https://api.example:444/video.mp4",
         "https://sub.api.example/video.mp4",
         "https://api.example.evil/video.mp4",
      ] {
         let per_call = Some(HashMap::from([("x-call".into(), "keep".into())]));
         assert_eq!(defaults.merge(source, per_call.clone()), per_call);
      }
   }

   #[test]
   fn absent_origins_are_global_and_empty_origins_allow_none() {
      let headers = HashMap::from([("user-agent".into(), "app/1.0".into())]);
      let global = DefaultHeaders::new(headers.clone(), None).unwrap();
      let empty = DefaultHeaders::new(headers.clone(), Some(vec![])).unwrap();
      assert_eq!(
         global.merge("https://any.example/video.mp4", None),
         Some(headers.clone())
      );
      assert_eq!(empty.merge("https://any.example/video.mp4", None), None);
      for source in [
         "/tmp/video.mp4",
         "C:\\video.mp4",
         "file:///tmp/video.mp4",
         "https://",
      ] {
         assert_eq!(global.merge(source, Some(headers.clone())), None);
         assert_eq!(empty.merge(source, Some(headers.clone())), None);
      }
   }

   #[test]
   fn invalid_default_header_origins_are_rejected() {
      for origin in [
         "not a URL",
         "file:///tmp/video.mp4",
         "ftp://api.example",
         "https://",
      ] {
         assert!(DefaultHeaders::new(HashMap::new(), Some(vec![origin.into()])).is_err());
      }
   }

   #[tokio::test]
   async fn remote_source_key_normalizes_header_names_and_order() {
      let first_headers = HashMap::from([
         ("X-Test".to_string(), "one".to_string()),
         ("Authorization".to_string(), "Bearer token".to_string()),
      ]);
      let second_headers = HashMap::from([
         ("authorization".to_string(), "Bearer token".to_string()),
         ("x-test".to_string(), "one".to_string()),
      ]);

      let first = source_key("https://example.com/video.mp4", Some(&first_headers)).await;
      let second = source_key("https://example.com/video.mp4", Some(&second_headers)).await;

      assert!(first == second);
   }

   #[tokio::test]
   async fn local_source_key_ignores_headers_and_changes_with_file_version() {
      let unique = std::time::SystemTime::now()
         .duration_since(std::time::UNIX_EPOCH)
         .expect("the system clock should follow the Unix epoch")
         .as_nanos();
      let path = std::env::temp_dir().join(format!(
         "media-parser-source-key-{}-{unique}",
         std::process::id()
      ));
      std::fs::write(&path, [1]).expect("fixture should be written");
      let source = path.to_string_lossy();
      let headers = HashMap::from([("Authorization".to_string(), "ignored".to_string())]);

      let first = source_key(&source, None).await;
      let with_headers = source_key(&source, Some(&headers)).await;
      assert!(first == with_headers);

      std::fs::write(&path, [1, 2]).expect("fixture version should change");
      let changed = source_key(&source, None).await;
      std::fs::remove_file(&path).expect("fixture should be removed");

      assert!(first != changed);
   }

   #[tokio::test]
   async fn missing_local_metadata_keeps_a_stable_key_without_a_version() {
      let first = source_key("/file/that/does/not/exist.mp4", None).await;
      let second = source_key("/file/that/does/not/exist.mp4", None).await;

      assert!(first == second);
      assert!(first.local_version.is_none());
   }

   #[tokio::test]
   async fn local_sessions_slide_for_one_minute_and_remote_sessions_end_after_five() {
      let local = source_key("/file/that/does/not/exist.mp4", None).await;
      let remote = source_key("https://example.com/video.mp4", None).await;

      assert_eq!(
         session_expiration(&local),
         ExpirationPolicy::Sliding(Duration::from_secs(60))
      );
      assert_eq!(
         session_expiration(&remote),
         ExpirationPolicy::Absolute(Duration::from_secs(5 * 60))
      );
   }

   #[test]
   fn shared_session_reaper_interval_is_one_second() {
      assert_eq!(SESSION_REAPER_INTERVAL, Duration::from_secs(1));
   }

   #[test]
   fn source_mode_recognizes_only_http_and_https_urls() {
      assert!(is_http_source("http://example.com/video.mp4"));
      assert!(is_http_source("https://example.com/video.mp4"));
      assert!(!is_http_source("file:///tmp/video.mp4"));
      assert!(!is_http_source("/tmp/video.mp4"));
   }
}

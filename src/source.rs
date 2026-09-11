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

#[derive(Debug, Default, PartialEq, Eq)]
pub(crate) struct MergedHeaders {
   pub(crate) headers: Option<HashMap<String, String>>,
   // A restricted default was actually inserted, rather than overridden per call.
   pub(crate) force_same_origin: bool,
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
   ) -> Result<MergedHeaders> {
      let Ok(url) = Url::parse(source) else {
         return Ok(MergedHeaders::default());
      };
      if !matches!(url.scheme(), "http" | "https") {
         return Ok(MergedHeaders::default());
      }
      if self.headers.is_empty()
         || self
            .origins
            .as_ref()
            .is_some_and(|origins| !origins.contains(&url.origin()))
      {
         return Ok(MergedHeaders {
            headers,
            force_same_origin: false,
         });
      }
      let mut headers = headers.unwrap_or_default();
      if headers.keys().any(|name| name.eq_ignore_ascii_case("host")) {
         return Err(crate::Error::Custom(
            "Per-call Host header is not allowed when default headers apply".into(),
         ));
      }
      let mut force_same_origin = false;
      for (name, value) in &self.headers {
         if !headers.keys().any(|key| key.eq_ignore_ascii_case(name)) {
            headers.insert(name.clone(), value.clone());
            force_same_origin |= self.origins.is_some();
         }
      }
      Ok(MergedHeaders {
         headers: Some(headers),
         force_same_origin,
      })
   }
}

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub(crate) struct MediaSourceKey {
   source: String,
   headers: Vec<(String, String)>,
   force_same_origin: bool,
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

pub(crate) async fn source_key(source: &str, merged: &MergedHeaders) -> MediaSourceKey {
   let is_remote = is_http_source(source);
   let mut headers = if is_remote {
      merged
         .headers
         .as_ref()
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
      force_same_origin: is_remote && merged.force_same_origin,
      local_version,
   }
}

pub(crate) async fn open_reader(
   source: &str,
   merged: &MergedHeaders,
) -> Result<Arc<dyn StreamReader>> {
   if is_http_source(source) {
      let reader = HttpStreamReader::with_headers_and_redirect_policy(
         source,
         merged.headers.clone().unwrap_or_default(),
         merged.force_same_origin,
      )
      .await?;
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
   fn per_call_host_is_rejected_ignoring_case_when_defaults_apply() {
      for origins in [None, Some(vec!["https://api.example".into()])] {
         let defaults = DefaultHeaders::new(
            HashMap::from([("authorization".into(), "Bearer test".into())]),
            origins,
         )
         .unwrap();
         for name in ["Host", "host", "HOST", "hOsT"] {
            for override_default in [false, true] {
               let mut headers = HashMap::from([(name.into(), "other-tenant.example".into())]);
               if override_default {
                  headers.insert("Authorization".into(), "Bearer per-call".into());
               }
               let result = defaults.merge("https://api.example/file", Some(headers));
               assert!(
                  matches!(result, Err(crate::Error::Custom(ref message)) if message.contains("Host")),
                  "per-call {name} must fail when defaults apply: {result:?}"
               );
            }
         }
      }
   }

   #[test]
   fn per_call_host_is_preserved_when_no_defaults_apply() {
      let configured = HashMap::from([("authorization".into(), "Bearer test".into())]);
      for (headers, origins) in [
         (HashMap::new(), None),
         (HashMap::new(), Some(vec!["https://api.example".into()])),
         (configured.clone(), Some(vec![])),
         (configured, Some(vec!["https://other.example".into()])),
      ] {
         let defaults = DefaultHeaders::new(headers, origins).unwrap();
         let per_call = Some(HashMap::from([(
            "Host".into(),
            "other-tenant.example".into(),
         )]));
         let merged = defaults
            .merge("https://api.example/file", per_call.clone())
            .unwrap();
         assert_eq!(merged.headers, per_call);
         assert!(!merged.force_same_origin);
      }
   }

   #[test]
   fn rust_default_host_is_allowed() {
      let headers = HashMap::from([
         ("Host".into(), "trusted-tenant.example".into()),
         ("authorization".into(), "Bearer test".into()),
      ]);
      for origins in [None, Some(vec!["https://api.example".into()])] {
         let restricted = origins.is_some();
         let defaults = DefaultHeaders::new(headers.clone(), origins).unwrap();
         let merged = defaults.merge("https://api.example/file", None).unwrap();
         assert_eq!(merged.headers, Some(headers.clone()));
         assert_eq!(merged.force_same_origin, restricted);
      }
   }

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
            defaults
               .merge("https://example.com/video.mp4", headers)
               .unwrap()
               .headers,
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
      let merged = defaults
         .merge("https://api.example/video.mp4", None)
         .unwrap();
      assert_eq!(merged.headers, Some(headers));
      assert!(merged.force_same_origin);
      for source in [
         "http://api.example/video.mp4",
         "https://api.example:444/video.mp4",
         "https://sub.api.example/video.mp4",
         "https://api.example.evil/video.mp4",
      ] {
         let per_call = Some(HashMap::from([("x-call".into(), "keep".into())]));
         let merged = defaults.merge(source, per_call.clone()).unwrap();
         assert_eq!(merged.headers, per_call);
         assert!(!merged.force_same_origin);
      }
   }

   #[test]
   fn absent_origins_are_global_and_empty_origins_allow_none() {
      let headers = HashMap::from([("user-agent".into(), "app/1.0".into())]);
      let global = DefaultHeaders::new(headers.clone(), None).unwrap();
      let empty = DefaultHeaders::new(headers.clone(), Some(vec![])).unwrap();
      let merged = global.merge("https://any.example/video.mp4", None).unwrap();
      assert_eq!(merged.headers, Some(headers.clone()));
      assert!(!merged.force_same_origin);
      assert_eq!(
         empty.merge("https://any.example/video.mp4", None).unwrap(),
         MergedHeaders::default()
      );
      for source in [
         "/tmp/video.mp4",
         "C:\\video.mp4",
         "file:///tmp/video.mp4",
         "https://",
      ] {
         let mut headers = headers.clone();
         headers.insert("Host".into(), "other-tenant.example".into());
         assert_eq!(
            global.merge(source, Some(headers.clone())).unwrap(),
            MergedHeaders::default()
         );
         assert_eq!(
            empty.merge(source, Some(headers.clone())).unwrap(),
            MergedHeaders::default()
         );
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
   async fn merged_user_agent_redirects_preserve_origin_restrictions_and_overrides() {
      use wiremock::matchers::{any, header};
      use wiremock::{Mock, MockServer, ResponseTemplate};

      for (restricted, overridden, blocked) in [
         (false, false, false),
         (true, false, true),
         (true, true, false),
      ] {
         let source = MockServer::start().await;
         let target = MockServer::start().await;
         Mock::given(header("User-Agent", "app/1.0"))
            .respond_with(ResponseTemplate::new(302).insert_header("Location", target.uri()))
            .expect(2)
            .mount(&source)
            .await;
         Mock::given(any())
            .and(header("User-Agent", "app/1.0"))
            .respond_with(ResponseTemplate::new(200).set_body_bytes(b"data"))
            .expect(if blocked { 0 } else { 2 })
            .mount(&target)
            .await;
         let mut builder = crate::Builder::new().user_agent("app/1.0");
         if restricted {
            builder = builder.default_headers_origins([source.uri(), target.uri()]);
         }
         let defaults = builder.into_default_headers().unwrap();
         let per_call =
            overridden.then(|| HashMap::from([("user-agent".into(), "app/1.0".into())]));
         let headers = defaults.merge(&source.uri(), per_call).unwrap();
         let reader = open_reader(&source.uri(), &headers).await.unwrap();

         let size = reader.size().await;
         let bytes = reader.read_vec(0, 4).await;
         if blocked {
            for error in [size.unwrap_err(), bytes.unwrap_err()] {
               assert_eq!(
                  serde_json::to_value(crate::Error::from(error)).unwrap(),
                  serde_json::json!(
                     "Media parser error: HTTP request failed: cross-origin redirect blocked: same-origin policy enforced"
                  )
               );
            }
            assert!(target.received_requests().await.unwrap().is_empty());
         } else {
            assert_eq!(size.unwrap(), 4);
            assert_eq!(bytes.unwrap(), b"data");
         }
      }
   }

   #[tokio::test]
   async fn restricted_default_override_with_the_same_value_has_a_distinct_source_key() {
      let source = "https://example.com/video.mp4";
      let defaults = DefaultHeaders::new(
         HashMap::from([("user-agent".into(), "app/1.0".into())]),
         Some(vec![source.into()]),
      )
      .unwrap();
      let inherited = defaults.merge(source, None).unwrap();
      let overridden = defaults.merge(source, inherited.headers.clone()).unwrap();

      assert_eq!(inherited.headers, overridden.headers);
      assert!(inherited.force_same_origin);
      assert!(!overridden.force_same_origin);
      let inherited_key = source_key(source, &inherited).await;
      let overridden_key = source_key(source, &overridden).await;
      assert_ne!(inherited_key, overridden_key);
      assert_eq!(
         std::collections::HashSet::from([inherited_key, overridden_key]).len(),
         2
      );
   }

   #[test]
   fn only_effectively_inserted_restricted_defaults_force_same_origin() {
      let source = "https://example.com/video.mp4";
      let defaults = DefaultHeaders::new(
         HashMap::from([
            ("User-Agent".into(), "app/1.0".into()),
            ("X-App-Version".into(), "1.0".into()),
         ]),
         Some(vec![source.into()]),
      )
      .unwrap();
      let partial_override = defaults
         .merge(
            source,
            Some(HashMap::from([("user-agent".into(), "app/1.0".into())])),
         )
         .unwrap();
      assert!(partial_override.force_same_origin);
      let full_override = defaults
         .merge(
            source,
            Some(HashMap::from([
               ("user-agent".into(), "app/1.0".into()),
               ("x-app-version".into(), "1.0".into()),
            ])),
         )
         .unwrap();
      assert!(!full_override.force_same_origin);
      assert!(
         !defaults
            .merge("https://other.example/file", None)
            .unwrap()
            .force_same_origin
      );

      let empty_defaults = DefaultHeaders::new(HashMap::new(), Some(vec![source.into()])).unwrap();
      assert!(
         !empty_defaults
            .merge(source, None)
            .unwrap()
            .force_same_origin
      );
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

      let first = source_key(
         "https://example.com/video.mp4",
         &MergedHeaders {
            headers: Some(first_headers),
            force_same_origin: false,
         },
      )
      .await;
      let second = source_key(
         "https://example.com/video.mp4",
         &MergedHeaders {
            headers: Some(second_headers),
            force_same_origin: false,
         },
      )
      .await;

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

      let first = source_key(&source, &MergedHeaders::default()).await;
      let with_headers = source_key(
         &source,
         &MergedHeaders {
            headers: Some(headers),
            force_same_origin: true,
         },
      )
      .await;
      assert!(first == with_headers);

      std::fs::write(&path, [1, 2]).expect("fixture version should change");
      let changed = source_key(&source, &MergedHeaders::default()).await;
      std::fs::remove_file(&path).expect("fixture should be removed");

      assert!(first != changed);
   }

   #[tokio::test]
   async fn missing_local_metadata_keeps_a_stable_key_without_a_version() {
      let first = source_key("/file/that/does/not/exist.mp4", &MergedHeaders::default()).await;
      let second = source_key("/file/that/does/not/exist.mp4", &MergedHeaders::default()).await;

      assert!(first == second);
      assert!(first.local_version.is_none());
   }

   #[tokio::test]
   async fn local_sessions_slide_for_one_minute_and_remote_sessions_end_after_five() {
      let local = source_key("/file/that/does/not/exist.mp4", &MergedHeaders::default()).await;
      let remote = source_key("https://example.com/video.mp4", &MergedHeaders::default()).await;

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

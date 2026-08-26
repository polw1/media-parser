use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;
use url::Url;

use media_parser::{FileStreamReader, HttpStreamReader, StreamReader};

use crate::Result;

const REMOTE_SESSION_TTL: Duration = Duration::from_secs(5 * 60);
const LOCAL_SESSION_TTL: Duration = Duration::from_secs(60);

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

pub(crate) fn is_http_source(source: &str) -> bool {
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

pub(crate) fn session_ttl(source: &MediaSourceKey) -> Duration {
   if is_http_source(&source.source) {
      REMOTE_SESSION_TTL
   } else {
      LOCAL_SESSION_TTL
   }
}

#[cfg(test)]
mod tests {
   use super::*;
   use std::collections::HashMap;
   use std::time::Duration;

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
   async fn shared_session_ttl_is_one_minute_local_and_five_minutes_remote() {
      let local = source_key("/file/that/does/not/exist.mp4", None).await;
      let remote = source_key("https://example.com/video.mp4", None).await;

      assert_eq!(session_ttl(&local), Duration::from_secs(60));
      assert_eq!(session_ttl(&remote), Duration::from_secs(5 * 60));
   }

   #[test]
   fn source_mode_recognizes_only_http_and_https_urls() {
      assert!(is_http_source("http://example.com/video.mp4"));
      assert!(is_http_source("https://example.com/video.mp4"));
      assert!(!is_http_source("file:///tmp/video.mp4"));
      assert!(!is_http_source("/tmp/video.mp4"));
   }
}

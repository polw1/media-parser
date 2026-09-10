use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use media_parser::format::mp4::SubtitleIndex;
use media_parser::{StreamReader, SubtitleTrack, TrackFilter};
use tauri::{State, command};

use crate::Result;
use crate::envelope::{
   JS_MAX_SAFE_INTEGER, MAX_SUBTITLE_OUTPUT_BYTES, encode_subtitle_envelope, run_envelope_task,
};
use crate::session_cache::SessionPool;
use crate::source::{
   MediaSourceKey, SESSION_REAPER_INTERVAL, open_reader, session_expiration, source_key,
};

const MAX_SUBTITLE_SESSIONS: usize = 8;

struct SubtitleSession {
   reader: Arc<dyn StreamReader>,
   index: Arc<SubtitleIndex>,
}

pub(crate) struct SubtitleSessions {
   pool: SessionPool<MediaSourceKey, SubtitleSession>,
}

impl Default for SubtitleSessions {
   fn default() -> Self {
      Self {
         pool: SessionPool::new(MAX_SUBTITLE_SESSIONS, SESSION_REAPER_INTERVAL),
      }
   }
}

#[derive(Debug)]
struct PreparedSubtitleRequest {
   filter: Option<TrackFilter>,
   first_track_only: bool,
   range: Option<(Duration, Duration)>,
}

fn prepare_subtitle_request(
   track_id: Option<u32>,
   language: Option<String>,
   start_ms: Option<u64>,
   end_ms: Option<u64>,
) -> Result<PreparedSubtitleRequest> {
   for endpoint in [start_ms, end_ms].into_iter().flatten() {
      if endpoint > JS_MAX_SAFE_INTEGER {
         return Err(crate::Error::Custom(
            "subtitle range endpoints must be JavaScript safe integers".to_string(),
         ));
      }
   }

   let range = match (start_ms, end_ms) {
      (None, None) => None,
      (Some(start), Some(end)) if start < end => {
         Some((Duration::from_millis(start), Duration::from_millis(end)))
      }
      (Some(_), Some(_)) => {
         return Err(crate::Error::Custom(
            "subtitle range start must be before end".to_string(),
         ));
      }
      _ => {
         return Err(crate::Error::Custom(
            "subtitle range requires both startMs and endMs".to_string(),
         ));
      }
   };

   let (filter, first_track_only) = match track_id {
      Some(0) => (None, true),
      Some(id) => (Some(TrackFilter::TrackId(id)), false),
      None => (language.map(TrackFilter::Language), false),
   };
   Ok(PreparedSubtitleRequest {
      filter,
      first_track_only,
      range,
   })
}

async fn subtitle_session(
   sessions: &SubtitleSessions,
   source: &str,
   headers: Option<&HashMap<String, String>>,
) -> Result<Arc<SubtitleSession>> {
   let key = source_key(source, headers).await;
   let expiration = session_expiration(&key);
   sessions
      .pool
      .get_or_try_build(key, expiration, || async {
         let reader = open_reader(source, headers).await?;
         let index = Arc::new(SubtitleIndex::read(reader.as_ref()).await?);
         Ok(SubtitleSession { reader, index })
      })
      .await
}

async fn subtitle_tracks(
   sessions: &SubtitleSessions,
   source: &str,
   track_id: Option<u32>,
   language: Option<String>,
   start_ms: Option<u64>,
   end_ms: Option<u64>,
   headers: Option<&HashMap<String, String>>,
) -> Result<Vec<SubtitleTrack>> {
   let request = prepare_subtitle_request(track_id, language, start_ms, end_ms)?;
   let session = subtitle_session(sessions, source, headers).await?;
   if request.first_track_only {
      return Ok(Arc::clone(&session.index)
         .subtitles_first(session.reader.as_ref(), request.range)
         .await?);
   }
   Ok(Arc::clone(&session.index)
      .subtitles(session.reader.as_ref(), request.filter, request.range)
      .await?)
}

async fn subtitle_envelope(
   sessions: &SubtitleSessions,
   source: &str,
   track_id: Option<u32>,
   language: Option<String>,
   start_ms: Option<u64>,
   end_ms: Option<u64>,
   headers: Option<&HashMap<String, String>>,
) -> Result<Vec<u8>> {
   let tracks = subtitle_tracks(
      sessions, source, track_id, language, start_ms, end_ms, headers,
   )
   .await?;
   run_envelope_task("subtitle", move || {
      encode_subtitle_envelope(&tracks, MAX_SUBTITLE_OUTPUT_BYTES)
   })
   .await
}

/// Extract subtitle tracks, optionally filtered and restricted to a half-open range.
#[command]
pub(crate) async fn get_subtitles(
   source: String,
   track_id: Option<u32>,
   language: Option<String>,
   start_ms: Option<u64>,
   end_ms: Option<u64>,
   headers: Option<HashMap<String, String>>,
   sessions: State<'_, SubtitleSessions>,
) -> Result<tauri::ipc::Response> {
   let envelope = subtitle_envelope(
      &sessions,
      &source,
      track_id,
      language,
      start_ms,
      end_ms,
      headers.as_ref(),
   )
   .await?;
   Ok(tauri::ipc::Response::new(envelope))
}

#[cfg(test)]
mod tests {
   use super::*;
   use crate::session_cache::ExpirationPolicy;
   use std::sync::Arc;
   use std::sync::atomic::{AtomicUsize, Ordering};
   use std::time::Duration;

   fn subtitle_fixture_source() -> String {
      std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
         .join("crates/media-parser/tests/fixtures/subtitles.mp4")
         .to_string_lossy()
         .into_owned()
   }

   fn temporary_subtitle_source(label: &str) -> std::path::PathBuf {
      let unique = std::time::SystemTime::now()
         .duration_since(std::time::UNIX_EPOCH)
         .expect("the system clock should follow the Unix epoch")
         .as_nanos();
      std::env::temp_dir().join(format!(
         "media-parser-subtitle-{label}-{}-{unique}.mp4",
         std::process::id()
      ))
   }

   #[test]
   fn subtitle_track_id_takes_precedence_over_language() {
      let request = prepare_subtitle_request(Some(2), Some("eng".to_string()), None, None)
         .expect("filter is valid");

      assert!(matches!(request.filter, Some(TrackFilter::TrackId(2))));
      assert!(!request.first_track_only);
   }

   #[test]
   fn subtitle_track_id_zero_prepares_the_first_track_core_request() {
      let request = prepare_subtitle_request(Some(0), Some("spa".to_string()), None, None)
         .expect("track zero is valid");

      assert!(request.filter.is_none());
      assert!(request.first_track_only);
   }

   #[tokio::test]
   async fn subtitle_track_id_zero_selects_the_first_valid_supported_track() {
      let sessions = SubtitleSessions::default();
      let tracks = subtitle_tracks(
         &sessions,
         &subtitle_fixture_source(),
         Some(0),
         Some("spa".to_string()),
         None,
         None,
         None,
      )
      .await
      .expect("track zero should select the first valid track");

      assert_eq!(tracks.len(), 1);
      assert_eq!(tracks[0].base.id, 1);
   }

   #[test]
   fn subtitle_accepts_the_maximum_u32_track_id_without_a_sentinel() {
      let request = prepare_subtitle_request(Some(u32::MAX), None, None, None)
         .expect("the full Rust track-id domain is valid");

      assert!(matches!(
         request.filter,
         Some(TrackFilter::TrackId(u32::MAX))
      ));
      assert!(!request.first_track_only);
   }

   #[test]
   fn subtitle_range_requires_paired_strictly_increasing_endpoints() {
      for (start, end) in [
         (Some(0), None),
         (None, Some(1)),
         (Some(1), Some(1)),
         (Some(2), Some(1)),
      ] {
         assert!(
            prepare_subtitle_request(None, None, start, end).is_err(),
            "range {start:?}..{end:?} should fail"
         );
      }

      let request = prepare_subtitle_request(None, None, Some(0), Some(1))
         .expect("a paired increasing range is valid");
      assert_eq!(
         request.range,
         Some((Duration::ZERO, Duration::from_millis(1)))
      );
   }

   #[test]
   fn subtitle_range_accepts_javascript_safe_integer_boundary() {
      let request = prepare_subtitle_request(
         None,
         None,
         Some(JS_MAX_SAFE_INTEGER - 1),
         Some(JS_MAX_SAFE_INTEGER),
      )
      .expect("the JavaScript safe integer boundary is valid");

      assert_eq!(
         request.range,
         Some((
            Duration::from_millis(JS_MAX_SAFE_INTEGER - 1),
            Duration::from_millis(JS_MAX_SAFE_INTEGER),
         ))
      );
   }

   #[test]
   fn subtitle_range_rejects_values_above_javascript_safe_integer_boundary() {
      let error = prepare_subtitle_request(
         None,
         None,
         Some(JS_MAX_SAFE_INTEGER),
         Some(JS_MAX_SAFE_INTEGER + 1),
      )
      .expect_err("unsafe millisecond values must fail")
      .to_string();

      assert!(error.contains("safe integer"));
   }

   #[tokio::test]
   async fn subtitle_invalid_and_empty_ranges_fail_before_reader_or_session_work() {
      let sessions = SubtitleSessions::default();
      for (start, end) in [(Some(1), None), (Some(1), Some(1)), (Some(2), Some(1))] {
         let error = subtitle_tracks(
            &sessions,
            "/file/that/does/not/exist.mp4",
            None,
            None,
            start,
            end,
            None,
         )
         .await
         .expect_err("invalid range must fail")
         .to_string();

         assert!(
            error.contains("subtitle range"),
            "unexpected error: {error}"
         );
      }
   }

   #[tokio::test]
   async fn subtitle_no_match_encodes_an_empty_envelope() {
      let sessions = SubtitleSessions::default();
      let envelope = subtitle_envelope(
         &sessions,
         &subtitle_fixture_source(),
         Some(99),
         None,
         None,
         None,
         None,
      )
      .await
      .expect("a missing track is a successful empty result");
      let header_len = u32::from_le_bytes(envelope[..4].try_into().expect("prefix")) as usize;
      let header: serde_json::Value = serde_json::from_slice(&envelope[4..4 + header_len])
         .expect("empty envelope header should be JSON");

      assert_eq!(header, serde_json::json!({ "version": 1, "entries": [] }));
      assert_eq!(envelope.len(), 4 + header_len);
   }

   #[tokio::test]
   async fn subtitle_session_is_reused_source_wide_across_filter_requests() {
      let sessions = SubtitleSessions::default();
      let source = subtitle_fixture_source();

      let first = subtitle_session(&sessions, &source, None)
         .await
         .expect("first session should build");
      let second = subtitle_session(&sessions, &source, None)
         .await
         .expect("the source-wide session should be reused");

      assert!(Arc::ptr_eq(&first, &second));
   }

   #[tokio::test]
   async fn subtitle_concurrent_cold_requests_share_one_session_build() {
      let sessions = Arc::new(SubtitleSessions::default());
      let source = subtitle_fixture_source();
      let request = |sessions: Arc<SubtitleSessions>, source: String| async move {
         subtitle_session(&sessions, &source, None).await
      };

      let (first, second, third) = tokio::time::timeout(Duration::from_secs(10), async {
         tokio::join!(
            request(Arc::clone(&sessions), source.clone()),
            request(Arc::clone(&sessions), source.clone()),
            request(Arc::clone(&sessions), source),
         )
      })
      .await
      .expect("concurrent builds should not hang");
      let first = first.expect("first request");
      let second = second.expect("second request");
      let third = third.expect("third request");

      assert!(Arc::ptr_eq(&first, &second));
      assert!(Arc::ptr_eq(&first, &third));
   }

   #[tokio::test]
   async fn subtitle_session_pool_obeys_ttl_and_lru_bounds() {
      let pool = SessionPool::new(2, Duration::from_secs(60));
      let builds = AtomicUsize::new(0);

      for key in ["first", "second"] {
         pool
            .get_or_try_build(
               key,
               ExpirationPolicy::Absolute(Duration::from_secs(60)),
               || async {
                  builds.fetch_add(1, Ordering::SeqCst);
                  Ok(key)
               },
            )
            .await
            .expect("initial value should build");
      }
      pool
         .get_or_try_build(
            "first",
            ExpirationPolicy::Absolute(Duration::from_secs(60)),
            || async { panic!("recent entry should be reused") },
         )
         .await
         .expect("recent entry should be cached");
      pool
         .get_or_try_build(
            "third",
            ExpirationPolicy::Absolute(Duration::from_secs(60)),
            || async {
               builds.fetch_add(1, Ordering::SeqCst);
               Ok("third")
            },
         )
         .await
         .expect("third value should build");
      pool
         .get_or_try_build(
            "second",
            ExpirationPolicy::Absolute(Duration::ZERO),
            || async {
               builds.fetch_add(1, Ordering::SeqCst);
               Ok("second rebuilt")
            },
         )
         .await
         .expect("least-recently-used value should rebuild");
      pool
         .get_or_try_build(
            "second",
            ExpirationPolicy::Absolute(Duration::from_secs(60)),
            || async {
               builds.fetch_add(1, Ordering::SeqCst);
               Ok("second after expiry")
            },
         )
         .await
         .expect("zero-TTL value should expire");

      assert_eq!(builds.load(Ordering::SeqCst), 5);
   }

   #[tokio::test]
   async fn subtitle_local_file_version_invalidates_the_cached_session() {
      let path = temporary_subtitle_source("invalidation");
      let fixture = std::fs::read(subtitle_fixture_source()).expect("read subtitle fixture");
      std::fs::write(&path, &fixture).expect("write temporary subtitle fixture");
      let source = path.to_string_lossy().into_owned();
      let sessions = SubtitleSessions::default();

      let first = subtitle_session(&sessions, &source, None)
         .await
         .expect("first version should build");
      let mut changed = fixture;
      changed.push(0);
      std::fs::write(&path, changed).expect("change local file version");
      let second = subtitle_session(&sessions, &source, None)
         .await
         .expect("changed version should build a new session");
      std::fs::remove_file(path).expect("remove temporary fixture");

      assert!(!Arc::ptr_eq(&first, &second));
   }

   #[tokio::test]
   async fn subtitle_failed_session_build_cleans_the_lock_and_can_retry() {
      let pool = SessionPool::<&str, usize>::new(1, Duration::from_secs(60));
      let failed = pool
         .get_or_try_build(
            "source",
            ExpirationPolicy::Absolute(Duration::from_secs(60)),
            || async { Err(crate::Error::Custom("expected failure".to_string())) },
         )
         .await;
      assert!(failed.is_err());

      let recovered = pool
         .get_or_try_build(
            "source",
            ExpirationPolicy::Absolute(Duration::from_secs(60)),
            || async { Ok(7) },
         )
         .await
         .expect("a failed source build must not retain or poison its lock");

      assert_eq!(*recovered, 7);
   }
}

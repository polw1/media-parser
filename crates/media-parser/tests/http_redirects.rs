use media_parser::{HttpStreamReader, MediaParserError, StreamReader};
use std::collections::HashMap;
use wiremock::matchers::{any, path};
use wiremock::{Mock, MockServer, Request, ResponseTemplate};

const SENSITIVE_HEADERS: [(&str, &str); 5] = [
   ("Authorization", "Bearer test-token"),
   ("Cookie", "session=test-cookie"),
   ("cookie2", "test-cookie2"),
   ("Proxy-Authorization", "Basic test-proxy-token"),
   ("WWW-Authenticate", "Bearer test-challenge"),
];

fn headers(pairs: &[(&str, &str)]) -> HashMap<String, String> {
   pairs
      .iter()
      .map(|(name, value)| (name.to_string(), value.to_string()))
      .collect()
}

fn media_response(request: &Request) -> ResponseTemplate {
   if request.method == "HEAD" {
      ResponseTemplate::new(200).set_body_bytes(b"data")
   } else {
      assert_eq!(request.method, "GET");
      assert_eq!(request.headers.get("range").unwrap(), "bytes=1-2");
      ResponseTemplate::new(206)
         .insert_header("Content-Range", "bytes 1-2/4")
         .set_body_bytes(b"at")
   }
}

fn assert_headers(request: &Request, expected: &HashMap<String, String>) {
   for (name, value) in expected {
      assert_eq!(request.headers.get(name.as_str()).unwrap(), value.as_str());
   }
}

fn assert_headers_absent(request: &Request, configured: &HashMap<String, String>) {
   for name in configured.keys() {
      assert!(
         !request.headers.contains_key(name.as_str()),
         "{name} was forwarded to {}",
         request.url
      );
   }
}

async fn assert_cross_origin_redirect(
   configured: HashMap<String, String>,
   force_same_origin: bool,
   status: u16,
   forwarded: Option<HashMap<String, String>>,
) {
   let source = MockServer::start().await;
   let target = MockServer::start().await;
   Mock::given(any())
      .respond_with(ResponseTemplate::new(status).insert_header(
         "Location",
         format!("{}/download?signature=test", target.uri()),
      ))
      .mount(&source)
      .await;
   Mock::given(any())
      .respond_with(media_response)
      .mount(&target)
      .await;
   let reader = HttpStreamReader::with_headers_and_redirect_policy(
      &source.uri(),
      configured.clone(),
      force_same_origin,
   )
   .await
   .unwrap();

   let head = reader.size().await;
   let get = reader.read_vec(1, 2).await;
   let initial = source.received_requests().await.unwrap();
   let destination = target.received_requests().await.unwrap();
   assert_eq!(initial.len(), 2);
   assert_eq!(initial[0].method, "HEAD");
   assert_eq!(initial[1].method, "GET");
   for request in &initial {
      assert_headers(request, &configured);
   }

   if let Some(forwarded) = forwarded {
      assert_eq!(head.unwrap(), 4);
      assert_eq!(get.unwrap(), b"at");
      assert_eq!(destination.len(), 2);
      assert_eq!(destination[0].method, "HEAD");
      assert_eq!(destination[1].method, "GET");
      for request in &destination {
         assert_headers(request, &forwarded);
         for name in configured.keys() {
            if !forwarded.contains_key(name) {
               assert!(!request.headers.contains_key(name.as_str()));
            }
         }
         assert_eq!(request.url.path(), "/download");
         assert_eq!(request.url.query(), Some("signature=test"));
      }
   } else {
      assert!(destination.is_empty());
      for error in [head.unwrap_err(), get.unwrap_err()] {
         assert!(
            matches!(error, MediaParserError::HttpRequest(ref reason)
               if reason == "cross-origin redirect blocked: same-origin policy enforced"),
            "unexpected error: {error:?}"
         );
      }
   }
}

#[tokio::test]
async fn cross_origin_redirects_strip_each_sensitive_header_and_combinations() {
   for pair in SENSITIVE_HEADERS {
      assert_cross_origin_redirect(headers(&[pair]), false, 302, Some(HashMap::new())).await;
   }
   assert_cross_origin_redirect(
      headers(&SENSITIVE_HEADERS),
      false,
      302,
      Some(HashMap::new()),
   )
   .await;
   assert_cross_origin_redirect(
      headers(&[("aUtHoRiZaTiOn", "Bearer test"), ("CoOkIe2", "test")]),
      false,
      302,
      Some(HashMap::new()),
   )
   .await;
}

#[tokio::test]
async fn cross_origin_redirect_statuses_preserve_methods_and_strip_headers() {
   for status in [301, 302, 303, 307, 308] {
      assert_cross_origin_redirect(
         headers(&SENSITIVE_HEADERS),
         false,
         status,
         Some(HashMap::new()),
      )
      .await;
   }
}

#[tokio::test]
async fn unrestricted_mixed_headers_follow_reqwest_defaults() {
   for (configured, forwarded) in [
      (
         headers(&[("Authorization", "Bearer test"), ("X-Api-Key", "test-key")]),
         headers(&[("X-Api-Key", "test-key")]),
      ),
      (
         headers(&[
            ("User-Agent", "app/1.0"),
            ("Authorization", "Bearer test"),
            ("Accept", "video/mp4"),
         ]),
         headers(&[("User-Agent", "app/1.0"), ("Accept", "video/mp4")]),
      ),
      (
         headers(&[("Accept", "video/mp4")]),
         headers(&[("Accept", "video/mp4")]),
      ),
   ] {
      assert_cross_origin_redirect(configured, false, 302, Some(forwarded)).await;
   }
   let mut mixed = headers(&SENSITIVE_HEADERS);
   mixed.insert("X-Api-Key".into(), "test-key".into());
   assert_cross_origin_redirect(
      mixed,
      false,
      302,
      Some(headers(&[("X-Api-Key", "test-key")])),
   )
   .await;
}

#[tokio::test]
async fn explicit_origin_restrictions_block_regardless_of_headers() {
   for configured in [
      HashMap::new(),
      headers(&SENSITIVE_HEADERS),
      headers(&[("User-Agent", "app/1.0"), ("Authorization", "Bearer test")]),
      headers(&[("X-Api-Key", "test-key")]),
   ] {
      assert_cross_origin_redirect(configured, true, 302, None).await;
   }
}

#[tokio::test]
async fn same_origin_redirects_preserve_sensitive_and_custom_headers() {
   let sensitive = headers(&SENSITIVE_HEADERS);
   let mut mixed = sensitive.clone();
   mixed.extend(headers(&[
      ("X-Api-Key", "test-key"),
      ("User-Agent", "app/1.0"),
   ]));
   for (configured, force) in [(sensitive, false), (mixed, true)] {
      let server = MockServer::start().await;
      Mock::given(path("/start"))
         .respond_with(ResponseTemplate::new(302).insert_header("Location", "/finish"))
         .mount(&server)
         .await;
      Mock::given(path("/finish"))
         .respond_with(media_response)
         .mount(&server)
         .await;
      let reader = HttpStreamReader::with_headers_and_redirect_policy(
         &format!("{}/start", server.uri()),
         configured.clone(),
         force,
      )
      .await
      .unwrap();
      assert_eq!(reader.size().await.unwrap(), 4);
      assert_eq!(reader.read_vec(1, 2).await.unwrap(), b"at");
      let requests = server.received_requests().await.unwrap();
      assert_eq!(requests.len(), 4);
      for request in &requests {
         assert_headers(request, &configured);
      }
   }
}

#[tokio::test]
async fn stripped_headers_do_not_reappear_when_redirecting_back_to_the_original_origin() {
   let source = MockServer::start().await;
   let target = MockServer::start().await;
   Mock::given(path("/start"))
      .respond_with(ResponseTemplate::new(302).insert_header("Location", "/intermediate"))
      .mount(&source)
      .await;
   Mock::given(path("/intermediate"))
      .respond_with(
         ResponseTemplate::new(302).insert_header("Location", format!("{}/cdn", target.uri())),
      )
      .mount(&source)
      .await;
   Mock::given(path("/cdn"))
      .respond_with(
         ResponseTemplate::new(302).insert_header("Location", format!("{}/return", source.uri())),
      )
      .mount(&target)
      .await;
   Mock::given(path("/return"))
      .respond_with(media_response)
      .mount(&source)
      .await;
   let configured = headers(&SENSITIVE_HEADERS);
   let reader =
      HttpStreamReader::with_headers(&format!("{}/start", source.uri()), configured.clone())
         .await
         .unwrap();

   assert_eq!(reader.size().await.unwrap(), 4);
   assert_eq!(reader.read_vec(1, 2).await.unwrap(), b"at");
   let initial = source.received_requests().await.unwrap();
   let destination = target.received_requests().await.unwrap();
   assert_eq!(initial.len(), 6);
   assert_eq!(destination.len(), 2);
   for request in initial.iter().chain(&destination) {
      if ["/start", "/intermediate"].contains(&request.url.path()) {
         assert_headers(request, &configured);
      } else {
         assert_headers_absent(request, &configured);
      }
   }
}

#[tokio::test]
async fn hostname_change_on_the_same_port_strips_sensitive_headers() {
   let server = MockServer::start().await;
   let target_url = format!("{}/finish", server.uri().replace("127.0.0.1", "localhost"));
   Mock::given(path("/start"))
      .respond_with(ResponseTemplate::new(302).insert_header("Location", target_url))
      .mount(&server)
      .await;
   Mock::given(path("/finish"))
      .respond_with(media_response)
      .mount(&server)
      .await;
   let configured = headers(&SENSITIVE_HEADERS);
   let reader =
      HttpStreamReader::with_headers(&format!("{}/start", server.uri()), configured.clone())
         .await
         .unwrap();
   assert_eq!(reader.size().await.unwrap(), 4);
   assert_eq!(reader.read_vec(1, 2).await.unwrap(), b"at");
   let requests = server.received_requests().await.unwrap();
   assert_eq!(requests.len(), 4);
   for request in &requests {
      if request.url.path() == "/start" {
         assert_headers(request, &configured);
      } else {
         assert_headers_absent(request, &configured);
      }
   }
}

#[tokio::test]
async fn cross_origin_redirects_keep_the_ten_hop_limit() {
   for hops in [10_u32, 11] {
      let a = MockServer::start().await;
      let b = MockServer::start().await;
      for server in [&a, &b] {
         let a_uri = a.uri();
         let b_uri = b.uri();
         Mock::given(any())
            .respond_with(move |request: &Request| {
               let step = request
                  .url
                  .path()
                  .trim_start_matches('/')
                  .parse::<u32>()
                  .unwrap();
               if step == hops {
                  media_response(request)
               } else {
                  let next = if step % 2 == 1 { &a_uri } else { &b_uri };
                  ResponseTemplate::new(302)
                     .insert_header("Location", format!("{next}/{}", step + 1))
               }
            })
            .mount(server)
            .await;
      }
      let configured = headers(&SENSITIVE_HEADERS);
      let reader = HttpStreamReader::with_headers(&format!("{}/0", a.uri()), configured.clone())
         .await
         .unwrap();
      let head = reader.size().await;
      let get = reader.read_vec(1, 2).await;
      let a_requests = a.received_requests().await.unwrap();
      let b_requests = b.received_requests().await.unwrap();
      assert_eq!(a_requests.len() + b_requests.len(), 22);
      if hops == 10 {
         assert_eq!(head.unwrap(), 4);
         assert_eq!(get.unwrap(), b"at");
      } else {
         for error in [head.unwrap_err(), get.unwrap_err()] {
            assert!(
               matches!(error, MediaParserError::HttpRequest(ref reason)
                  if reason.contains("error following redirect") && !reason.contains("same-origin policy")),
               "unexpected error: {error:?}"
            );
         }
         assert!(
            a_requests
               .iter()
               .chain(&b_requests)
               .all(|request| request.url.path() != "/11")
         );
      }
      for request in a_requests.iter().chain(&b_requests) {
         if request.url.path() == "/0" {
            assert_headers(request, &configured);
         } else {
            assert_headers_absent(request, &configured);
         }
      }
   }
}

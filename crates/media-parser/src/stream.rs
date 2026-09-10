//! Stream reading abstraction for media parsing.
//!
//! This module provides an interface for reading media streams from local files and remote
//! HTTP/HTTPS sources with support for direct offset access.
//! This is essential for parsing MP4 files, which require seeking to specific locations
//! to read box headers and payloads.
//!

use crate::errors::{MediaParserError, Result};
use async_trait::async_trait;
use futures::StreamExt;
use reqwest::header::{CONTENT_LENGTH, CONTENT_RANGE, HeaderMap, HeaderName, HeaderValue, RANGE};
use reqwest::{Client, redirect::Policy};
use std::collections::HashMap;
use std::path::Path;
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;

#[cfg(unix)]
use std::os::unix::fs::FileExt;
#[cfg(windows)]
use std::os::windows::fs::FileExt;

/// Internal trait to unify file read_at behavior across platforms
trait FileReadAt {
   fn read_at_offset(&self, buf: &mut [u8], offset: u64) -> std::io::Result<usize>;
}

#[cfg(unix)]
impl FileReadAt for std::fs::File {
   fn read_at_offset(&self, buf: &mut [u8], offset: u64) -> std::io::Result<usize> {
      FileExt::read_at(self, buf, offset)
   }
}

#[cfg(windows)]
impl FileReadAt for std::fs::File {
   fn read_at_offset(&self, buf: &mut [u8], offset: u64) -> std::io::Result<usize> {
      FileExt::seek_read(self, buf, offset)
   }
}

// Constants
/// HTTP status code for partial content (Range request success)
const HTTP_PARTIAL_CONTENT: u16 = 206;
/// HTTP status code for an unsatisfiable Range request.
const HTTP_RANGE_NOT_SATISFIABLE: u16 = 416;
/// Small random reads share a bounded read-ahead window.
const HTTP_READ_AHEAD_BYTES: usize = 32 * 1024;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ContentRange {
   Bytes {
      start: u64,
      end: u64,
      total: Option<u64>,
   },
   Unsatisfied {
      total: u64,
   },
}

fn parse_content_range(value: &str) -> Option<ContentRange> {
   let value = value.strip_prefix("bytes ")?;
   let (range, total) = value.split_once('/')?;
   let total = if total == "*" {
      None
   } else {
      Some(total.parse().ok()?)
   };
   if range == "*" {
      return Some(ContentRange::Unsatisfied { total: total? });
   }
   let (start, end) = range.split_once('-')?;
   let start = start.parse().ok()?;
   let end = end.parse().ok()?;
   (end >= start).then_some(ContentRange::Bytes { start, end, total })
}

/// Copies bytes from `src` into `dst` and returns the count.
fn copy_into(dst: &mut [u8], src: &[u8]) -> usize {
   let len = dst.len().min(src.len());
   dst[..len].copy_from_slice(&src[..len]);
   len
}

pub(crate) fn try_zeroed_bytes(len: usize, context: &'static str) -> Result<Vec<u8>> {
   let mut bytes = Vec::new();
   bytes.try_reserve_exact(len).map_err(|_| {
      MediaParserError::Other(format!("{context} allocation failed for {len} bytes"))
   })?;
   bytes.resize(len, 0);
   Ok(bytes)
}

pub(crate) fn try_copy_bytes(bytes: &[u8], context: &'static str) -> Result<Vec<u8>> {
   let mut copy = try_zeroed_bytes(bytes.len(), context)?;
   copy.copy_from_slice(bytes);
   Ok(copy)
}

/// Provides a interface for reading data at arbitrary offsets
///
/// # Contract
///
/// - `read_at` reads up to `buf.len()` bytes starting at `offset`
/// - Returns the number of bytes read (may be less than `buf.len()` if EOF)
/// - Returns `0` if `offset >= size()`
/// - Partial reads are allowed

#[async_trait]
pub trait StreamReader: Send + Sync {
   /// Reads data at the specified offset into the buffer.
   ///
   /// Reads up to `buf.len()` bytes starting at `offset`. Returns the number of bytes
   /// actually read, which may be less than `buf.len()` if EOF is reached. Returns `0`
   /// if `offset >= size()` or if `buf.is_empty()`.
   async fn read_at(&self, offset: u64, buf: &mut [u8]) -> Result<usize>;

   /// Reads up to `len` bytes at the specified offset into a new buffer.
   ///
   /// The returned buffer is truncated to the number of bytes actually read,
   /// which may be less than `len` if EOF is reached, and is empty if
   /// `offset >= size()`. The default implementation reads through `read_at`;
   /// implementations may override it to avoid an intermediate copy.
   async fn read_vec(&self, offset: u64, len: usize) -> Result<Vec<u8>> {
      let mut buf = try_zeroed_bytes(len, "stream read buffer")?;
      let read = self.read_at(offset, &mut buf).await?;
      buf.truncate(read);
      Ok(buf)
   }

   /// Returns the total size of the stream in bytes.
   async fn size(&self) -> Result<u64>;
}

#[async_trait]
impl<T: StreamReader + ?Sized> StreamReader for &T {
   async fn read_at(&self, offset: u64, buf: &mut [u8]) -> Result<usize> {
      (**self).read_at(offset, buf).await
   }

   async fn read_vec(&self, offset: u64, len: usize) -> Result<Vec<u8>> {
      (**self).read_vec(offset, len).await
   }

   async fn size(&self) -> Result<u64> {
      (**self).size().await
   }
}

#[async_trait]
impl<T: StreamReader + ?Sized> StreamReader for Arc<T> {
   async fn read_at(&self, offset: u64, buf: &mut [u8]) -> Result<usize> {
      (**self).read_at(offset, buf).await
   }

   async fn read_vec(&self, offset: u64, len: usize) -> Result<Vec<u8>> {
      (**self).read_vec(offset, len).await
   }

   async fn size(&self) -> Result<u64> {
      (**self).size().await
   }
}

/// `StreamReader` implementation backed by a local file handle.
///
/// # Examples
///
/// ```no_run
/// use media_parser::{FileStreamReader, StreamReader};
///
/// # async fn example() -> media_parser::Result<()> {
/// let reader = FileStreamReader::new("video.mp4")?;
/// let size = reader.size().await?;
///
/// let mut buffer = vec![0u8; 1024];
/// let bytes_read = reader.read_at(0, &mut buffer).await?;
/// # Ok(())
/// # }
/// ```
pub struct FileStreamReader {
   file: Arc<std::fs::File>,
   cached_size: OnceLock<u64>,
}

impl FileStreamReader {
   /// Opens a file at the given path for random-access reading.
   ///
   /// Returns an error if the file does not exist or cannot be opened.
   pub fn new<P: AsRef<Path>>(path: P) -> Result<Self> {
      let file = Arc::new(std::fs::File::open(path).map_err(MediaParserError::Io)?);
      Ok(Self {
         file,
         cached_size: OnceLock::new(),
      })
   }

   /// Performs a blocking `read_at` directly into the provided buffer.
   fn sync_read_into(file: &std::fs::File, offset: u64, buf: &mut [u8]) -> Result<usize> {
      let mut read_total = 0usize;
      while read_total < buf.len() {
         let read_offset = offset
            .checked_add(
               u64::try_from(read_total)
                  .map_err(|_| MediaParserError::Other("file read offset exceeds u64".into()))?,
            )
            .ok_or_else(|| MediaParserError::Other("file read offset overflow".into()))?;
         let n = file
            .read_at_offset(&mut buf[read_total..], read_offset)
            .map_err(MediaParserError::Io)?;
         if n == 0 {
            break;
         }
         read_total += n;
      }
      Ok(read_total)
   }
}

#[async_trait]
impl StreamReader for FileStreamReader {
   /// Reads data from the file at the specified offset.
   async fn read_at(&self, offset: u64, buf: &mut [u8]) -> Result<usize> {
      if buf.is_empty() {
         return Ok(0);
      }

      let data = self.read_vec(offset, buf.len()).await?;
      Ok(copy_into(buf, &data))
   }

   /// Reads data from the file at the specified offset into a new buffer.
   ///
   /// Reads directly into the returned buffer inside the blocking task, avoiding
   /// the extra copy `read_at` needs to fill a caller-provided buffer.
   async fn read_vec(&self, offset: u64, len: usize) -> Result<Vec<u8>> {
      if len == 0 {
         return Ok(Vec::new());
      }

      let file = Arc::clone(&self.file);

      let buf = tokio::task::spawn_blocking(move || {
         let mut buf = try_zeroed_bytes(len, "file stream read buffer")?;
         let read = Self::sync_read_into(&file, offset, &mut buf)?;
         buf.truncate(read);
         Ok::<_, MediaParserError>(buf)
      })
      .await
      .map_err(|e| MediaParserError::BlockingTask(format!("spawn_blocking failed: {}", e)))??;

      Ok(buf)
   }

   /// Returns the file size in bytes.
   ///
   /// The size is cached after the first call.
   async fn size(&self) -> Result<u64> {
      // Try to get cached value (lock-free read)
      if let Some(size) = self.cached_size.get() {
         return Ok(*size);
      }

      // Cache miss - fetch metadata
      let file = Arc::clone(&self.file);

      let size = tokio::task::spawn_blocking(move || {
         file
            .metadata()
            .map_err(MediaParserError::Io)
            .map(|m| m.len())
      })
      .await
      .map_err(|e| MediaParserError::BlockingTask(format!("spawn_blocking failed: {}", e)))??;

      if self.cached_size.set(size).is_err()
         && let Some(existing) = self.cached_size.get()
      {
         return Ok(*existing);
      }

      Ok(size)
   }
}

/// `StreamReader` implementation that issues HTTP range requests.
///
/// Learns the stream size lazily from range responses. Optional custom headers
/// can be provided for authentication or other metadata.
///
/// # Examples
///
/// Reading without custom headers:
///
/// ```no_run
/// use media_parser::{HttpStreamReader, StreamReader};
///
/// # async fn example() -> media_parser::Result<()> {
/// let reader = HttpStreamReader::new("https://example.com/video.mp4").await?;
/// let mut buf = vec![0u8; 1024];
/// let n = reader.read_at(0, &mut buf).await?;
/// println!("read {} bytes", n);
/// # Ok(())
/// # }
/// ```
///
/// Reading with custom headers (e.g., authentication):
///
/// ```no_run
/// use media_parser::{HttpStreamReader, StreamReader};
/// use std::collections::HashMap;
///
/// # async fn example() -> media_parser::Result<()> {
/// let mut headers = HashMap::new();
/// headers.insert("Authorization".into(), "Bearer token123".into());
///
/// let reader = HttpStreamReader::with_headers("https://example.com/video.mp4", headers).await?;
/// let mut buf = vec![0u8; 4096];
/// let bytes = reader.read_at(0, &mut buf).await?;
/// println!("read {} bytes", bytes);
/// # Ok(())
/// # }
/// ```
pub struct HttpStreamReader {
   url: String,
   client: Client,
   cached_size: OnceLock<u64>,
   read_window: Mutex<Option<ReadWindow>>,
}

struct ReadWindow {
   offset: u64,
   data: Vec<u8>,
}

impl HttpStreamReader {
   /// Creates a new `HttpStreamReader` for the given URL.
   pub async fn new(url: &str) -> Result<Self> {
      Self::build_with_headers(url, HeaderMap::new()).await
   }

   /// Creates a new `HttpStreamReader` with custom HTTP headers.
   ///
   /// Custom headers (e.g., for authentication) will be sent with all requests.
   /// At most 64 entries are accepted. Redirects to another origin are stopped
   /// when any headers are configured; same-origin redirects are limited to ten hops.
   pub async fn with_headers(url: &str, headers: HashMap<String, String>) -> Result<Self> {
      if headers.len() > 64 {
         return Err(MediaParserError::HttpRequest(
            "At most 64 HTTP headers are supported".into(),
         ));
      }
      let mut header_map = HeaderMap::new();
      for (k, v) in headers {
         let header_name = HeaderName::try_from(k.as_str()).map_err(|e| {
            MediaParserError::HttpRequest(format!("Invalid header name '{}': {}", k, e))
         })?;
         let header_value = HeaderValue::from_str(v.as_str()).map_err(|e| {
            MediaParserError::HttpRequest(format!("Invalid header value for '{}': {}", k, e))
         })?;
         if header_map.insert(header_name, header_value).is_some() {
            return Err(MediaParserError::HttpRequest(format!(
               "duplicate header name ignoring case: '{k}'"
            )));
         }
      }
      Self::build_with_headers(url, header_map).await
   }

   async fn build_with_headers(url: &str, headers: HeaderMap) -> Result<Self> {
      let mut builder = Client::builder();
      if !headers.is_empty() {
         builder = builder.redirect(Policy::custom(|attempt| {
            if attempt
               .previous()
               .last()
               .is_some_and(|previous| previous.origin() == attempt.url().origin())
            {
               Policy::default().redirect(attempt)
            } else {
               attempt.stop()
            }
         }));
      }
      let builder = builder
         .default_headers(headers)
         .timeout(Duration::from_secs(30));
      #[cfg(target_os = "android")]
      let builder = builder.tls_certs_only(bundled_tls_certificates()?);
      let client = builder
         .build()
         .map_err(|e| MediaParserError::HttpRequest(format!("Failed to build client: {}", e)))?;

      Ok(Self {
         url: url.to_string(),
         client,
         cached_size: OnceLock::new(),
         read_window: Mutex::new(None),
      })
   }

   fn cache_size(&self, size: u64) {
      let _ = self.cached_size.set(size);
   }

   fn response_content_range(headers: &HeaderMap) -> Result<ContentRange> {
      headers
         .get(CONTENT_RANGE)
         .and_then(|header| header.to_str().ok())
         .and_then(parse_content_range)
         .ok_or_else(|| {
            MediaParserError::HttpRequest("missing or invalid Content-Range response".to_string())
         })
   }

   fn copy_from_read_window(&self, offset: u64, buf: &mut [u8]) -> Option<usize> {
      let window = self.read_window.lock().ok()?;
      let window = window.as_ref()?;
      let relative_offset = usize::try_from(offset.checked_sub(window.offset)?).ok()?;
      let available = window.data.get(relative_offset..)?;
      let window_end = window
         .offset
         .checked_add(u64::try_from(window.data.len()).ok()?);
      let reaches_eof = window_end
         .zip(self.cached_size.get().copied())
         .is_some_and(|(end, size)| end == size);
      if available.len() < buf.len() && !reaches_eof {
         return None;
      }
      Some(copy_into(buf, available))
   }

   fn store_read_window(&self, offset: u64, data: Vec<u8>) {
      if let Ok(mut window) = self.read_window.lock() {
         *window = Some(ReadWindow { offset, data });
      }
   }

   /// Performs an HTTP Range request and streams data into the buffer.
   /// # Arguments
   /// * `start` - Start byte offset (inclusive)
   /// * `end` - End byte offset (inclusive)
   /// * `buf` - Buffer to fill with the response data
   async fn fetch_range_stream(&self, start: u64, end: u64, buf: &mut [u8]) -> Result<usize> {
      // Validate range: end must be >= start
      if end < start {
         return Err(MediaParserError::InvalidFormat(format!(
            "Invalid range: end ({}) < start ({})",
            end, start
         )));
      }

      // Format: "bytes={start}-{end}" (end is inclusive in HTTP Range requests)
      let range_header = format!("bytes={}-{}", start, end);
      let req = self.client.get(&self.url).header(RANGE, range_header);

      let resp = req
         .send()
         .await
         .map_err(|e| MediaParserError::HttpRequest(format!("GET request failed: {}", e)))?;

      let status = resp.status();
      let expected_body_length = match status.as_u16() {
         HTTP_PARTIAL_CONTENT => {
            let ContentRange::Bytes {
               start: actual_start,
               end: actual_end,
               total,
            } = Self::response_content_range(resp.headers())?
            else {
               return Err(MediaParserError::HttpRequest(
                  "invalid Content-Range for a partial response".to_string(),
               ));
            };
            if actual_start != start || actual_end > end {
               return Err(MediaParserError::HttpRequest(format!(
                  "Content-Range bytes {actual_start}-{actual_end} does not match requested range {start}-{end}"
               )));
            }
            if let Some(total) = total {
               if actual_end >= total {
                  return Err(MediaParserError::HttpRequest(
                     "Content-Range exceeds the reported stream size".to_string(),
                  ));
               }
               self.cache_size(total);
            }
            Some(usize::try_from(actual_end - actual_start + 1).map_err(|_| {
               MediaParserError::HttpRequest(
                  "partial response body length exceeds usize".to_string(),
               )
            })?)
         }
         HTTP_RANGE_NOT_SATISFIABLE => {
            let ContentRange::Unsatisfied { total } = Self::response_content_range(resp.headers())?
            else {
               return Err(MediaParserError::HttpRequest(
                  "invalid Content-Range for an unsatisfiable response".to_string(),
               ));
            };
            self.cache_size(total);
            if start >= total {
               return Ok(0);
            }
            return Err(MediaParserError::HttpStatus(HTTP_RANGE_NOT_SATISFIABLE));
         }
         200 => {
            if start != 0 {
               return Err(MediaParserError::HttpRequest(format!(
                  "server ignored the requested byte range {start}-{end}"
               )));
            }
            if let Some(size) = resp
               .headers()
               .get(CONTENT_LENGTH)
               .and_then(|header| header.to_str().ok())
               .and_then(|value| value.parse().ok())
            {
               self.cache_size(size);
            }
            None
         }
         code => return Err(MediaParserError::HttpStatus(code)),
      };

      // Stream data directly into buffer
      let mut stream = resp.bytes_stream();
      let mut total_read = 0usize;

      while let Some(chunk_result) = stream.next().await {
         let chunk = chunk_result
            .map_err(|e| MediaParserError::HttpRequest(format!("Failed to read chunk: {}", e)))?;

         let written = copy_into(&mut buf[total_read..], &chunk);
         total_read += written;

         // Buffer is full or chunk exceeded remaining capacity.
         if written < chunk.len() || total_read >= buf.len() {
            break;
         }
      }

      if expected_body_length.is_some_and(|expected| total_read != expected) {
         return Err(MediaParserError::HttpRequest(format!(
            "partial response body length mismatch: expected {} bytes, received {total_read}",
            expected_body_length.unwrap_or(0)
         )));
      }
      Ok(total_read)
   }

   async fn read_at_uncached(&self, offset: u64, buf: &mut [u8]) -> Result<usize> {
      if self.cached_size.get().is_some_and(|size| offset >= *size) {
         return Ok(0);
      }

      let mut total_read = 0usize;
      let mut current_offset = offset;
      while total_read < buf.len() {
         let known_size = self.cached_size.get().copied();
         if known_size.is_some_and(|size| current_offset >= size) {
            break;
         }
         let remaining = buf.len() - total_read;
         let to_read = known_size
            .map(|size| remaining.min(usize::try_from(size - current_offset).unwrap_or(usize::MAX)))
            .unwrap_or(remaining);
         let start = current_offset;
         let end = current_offset
            .checked_add(
               u64::try_from(to_read)
                  .map_err(|_| MediaParserError::Other("HTTP read length exceeds u64".into()))?,
            )
            .and_then(|end| end.checked_sub(1))
            .ok_or_else(|| MediaParserError::Other("HTTP range overflow".into()))?;
         let bytes_read = self
            .fetch_range_stream(start, end, &mut buf[total_read..total_read + to_read])
            .await?;
         if bytes_read == 0 {
            break;
         }
         total_read += bytes_read;
         current_offset = current_offset
            .checked_add(
               u64::try_from(bytes_read)
                  .map_err(|_| MediaParserError::Other("HTTP read length exceeds u64".into()))?,
            )
            .ok_or_else(|| MediaParserError::Other("HTTP read offset overflow".into()))?;
      }
      Ok(total_read)
   }
}

#[cfg(target_os = "android")]
fn bundled_tls_certificates() -> Result<Vec<reqwest::Certificate>> {
   webpki_root_certs::TLS_SERVER_ROOT_CERTS
      .iter()
      .map(|certificate| reqwest::Certificate::from_der(certificate.as_ref()))
      .collect::<reqwest::Result<Vec<_>>>()
      .map_err(|error| MediaParserError::HttpRequest(format!("Invalid bundled TLS root: {error}")))
}

#[async_trait]
impl StreamReader for HttpStreamReader {
   /// Reads data from the HTTP stream at the specified offset.
   ///
   /// Uses HTTP `Range` requests with streaming to read data efficiently.
   /// Handles partial reads by retrying to fetch the remaining data if needed.
   async fn read_at(&self, offset: u64, buf: &mut [u8]) -> Result<usize> {
      if buf.is_empty() {
         return Ok(0);
      }

      if buf.len() <= HTTP_READ_AHEAD_BYTES {
         if let Some(read) = self.copy_from_read_window(offset, buf) {
            return Ok(read);
         }
         let read_ahead = self
            .cached_size
            .get()
            .copied()
            .map(|size| {
               usize::try_from(size.saturating_sub(offset))
                  .unwrap_or(usize::MAX)
                  .min(HTTP_READ_AHEAD_BYTES)
            })
            .unwrap_or(HTTP_READ_AHEAD_BYTES);
         let mut window = try_zeroed_bytes(read_ahead, "HTTP read-ahead buffer")?;
         let read = self.read_at_uncached(offset, &mut window).await?;
         window.truncate(read);
         let copied = copy_into(buf, &window);
         if read != 0 {
            self.store_read_window(offset, window);
         }
         return Ok(copied);
      }
      self.read_at_uncached(offset, buf).await
   }

   /// Reads directly into a new buffer without populating the navigation read-ahead window.
   async fn read_vec(&self, offset: u64, len: usize) -> Result<Vec<u8>> {
      let mut buf = try_zeroed_bytes(len, "HTTP read buffer")?;
      let read = self.read_at_uncached(offset, &mut buf).await?;
      buf.truncate(read);
      Ok(buf)
   }

   /// Returns the total size of the HTTP stream.
   async fn size(&self) -> Result<u64> {
      if let Some(size) = self.cached_size.get() {
         return Ok(*size);
      }

      let response =
         self.client.head(&self.url).send().await.map_err(|error| {
            MediaParserError::HttpRequest(format!("HEAD request failed: {error}"))
         })?;
      if !response.status().is_success() {
         return Err(MediaParserError::HttpStatus(response.status().as_u16()));
      }
      let size = response
         .headers()
         .get(CONTENT_LENGTH)
         .and_then(|header| header.to_str().ok())
         .and_then(|value| value.parse().ok())
         .ok_or(MediaParserError::ContentLengthMissing)?;
      self.cache_size(size);
      Ok(self.cached_size.get().copied().unwrap_or(size))
   }
}

#[cfg(test)]
mod tests {
   use super::*;
   use std::io::Write;
   use tempfile::NamedTempFile;

   // Test content reused across tests
   const TEST_CONTENT: &[u8] =
      b"All things, therefore, that you want men to do to you, you also must do to them.";
   const EXPECTED_HTTP_READ_AHEAD_BYTES: usize = 32 * 1024;

   fn create_test_file(content: &[u8]) -> NamedTempFile {
      let mut file = NamedTempFile::new().unwrap();
      file.write_all(content).unwrap();
      file.flush().unwrap();
      file
   }

   #[test]
   fn fallible_byte_buffer_reports_capacity_overflow() {
      let error = try_zeroed_bytes(usize::MAX, "test byte buffer")
         .expect_err("usize::MAX cannot be represented as a Vec capacity");

      assert!(
         matches!(error, MediaParserError::Other(message) if message.contains("test byte buffer allocation failed"))
      );
   }

   #[test]
   fn test_content_range_rejects_an_invalid_total() {
      assert_eq!(parse_content_range("bytes 0-3/not-a-size"), None);
   }

   #[tokio::test]
   async fn test_read_at_beginning() {
      let test_file = create_test_file(TEST_CONTENT);
      let reader = FileStreamReader::new(test_file.path()).unwrap();

      let mut buffer = vec![0u8; TEST_CONTENT.len()];
      let bytes_read = reader.read_at(0, &mut buffer).await.unwrap();

      assert_eq!(bytes_read, TEST_CONTENT.len());
      assert_eq!(&buffer[..bytes_read], TEST_CONTENT);
   }

   // HttpStreamReader tests
   use wiremock::matchers::{header, method};
   use wiremock::{Mock, MockServer, ResponseTemplate};

   #[tokio::test]
   async fn test_http_header_count_is_limited_before_building_the_client() {
      for count in [64, 65, 24_576] {
         let headers = (0..count)
            .map(|i| (format!("x-test-{i}"), "v".into()))
            .collect();
         let result = HttpStreamReader::with_headers("https://example.com/file", headers).await;
         if count == 64 {
            assert!(result.is_ok());
         } else {
            assert!(
               matches!(result, Err(MediaParserError::HttpRequest(message)) if message.contains("64"))
            );
         }
      }
   }

   #[tokio::test]
   async fn test_http_cross_origin_redirects_require_no_configured_headers() {
      for configured in [true, false] {
         let source = MockServer::start().await;
         let target = MockServer::start().await;
         Mock::given(wiremock::matchers::any())
            .respond_with(ResponseTemplate::new(302).insert_header("Location", target.uri()))
            .expect(2)
            .mount(&source)
            .await;
         Mock::given(wiremock::matchers::any())
            .respond_with(ResponseTemplate::new(200).set_body_bytes(b"data"))
            .expect(if configured { 0 } else { 2 })
            .mount(&target)
            .await;
         let headers = if configured {
            HashMap::from([("X-Api-Key".into(), "test-secret".into())])
         } else {
            HashMap::new()
         };
         let reader = HttpStreamReader::with_headers(&source.uri(), headers)
            .await
            .unwrap();
         let size = reader.size().await;
         let bytes = reader.read_vec(0, 4).await;
         if configured {
            assert!(matches!(size, Err(MediaParserError::HttpStatus(302))));
            assert!(matches!(bytes, Err(MediaParserError::HttpStatus(302))));
         } else {
            assert_eq!(size.unwrap(), 4);
            assert_eq!(bytes.unwrap(), b"data");
         }
      }
   }

   #[tokio::test]
   async fn test_http_same_origin_redirects_keep_headers_and_the_ten_hop_limit() {
      let server = MockServer::start().await;
      Mock::given(header("X-Api-Key", "test-secret"))
         .respond_with(|request: &wiremock::Request| {
            let step: usize = request.url.path().trim_start_matches('/').parse().unwrap();
            if step == 11 {
               ResponseTemplate::new(200).set_body_bytes(b"data")
            } else {
               ResponseTemplate::new(302).insert_header("Location", format!("/{}", step + 1))
            }
         })
         .mount(&server)
         .await;
      for (start, allowed) in [(1, true), (0, false)] {
         let reader = HttpStreamReader::with_headers(
            &format!("{}/{start}", server.uri()),
            HashMap::from([("X-Api-Key".into(), "test-secret".into())]),
         )
         .await
         .unwrap();
         let result = reader.size().await;
         if allowed {
            assert_eq!(result.unwrap(), 4);
            assert_eq!(reader.read_vec(0, 4).await.unwrap(), b"data");
         } else {
            assert!(
               matches!(result, Err(MediaParserError::HttpRequest(message)) if message.contains("redirect"))
            );
         }
      }
   }

   #[tokio::test]
   async fn test_http_read_at_beginning() {
      let mock_server = MockServer::start().await;

      // Mock GET request with Range header for reading from beginning
      let range_header = format!("bytes=0-{}", EXPECTED_HTTP_READ_AHEAD_BYTES - 1);
      let range_resp_header = format!("bytes 0-{}/{}", TEST_CONTENT.len() - 1, TEST_CONTENT.len());

      Mock::given(method("GET"))
         .and(header("Range", range_header.as_str()))
         .respond_with(
            ResponseTemplate::new(206)
               .set_body_bytes(TEST_CONTENT)
               .insert_header("Content-Range", range_resp_header.as_str()),
         )
         .mount(&mock_server)
         .await;

      let reader = HttpStreamReader::new(&mock_server.uri()).await.unwrap();

      let mut buffer = vec![0u8; TEST_CONTENT.len()];
      let bytes_read = reader.read_at(0, &mut buffer).await.unwrap();

      assert_eq!(bytes_read, TEST_CONTENT.len());
      assert_eq!(&buffer[..bytes_read], TEST_CONTENT);
      assert_eq!(reader.size().await.unwrap(), TEST_CONTENT.len() as u64);
   }

   #[tokio::test]
   async fn test_http_read_at_offset() {
      let html_body = format!(
         "<html><body><p>{}</p></body></html>",
         String::from_utf8_lossy(TEST_CONTENT)
      );
      let html_bytes = html_body.as_bytes();
      let mock_server = MockServer::start().await;

      let expected_str = "you also must do to them";
      let expected = expected_str.as_bytes();
      let range_start = html_body
         .find(expected_str)
         .expect("phrase should be present in HTML") as u64;
      let range_end = html_bytes.len() as u64 - 1;
      let requested_end = range_start + EXPECTED_HTTP_READ_AHEAD_BYTES as u64 - 1;
      let range_header = format!("bytes={}-{}", range_start, requested_end);
      let range_resp_header = format!("bytes {}-{}/{}", range_start, range_end, html_bytes.len());

      Mock::given(method("GET"))
         .and(header("Range", range_header.as_str()))
         .respond_with(
            ResponseTemplate::new(206)
               .set_body_bytes(&html_bytes[range_start as usize..])
               .insert_header("Content-Range", range_resp_header.as_str()),
         )
         .mount(&mock_server)
         .await;

      let reader = HttpStreamReader::new(&mock_server.uri()).await.unwrap();

      let mut buffer = vec![0u8; expected.len()];
      let bytes_read = reader.read_at(range_start, &mut buffer).await.unwrap();

      assert_eq!(bytes_read, expected.len());
      assert_eq!(&buffer[..bytes_read], expected);
   }

   #[tokio::test]
   async fn test_http_read_vec_requests_only_the_requested_range() {
      let mock_server = MockServer::start().await;
      let offset = 8u64;
      let len = 4usize;
      let range_end = offset + len as u64 - 1;
      let requested_range = format!("bytes={offset}-{range_end}");
      let content_range = format!("bytes {offset}-{range_end}/{}", TEST_CONTENT.len());
      Mock::given(method("GET"))
         .and(header("Range", requested_range.as_str()))
         .respond_with(
            ResponseTemplate::new(206)
               .set_body_bytes(&TEST_CONTENT[offset as usize..=range_end as usize])
               .insert_header("Content-Range", content_range.as_str()),
         )
         .expect(1)
         .mount(&mock_server)
         .await;

      let reader = HttpStreamReader::new(&mock_server.uri()).await.unwrap();
      let reader: &dyn StreamReader = &reader;

      let bytes = reader.read_vec(offset, len).await.unwrap();

      assert_eq!(bytes, TEST_CONTENT[offset as usize..=range_end as usize]);
   }

   #[tokio::test]
   async fn test_http_reuses_a_read_ahead_window_for_nearby_reads() {
      let mock_server = MockServer::start().await;
      let content_range = format!("bytes 0-{}/{}", TEST_CONTENT.len() - 1, TEST_CONTENT.len());
      let requested_range = format!("bytes=0-{}", EXPECTED_HTTP_READ_AHEAD_BYTES - 1);
      Mock::given(method("GET"))
         .and(header("Range", requested_range.as_str()))
         .respond_with(
            ResponseTemplate::new(206)
               .set_body_bytes(TEST_CONTENT)
               .insert_header("Content-Range", content_range.as_str()),
         )
         .expect(1)
         .mount(&mock_server)
         .await;

      let reader = HttpStreamReader::new(&mock_server.uri()).await.unwrap();
      let mut first = [0; 4];
      let mut second = [0; 4];

      assert_eq!(reader.read_at(0, &mut first).await.unwrap(), first.len());
      assert_eq!(reader.read_at(4, &mut second).await.unwrap(), second.len());
      assert_eq!(&first, &TEST_CONTENT[..4]);
      assert_eq!(&second, &TEST_CONTENT[4..8]);
   }

   #[tokio::test]
   async fn test_http_stream_reader_construction_is_lazy() {
      let invalid_url = "http://localhost:1/invalid";
      let reader = HttpStreamReader::new(invalid_url)
         .await
         .expect("construction must not perform a request");

      let mut buffer = [0; 16];
      let result = reader.read_at(0, &mut buffer).await;

      assert!(matches!(result, Err(MediaParserError::HttpRequest(_))));
   }

   #[tokio::test]
   async fn test_http_rejects_duplicate_header_names_ignoring_case() {
      let headers = HashMap::from([
         ("Authorization".to_string(), "Bearer first".to_string()),
         ("authorization".to_string(), "Bearer second".to_string()),
      ]);

      let result = HttpStreamReader::with_headers("https://example.com/video.mp4", headers).await;

      assert!(
         matches!(result, Err(MediaParserError::HttpRequest(message)) if message.contains("duplicate header name"))
      );
   }

   #[tokio::test]
   async fn test_http_size_falls_back_to_head_before_the_first_read() {
      let mock_server = MockServer::start().await;
      let content_len = TEST_CONTENT.len().to_string();
      Mock::given(method("HEAD"))
         .respond_with(
            ResponseTemplate::new(200).insert_header("Content-Length", content_len.as_str()),
         )
         .expect(1)
         .mount(&mock_server)
         .await;

      let reader = HttpStreamReader::new(&mock_server.uri()).await.unwrap();

      assert_eq!(reader.size().await.unwrap(), TEST_CONTENT.len() as u64);
      assert_eq!(reader.size().await.unwrap(), TEST_CONTENT.len() as u64);
   }

   #[tokio::test]
   async fn test_http_range_not_satisfiable_reports_eof_and_learns_size() {
      let mock_server = MockServer::start().await;
      let content_range = format!("bytes */{}", TEST_CONTENT.len());
      let offset = TEST_CONTENT.len() as u64 + 10;
      let requested_range = format!(
         "bytes={}-{}",
         offset,
         offset + EXPECTED_HTTP_READ_AHEAD_BYTES as u64 - 1
      );
      Mock::given(method("GET"))
         .and(header("Range", requested_range.as_str()))
         .respond_with(
            ResponseTemplate::new(416).insert_header("Content-Range", content_range.as_str()),
         )
         .expect(1)
         .mount(&mock_server)
         .await;

      let reader = HttpStreamReader::new(&mock_server.uri()).await.unwrap();
      let mut buffer = [0; 16];

      assert_eq!(reader.read_at(offset, &mut buffer).await.unwrap(), 0);
      assert_eq!(reader.size().await.unwrap(), TEST_CONTENT.len() as u64);
   }

   #[tokio::test]
   async fn test_http_rejects_mismatched_content_range() {
      let mock_server = MockServer::start().await;
      Mock::given(method("GET"))
         .and(header(
            "Range",
            format!("bytes=10-{}", 10 + EXPECTED_HTTP_READ_AHEAD_BYTES - 1).as_str(),
         ))
         .respond_with(
            ResponseTemplate::new(206)
               .set_body_bytes(b"data")
               .insert_header("Content-Range", "bytes 0-3/100"),
         )
         .mount(&mock_server)
         .await;

      let reader = HttpStreamReader::new(&mock_server.uri()).await.unwrap();
      let mut buffer = [0; 4];
      let error = reader.read_at(10, &mut buffer).await.unwrap_err();

      assert!(
         matches!(error, MediaParserError::HttpRequest(message) if message.contains("Content-Range"))
      );
   }

   #[tokio::test]
   async fn test_http_rejects_a_truncated_partial_response() {
      let mock_server = MockServer::start().await;
      Mock::given(method("GET"))
         .and(header(
            "Range",
            format!("bytes=0-{}", EXPECTED_HTTP_READ_AHEAD_BYTES - 1).as_str(),
         ))
         .respond_with(
            ResponseTemplate::new(206)
               .set_body_bytes(b"ab")
               .insert_header("Content-Range", "bytes 0-3/100"),
         )
         .mount(&mock_server)
         .await;

      let reader = HttpStreamReader::new(&mock_server.uri()).await.unwrap();
      let mut buffer = [0; 4];
      let error = reader.read_at(0, &mut buffer).await.unwrap_err();

      assert!(
         matches!(error, MediaParserError::HttpRequest(message) if message.contains("body length"))
      );
   }

   #[tokio::test]
   async fn test_http_rejects_server_ignoring_nonzero_range() {
      let mock_server = MockServer::start().await;
      Mock::given(method("GET"))
         .and(header(
            "Range",
            format!("bytes=10-{}", 10 + EXPECTED_HTTP_READ_AHEAD_BYTES - 1).as_str(),
         ))
         .respond_with(ResponseTemplate::new(200).set_body_bytes(TEST_CONTENT))
         .mount(&mock_server)
         .await;

      let reader = HttpStreamReader::new(&mock_server.uri()).await.unwrap();
      let mut buffer = [0; 4];
      let error = reader.read_at(10, &mut buffer).await.unwrap_err();

      assert!(
         matches!(error, MediaParserError::HttpRequest(message) if message.contains("ignored"))
      );
   }

   #[tokio::test]
   async fn test_file_read_vec_truncates_at_eof() {
      let test_file = create_test_file(TEST_CONTENT);
      let reader = FileStreamReader::new(test_file.path()).unwrap();

      // Ask for more bytes than remain; the result is truncated to EOF
      let data = reader
         .read_vec(TEST_CONTENT.len() as u64 - 4, 100)
         .await
         .unwrap();
      assert_eq!(data, &TEST_CONTENT[TEST_CONTENT.len() - 4..]);

      // Reads starting beyond EOF return an empty buffer
      let data = reader
         .read_vec(TEST_CONTENT.len() as u64 + 100, 16)
         .await
         .unwrap();
      assert!(data.is_empty());
   }

   #[tokio::test]
   async fn test_file_read_at_offset() {
      let test_file = create_test_file(TEST_CONTENT);
      let reader = FileStreamReader::new(test_file.path()).unwrap();

      // Verify size
      let size = reader.size().await.unwrap();
      assert_eq!(size, TEST_CONTENT.len() as u64);

      // Read 16 bytes starting at offset 37: "men to do to you"
      let expected = b"men to do to you";
      let mut buffer = vec![0u8; expected.len()];
      let bytes_read = reader.read_at(37, &mut buffer).await.unwrap();

      assert_eq!(bytes_read, expected.len());
      assert_eq!(&buffer[..bytes_read], expected);
   }

   #[tokio::test]
   async fn test_file_read_beyond_eof() {
      let test_file = create_test_file(TEST_CONTENT);
      let reader = FileStreamReader::new(test_file.path()).unwrap();

      let mut buffer = vec![0u8; 100];

      // Read beyond EOF returns 0
      let bytes_read = reader
         .read_at(TEST_CONTENT.len() as u64 + 100, &mut buffer)
         .await
         .unwrap();
      assert_eq!(bytes_read, 0);
   }

   #[tokio::test]
   async fn test_file_read_empty_buffer() {
      let test_file = create_test_file(TEST_CONTENT);
      let reader = FileStreamReader::new(test_file.path()).unwrap();

      let mut buffer: Vec<u8> = vec![];
      let bytes_read = reader.read_at(0, &mut buffer).await.unwrap();

      assert_eq!(bytes_read, 0);
   }

   #[tokio::test]
   async fn test_file_concurrent_reads() {
      let test_file = create_test_file(TEST_CONTENT);
      let reader = Arc::new(FileStreamReader::new(test_file.path()).unwrap());

      let mut handles = vec![];

      // Spawn multiple concurrent read tasks
      for _ in 0..10 {
         let reader_clone = Arc::clone(&reader);
         let handle = tokio::spawn(async move {
            let mut buffer = vec![0u8; TEST_CONTENT.len()];
            let bytes_read = reader_clone.read_at(0, &mut buffer).await.unwrap();
            (bytes_read, buffer)
         });
         handles.push(handle);
      }

      // Verify all reads succeeded with correct data
      for handle in handles {
         let (bytes_read, buffer) = handle.await.unwrap();
         assert_eq!(bytes_read, TEST_CONTENT.len());
         assert_eq!(&buffer[..bytes_read], TEST_CONTENT);
      }
   }
}

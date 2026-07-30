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
use reqwest::Client;
use reqwest::header::{CONTENT_LENGTH, HeaderMap, HeaderName, HeaderValue, RANGE};
use std::collections::HashMap;
use std::path::Path;
use std::sync::Arc;
use std::sync::OnceLock;
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

/// Copies bytes from `src` into `dst` and returns the count.
fn copy_into(dst: &mut [u8], src: &[u8]) -> usize {
   let len = dst.len().min(src.len());
   dst[..len].copy_from_slice(&src[..len]);
   len
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

   /// Returns the total size of the stream in bytes.
   async fn size(&self) -> Result<u64>;
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

      let file = Arc::clone(&self.file);
      let len = buf.len();

      let mut temp_buf = vec![0u8; len];
      let bytes_read = tokio::task::spawn_blocking(move || {
         let read = Self::sync_read_into(&file, offset, &mut temp_buf)?;
         Ok::<_, MediaParserError>((read, temp_buf))
      })
      .await
      .map_err(|e| MediaParserError::BlockingTask(format!("spawn_blocking failed: {}", e)))??;

      let (bytes_read, temp_buf) = bytes_read;
      copy_into(buf, &temp_buf[..bytes_read]);
      Ok(bytes_read)
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
/// Uses an initial HEAD request to cache `Content-Length`. Optional custom
/// headers can be provided for authentication or other metadata.
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
}

impl HttpStreamReader {
   /// Creates a new `HttpStreamReader` for the given URL.
   ///
   /// Performs a HEAD request to obtain the content length. Returns an error
   /// if the request fails or if `Content-Length` header is missing.
   pub async fn new(url: &str) -> Result<Self> {
      Self::build_with_headers(url, HeaderMap::new()).await
   }

   /// Creates a new `HttpStreamReader` with custom HTTP headers.
   ///
   /// Custom headers (e.g., for authentication) will be sent with all requests.
   pub async fn with_headers(url: &str, headers: HashMap<String, String>) -> Result<Self> {
      let mut header_map = HeaderMap::new();
      for (k, v) in headers {
         let header_name = HeaderName::try_from(k.as_str()).map_err(|e| {
            MediaParserError::HttpRequest(format!("Invalid header name '{}': {}", k, e))
         })?;
         let header_value = HeaderValue::from_str(v.as_str()).map_err(|e| {
            MediaParserError::HttpRequest(format!("Invalid header value for '{}': {}", k, e))
         })?;
         header_map.insert(header_name, header_value);
      }
      Self::build_with_headers(url, header_map).await
   }

   async fn build_with_headers(url: &str, headers: HeaderMap) -> Result<Self> {
      let client = Client::builder()
         .default_headers(headers.clone())
         .timeout(Duration::from_secs(30))
         .build()
         .map_err(|e| MediaParserError::HttpRequest(format!("Failed to build client: {}", e)))?;

      let resp = client
         .head(url)
         .send()
         .await
         .map_err(|e| MediaParserError::HttpRequest(format!("HEAD request failed: {}", e)))?;

      let len = resp
         .headers()
         .get(CONTENT_LENGTH)
         .and_then(|h| h.to_str().ok())
         .and_then(|s| s.parse::<u64>().ok())
         .ok_or(MediaParserError::ContentLengthMissing)?;

      let cached_size = OnceLock::new();
      let _ = cached_size.set(len);

      Ok(Self {
         url: url.to_string(),
         client,
         cached_size,
      })
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
      if !status.is_success() && status.as_u16() != HTTP_PARTIAL_CONTENT {
         return Err(MediaParserError::HttpStatus(status.as_u16()));
      }

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

      Ok(total_read)
   }
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

      let size = self.size().await?;
      if offset >= size {
         return Ok(0);
      }

      let mut total_read = 0usize;
      let mut current_offset = offset;

      // Loop to handle short reads: if server returns less than requested,
      // request the remaining bytes in subsequent requests
      while total_read < buf.len() && current_offset < size {
         let remaining = buf.len() - total_read;
         let available = usize::try_from(size - current_offset).unwrap_or(usize::MAX);
         let to_read = remaining.min(available);

         // Calculate range for this request directly
         let start = current_offset;
         let end = current_offset
            .checked_add(
               u64::try_from(to_read)
                  .map_err(|_| MediaParserError::Other("HTTP read length exceeds u64".into()))?,
            )
            .and_then(|end| end.checked_sub(1))
            .ok_or_else(|| MediaParserError::Other("HTTP range overflow".into()))?;

         // Read into the remaining portion of the buffer
         let bytes_read = self
            .fetch_range_stream(start, end, &mut buf[total_read..])
            .await?;

         if bytes_read == 0 {
            // EOF or no more data available
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

   /// Returns the total size of the HTTP stream.
   async fn size(&self) -> Result<u64> {
      self
         .cached_size
         .get()
         .copied()
         .ok_or(MediaParserError::ContentLengthUnavailable)
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

   fn create_test_file(content: &[u8]) -> NamedTempFile {
      let mut file = NamedTempFile::new().unwrap();
      file.write_all(content).unwrap();
      file.flush().unwrap();
      file
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
   async fn test_http_read_at_beginning() {
      let content_len_str = TEST_CONTENT.len().to_string();
      let mock_server = MockServer::start().await;

      // Mock HEAD request for Content-Length
      Mock::given(method("HEAD"))
         .respond_with(
            ResponseTemplate::new(200).insert_header("Content-Length", content_len_str.as_str()),
         )
         .mount(&mock_server)
         .await;

      // Mock GET request with Range header for reading from beginning
      let range_header = format!("bytes=0-{}", TEST_CONTENT.len() - 1);
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
   }

   #[tokio::test]
   async fn test_http_read_at_offset() {
      let html_body = format!(
         "<html><body><p>{}</p></body></html>",
         String::from_utf8_lossy(TEST_CONTENT)
      );
      let html_bytes = html_body.as_bytes();
      let content_len_str = html_bytes.len().to_string();
      let mock_server = MockServer::start().await;

      // Mock HEAD request for Content-Length
      Mock::given(method("HEAD"))
         .respond_with(
            ResponseTemplate::new(200).insert_header("Content-Length", content_len_str.as_str()),
         )
         .mount(&mock_server)
         .await;

      let expected_str = "you also must do to them";
      let expected = expected_str.as_bytes();
      let range_start = html_body
         .find(expected_str)
         .expect("phrase should be present in HTML") as u64;
      let range_end = range_start + expected.len() as u64 - 1;
      let range_header = format!("bytes={}-{}", range_start, range_end);
      let range_resp_header = format!("bytes {}-{}/{}", range_start, range_end, html_bytes.len());

      Mock::given(method("GET"))
         .and(header("Range", range_header.as_str()))
         .respond_with(
            ResponseTemplate::new(206)
               .set_body_bytes(&html_bytes[range_start as usize..=range_end as usize])
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
   async fn test_http_stream_reader_error_head_request_failed() {
      // Use an invalid URL to trigger HTTP request error
      let invalid_url = "http://localhost:1/invalid";
      let result = HttpStreamReader::new(invalid_url).await;

      assert!(result.is_err());

      if let Err(MediaParserError::HttpRequest(_)) = result {
         // Expected: HttpRequest error for failed HEAD request
      } else {
         panic!("Expected HttpRequest error for failed HEAD request");
      }
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

# media-parser

## Overview

The `media-parser` crate provides an API for getting metadata, tracks, subtitles
and frames from a local or remote MP4 media file.

`HttpStreamReader::with_headers` accepts at most 64 header entries and returns an
error above that limit. With a nonempty header map, redirects may only stay within
the same origin (scheme, host and port), for up to ten hops. This also applies to
public headers such as `User-Agent`. Without configured headers, redirects retain
the default policy of up to ten hops across origins.

## Examples

### 1) Metadata

```rust
use media_parser::{MediaParser, FileStreamReader};

#[tokio::main]
async fn main() -> media_parser::Result<()> {
    let reader = FileStreamReader::new("video.mp4");
    let parser = MediaParser::new(reader);

    let metadata = parser.metadata().await?;

    println!("Title: {:?}", metadata.get("title"));
    println!("Artist: {:?}", metadata.get("artist"));
    println!("Album: {:?}", metadata.get("album"));
    // Duration is represented as raw ticks with a timescale.
    let seconds = metadata.duration as f64 / metadata.timescale as f64;
    println!("Duration: {:.3}s (timescale: {}, ticks: {})", seconds, metadata.timescale, metadata.duration);

    Ok(())
}
```

### 2) Tracks

```rust
use media_parser::{MediaParser, FileStreamReader, TrackType};

#[tokio::main]
async fn main() -> media_parser::Result<()> {
    let mut parser = MediaParser::new(FileStreamReader::new("video.mp4"));
    let tracks = parser.tracks().await?; // Vec<TrackType>
    for t in tracks {
        match t {
            TrackType::Video(v) => println!("Video #{} {}x{} ({})", v.base.id, v.width, v.height, v.base.codec),
            TrackType::Audio(a) => println!("Audio #{} {}ch @{}Hz ({})", a.base.id, a.channels, a.sample_rate, a.base.codec),
            TrackType::Subtitle(s) => println!("Subtitle #{} {:?}", s.base.id, s.base.language),
            TrackType::Unknown(u) => println!("Unknown #{} {}", u.base.id, u.base.codec),
        }
    }
    Ok(())
}
```

### 3) Subtitles

```rust
use std::time::Duration;
use media_parser::{FileStreamReader, MediaParser, TrackFilter};

#[tokio::main]
async fn main() -> media_parser::Result<()> {
   let parser = MediaParser::new(FileStreamReader::new("video.mp4")?);
   let tracks = parser
      .subtitles_in_range(
         Some(TrackFilter::Language("ENG".into())),
         (Duration::from_secs(5), Duration::from_secs(15)),
      )
      .await?;

   for track in tracks {
      for cue in track.cues {
         println!(
            "#{} [{:?} - {:?}] {}",
            cue.cue_id, cue.start_time, cue.end_time, cue.text,
         );
      }
   }
   Ok(())
}
```

`MediaParser::subtitles` returns all cues; `subtitles_in_range` returns cues
that overlap the half-open range `[start, end)`. Cue times remain absolute to
the source rather than being clipped or rebased, and each `cue_id` is the
stable, one-based MP4 sample index. `SubtitleTrack::base.duration` is the raw
media duration in `base.timescale` ticks; cue times are `Duration` values.

Only a single, non-empty, normal-rate MP4 edit-list segment is modeled as a
scalar presentation offset before range selection. Empty, multi-segment,
malformed, or non-1× edit lists degrade to a zero offset.

The MP4 implementation supports `tx3g`, `wvtt`, `stpp`, and QuickTime `text`
sample entries. It does not decode CEA-608/708 data embedded in video samples.
Formats without a subtitle implementation, including MP3, return an empty
vector.

For `stpp`, `cue.text` contains the decoded TTML markup without XML
interpretation or separation of `<p>` elements. Each decoded sample that
remains non-empty after trimming whitespace and NUL characters produces one
cue with its original interval. Parsing and rendering that markup is the caller's
responsibility.

With no `TrackFilter`, all valid supported tracks are returned only when their
combined work fits the aggregate request budgets. `TrackFilter::TrackId` is the
narrowest selector; `TrackFilter::Language` may select a group of tracks and
matches ASCII case-insensitively. Use `subtitles_in_range` or the range argument
to `SubtitleIndex::subtitles` when even one selected track is individually too
dense. A filter with no match returns an empty vector.

`TrackFilter::TrackId(0)` is a literal ID in the Rust API; only the
Tauri/TypeScript layer treats zero as "first valid supported track". An
explicitly selected recoverably malformed or unsupported track returns an
error, while unfiltered or language-filtered extraction skips it.
Container-wide, I/O, and aggregate-budget failures reject the complete request
explicitly: extraction never returns a partial track prefix or silently chooses
fewer tracks.

#### Reusing an MP4 subtitle index

Repeated range requests should build one `SubtitleIndex` and retain it:

```rust
use std::{sync::Arc, time::Duration};
use media_parser::{FileStreamReader, TrackFilter, format::mp4::SubtitleIndex};

#[tokio::main]
async fn main() -> media_parser::Result<()> {
   let reader = FileStreamReader::new("video.mp4")?;
   let index = Arc::new(SubtitleIndex::read(&reader).await?);

   for start in [0, 30] {
      let tracks = Arc::clone(&index)
         .subtitles(
            &reader,
            Some(TrackFilter::Language("eng".into())),
            Some((
               Duration::from_secs(start),
               Duration::from_secs(start + 30),
            )),
         )
         .await?;
      println!("{} track(s) overlap this segment", tracks.len());
   }
   Ok(())
}
```

The index retains compact sample tables, not subtitle payloads or decoded text.
It may be reused with any reader over exactly the same immutable source bytes;
using it after the source changes is a caller error. The convenience functions
`format::mp4::read_subtitles` and `read_subtitles_in_range` build a temporary
index for each call.

Clients making repeated range requests should retain one `SubtitleIndex`
across those requests. Because returned cue times stay absolute to the media
source, callers must clamp and rebase them when their output uses a different
timeline.

Index construction scans at most 1,000 MP4 tracks, accounts at most 200,000
subtitle samples, and retains at most 32 MiB of index data. A request selects at
most 200,000 samples/cues, reads at most 1 MiB per sample, 64 MiB logically and
96 MiB physically, and decodes at most 32 MiB of text. Coalesced I/O is limited
to 16,384 regions of at most 8 MiB, with at most a 64 KiB gap joined into a
region. These aggregate limits are shared across every selected track and
overflow or allocation failures return errors instead of permitting unbounded
growth. A budget failure never returns the tracks that happened to finish before
the limit was reached.

### 4) Frames

#### Single Frame

```rust
use media_parser::{MediaParser, FileStreamReader, PixelFormat};
use std::time::Duration;

#[tokio::main]
async fn main() -> media_parser::Result<()> {
   let reader = FileStreamReader::new("video.mp4");
   let parser = MediaParser::new(reader);

   let frame = parser.frame(0, Duration::from_secs(30)).await?;

   println!("Captured frame: {}x{} in {:?} format", frame.width, frame.height, frame.format);
   println!("Frame data: {} bytes", frame.data.len());

   match frame.format {
      PixelFormat::Yuv420p => println!("YUV 4:2:0 format detected"),
      PixelFormat::Rgb24 => println!("RGB format detected"),
      _ => println!("Other format: {:?}", frame.format),
   }

   Ok(())
}
```

#### Multiple Frames

```rust
use media_parser::{MediaParser, FileStreamReader, PixelFormat};
use std::time::Duration;

#[tokio::main]
async fn main() -> media_parser::Result<()> {
   let reader = FileStreamReader::new("video.mp4");
   let parser = MediaParser::new(reader);

   let timestamps = vec![
      Duration::from_secs(10),
      Duration::from_secs(30),
      Duration::from_secs(60),
      Duration::from_secs(120),
   ];

   let frames = parser.frames(0, &timestamps).await?;

   for (i, frame) in frames.iter().enumerate() {
      println!("Frame {}: {}x{} ({:?})", i, frame.width, frame.height, frame.format);
      println!("Data: {} bytes", frame.data.len());

      match frame.format {
         PixelFormat::Yuv420p => {
            let y_size = (frame.width * frame.height) as usize;
            let uv_size = y_size / 4;
            println!("Y: {} bytes, U: {} bytes, V: {} bytes", y_size, uv_size, uv_size);
         },
         PixelFormat::Rgb24 => {
            println!("RGB pixels: {}", frame.data.len() / 3);
         },
         _ => {},
      }
   }

   println!("Generated {} frames total", frames.len());

   Ok(())
}
```

## Development

### Linting

   * `npm run standards` - Runs all linting, including `clippy`, `rustfmt` (check only),
     `commitlint`, `markdownlint`, etc.
   * `npm run rust:lint` - Runs linting on Rust code only
   * `npm run rust:lint:fix` - Formats Rust code

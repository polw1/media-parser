# Tauri Media Parser Plugin

[![CI][ci-badge]][ci-url]

A Tauri plugin to parse media files (MP3, MP4): extract metadata,
tracks, frames, and subtitles. Async API for getting info from local
files or HTTP streams.

[ci-badge]: https://github.com/silvermine/tauri-plugin-media-parser/actions/workflows/ci.yml/badge.svg
[ci-url]: https://github.com/silvermine/tauri-plugin-media-parser/actions/workflows/ci.yml

## Project Structure

This project is organized as a Cargo workspace with the following structure:

```text
tauri-plugin-media-parser/
├── crates/
│   └── media-parser/          # Rust media parser library
│       ├── src/
│       │   ├── format/
│       │   │   ├── mp3/       # MP3 parsing (frames, duration, ID3 tags)
│       │   │   ├── mp4/       # MP4 parsing (atoms, moov, metadata, tracks)
│       │   │   │   └── atoms/ # Box/atom reading, iteration, navigation, media atom parsing
│       │   │   ├── registry.rs # Format detection and parser dispatch
│       │   │   └── signatures.rs # Markers and extension mappings
│       │   ├── helpers/       # Byte reading, text decoding utilities
│       │   ├── errors.rs
│       │   ├── lib.rs
│       │   ├── stream.rs
│       │   └── types.rs
│       └── Cargo.toml
├── src/                        # Tauri plugin implementation
│   ├── commands.rs             # Plugin commands
│   ├── error.rs                 # Error types
│   └── lib.rs                   # Main plugin code
├── guest-js/                    # JavaScript/TypeScript bindings
│   ├── index.ts
│   └── tsconfig.json
├── permissions/                 # Permission definitions (mostly generated)
├── dist-js/                     # Compiled JS (generated)
├── Cargo.toml                   # Workspace configuration
├── package.json                 # NPM package configuration
└── build.rs                     # Build script
```

## Crates

### media-parser

A Rust module with no dependencies on Tauri or its plugin architecture. It
provides an async API for parsing MP4 media files, extracting metadata, tracks,
subtitles, and frames from local files or HTTP streams. It's designed to be
published as a standalone crate in the future with minimal changes.

See [`crates/media-parser/README.md`](crates/media-parser/README.md)
for more details.

### Tauri Plugin

The main plugin provides a Tauri integration layer that exposes media parsing
functionality to Tauri applications. It uses the `media-parser` module internally.

## Getting Started

### Installation

1. Install NPM dependencies:

   ```bash
   npm install
   ```

2. Build the TypeScript bindings:

   ```bash
   npm run build
   ```

3. Build the Rust plugin:

   ```bash
   cargo build
   ```

### Tests

Run Rust tests:

```bash
cargo test
```

### Linting and standards checks

```bash
npm run standards
```

## Usage

### In a Tauri Application

Add the plugin to your Tauri application's `Cargo.toml`:

```toml
[dependencies]
tauri-plugin-media-parser = { path = "../path/to/tauri-plugin-media-parser" }
```

Add the plugin permission to your capabilities file
`src-tauri/capabilities/default.json`

```json
{
  "permissions": [
    "core:default",
    "media-parser:default"
  ]
}
```

Initialize the plugin in your Tauri app:

```rust
fn main() {
    tauri::Builder::default()
        .plugin(tauri_plugin_media_parser::init())
        .run(tauri::generate_context!())
        .expect("error while running tauri application");
}
```

### JavaScript/TypeScript API

Install the JavaScript package in your frontend:

```bash
npm install @silvermine/tauri-plugin-media-parser
```

Use the plugin from JavaScript/TypeScript:

```typescript
import {
   getMetadata,
   getTracks,
   getDurationInSeconds,
   getMetadataValue,
} from '@silvermine/tauri-plugin-media-parser';

// Extract metadata from a local file
const metadata = await getMetadata('/path/to/video.mp4');

// Or from a remote URL with optional headers
const remoteMetadata = await getMetadata('https://example.com/video.mp4', {
   headers: { 'Authorization': 'Bearer token123' },
});

// Get duration in seconds
const duration = getDurationInSeconds(metadata);
console.log(`Duration: ${duration}s`);

// Get specific metadata values
const title = getMetadataValue(metadata, 'Title');
const artist = getMetadataValue(metadata, 'Artist');
console.log(`Title: ${title}, Artist: ${artist}`);

// Extract track details
const tracks = await getTracks('/path/to/video.mp4');
for (const track of tracks) {
   console.log(`${track.kind} track ${track.id}: ${track.codec}`);

   if (track.kind === 'video') {
      console.log(`Resolution: ${track.width}x${track.height}`);
   }

   if (track.kind === 'audio') {
      console.log(`Audio: ${track.channels} channels at ${track.sampleRate}Hz`);
   }
}
```

### Cover art

`getCover` extracts embedded cover artwork. Unlike thumbnails, it works for
MP3 as well as MP4/M4A/MOV, and it returns `null` when the file carries no
cover, so the result must be checked before use.

```typescript
import { getCover } from '@silvermine/tauri-plugin-media-parser';

const cover = await getCover('/path/to/song.mp3');

if (cover) {
   // `format` is 'jpeg' or 'png', whichever the file embeds.
   console.log(cover.format, cover.mimeType, cover.data.length);
} else {
   console.log('This file has no embedded cover art.');
}
```

The `data` field is a view into the binary IPC response rather than a
standalone copy. Copy it with `new Uint8Array(cover.data)` when it must
outlive the rest of the response.

### Video thumbnails

`getThumbnails` extracts JPEG previews from H.264/AVC video tracks in
MP4/M4V/MOV containers. Other video codecs and audio-only formats such as MP3
do not have a thumbnail path.

```typescript
import { getThumbnails } from '@silvermine/tauri-plugin-media-parser';

const thumbnails = await getThumbnails('/path/to/video.mp4', {
   // Input timestamps are milliseconds.
   timestamps: [0, 5_000, 10_000],
   maxWidth: 640,
   maxHeight: 360,
   quality: 60,
});

for (const thumbnail of thumbnails) {
   // Output timestamps are the returned frames' presentation times in seconds.
   console.log(thumbnail.timestampSec, thumbnail.width, thumbnail.height);
}
```

Fast mode is the default (`accurate: false`). It returns the preceding
keyframe, so `timestampSec` can be earlier than the requested timestamp. Set
`accurate: true` to decode the exact requested frame. Timestamps must be
non-negative safe integers, and one request may contain at most 4,096 entries.
JPEGs preserve the source aspect ratio, never upscale, and fit within a 320×320
box by default. Set `maxWidth` and/or `maxHeight` to choose another bound; when
only one is supplied, the other dimension is unconstrained. Downscaling occurs
directly from decoded YUV, before allocating the RGB buffer used by the JPEG
encoder.

Thumbnail output is capped at 256 MiB in total, including the envelope header
and JPEG payloads. Identical requested timestamps are decoded once and share
one JPEG payload. Distinct timestamps count separately even when they resolve
to the same frame. Large dimensions can reach the cap well before the
4,096-entry limit, and exceeding it rejects the whole request at runtime.

All `data` fields returned by one call are subarray views into a shared binary
IPC buffer. Retaining one thumbnail retains the complete response. Copy a view
with `new Uint8Array(thumbnail.data)` when it must outlive the rest of the
batch.

The plugin caches up to eight parsed thumbnail sessions. Remote sessions expire
after five minutes and local sessions after one minute; concurrent requests for
the same cold source share one index build.

H.264 decoding and JPEG encoding are prohibitively slow when their dependencies
use Cargo's unoptimized development profile. Add this to the Tauri
application's `src-tauri/Cargo.toml` for usable development performance:

```toml
[profile.dev.package."*"]
opt-level = 2
```

### Subtitles

`getSubtitles` extracts `tx3g`, `wvtt`, `stpp`, and QuickTime `text` subtitle
tracks from MP4/M4V/MOV sources. It does not decode CEA-608/708 captions carried
inside video samples. This Tauri command is MP4-family-only; a non-MP4 source,
including MP3, returns a parsing error. The lower-level Rust `MediaParser`
registry instead returns an empty subtitle vector for MP3.

```typescript
import { getSubtitles } from '@silvermine/tauri-plugin-media-parser';

const tracks = await getSubtitles('/path/to/video.mp4', {
   language: 'ENG',
   // The range is half-open: cues overlap [5,000 ms, 15,000 ms).
   startMs: 5_000,
   endMs: 15_000,
});

for (const track of tracks) {
   for (const cue of track.cues) {
      // Times remain absolute to the source and are expressed in seconds.
      console.log(cue.cueId, cue.startSec, cue.endSec, cue.text);
   }
}
```

A cue is selected when it overlaps the requested half-open range; it is not
clipped or rebased. `cueId` is the stable, one-based MP4 sample index, so it
does not change between full and ranged requests. `SubtitleInfo.duration` is
the raw media duration in `timescale` ticks; `startSec` and `endSec` are seconds.

Only a single, non-empty, normal-rate MP4 edit-list segment is modeled as a
scalar presentation offset before selection. Empty, multi-segment, malformed,
or non-1× edit lists degrade to a zero offset.

Subtitle filters behave as follows:

   * With neither `trackId` nor `language`, every valid supported track is
     returned.
   * Language matching is ASCII case-insensitive. An empty language is a valid
     filter and normally returns no matches.
   * `trackId` takes precedence when both selectors are present. `trackId: 0`
     selects only the first valid supported track; a positive value selects
     that exact track.
   * A selector with no match returns an empty array.

Unfiltered and language-filtered requests skip recoverably malformed or
unsupported tracks. Selecting one of those tracks explicitly by a positive
`trackId` returns an error instead of a partial result.

`startMs` and `endMs` must either both be absent or both be non-negative safe
integers with `startMs < endMs`. Subtitle work is bounded per request: at most
200,000 selected samples and cues, 1 MiB per sample, 64 MiB of logical sample
data, 96 MiB of physical reads, 32 MiB of decoded UTF-8 text, and a 64 MiB
binary response envelope. Reads are further capped at 4,096 coalesced regions
of at most 8 MiB each, with at most a 64 KiB gap joined into a region.

The plugin caches at most eight source-wide subtitle sessions separately from
the thumbnail cache. All filters and ranges for a source reuse the same parsed
index; concurrent cold requests share its construction. Local sessions expire
after one minute and remote sessions after five minutes. The source bytes must
remain unchanged while a session is reused. Local path keys include file size
and modification time; remote content served by the same URL and headers may
remain cached until its TTL expires.

Clients making repeated range requests should retain one core `SubtitleIndex`
across those requests. Its cues keep absolute source times, so callers remain
responsible for clamping and rebasing them to their output timeline.

## Development Standards

This project follows the
[Silvermine standardization](https://github.com/silvermine/standardization)
guidelines. Key standards include:

   * **EditorConfig**: Consistent editor settings across the team
   * **Markdownlint**: Markdown linting for documentation
   * **Commitlint**: Conventional commit message format
   * **Code Style**: 3-space indentation, LF line endings

### Running Standards Checks

```bash
npm run standards
```

## License

MIT

### Third-party notices

This plugin links code whose license requires notices beyond the usual MIT and
Apache-2.0 boilerplate: the JPEG encoder carries an Independent JPEG Group
obligation. [THIRD_PARTY_NOTICES.md](THIRD_PARTY_NOTICES.md) records it, with
the exact text the license asks for.

That obligation transfers. The license requires the notice to reach the user
with the distribution, so an application that ships a compiled binary
containing this plugin must carry it in its own documentation, licenses
screen, or bundled resources. This repository supplies the text; including it
is the application's step.

## Contributing

Contributions are welcome! Please follow the established coding standards and commit
message conventions.

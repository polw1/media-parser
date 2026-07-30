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

## Contributing

Contributions are welcome! Please follow the established coding standards and commit
message conventions.

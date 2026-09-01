import { invoke } from '@tauri-apps/api/core';

import { decodeEnvelope } from './envelope';
import { decodeSubtitleEnvelope } from './subtitle-envelope';
import { validateSubtitleOptions } from './subtitle-options';
import { validateThumbnailDimensions, validateTimestamps } from './thumbnail-options';
import type {
   CoverInfo,
   Metadata,
   MetadataOptions,
   SubtitleInfo,
   SubtitleOptions,
   ThumbnailInfo,
   ThumbnailsOptions,
   TrackInfo,
} from './types';

export * from './types';

// ============================================================================
// Functions
// ============================================================================

/**
 * Extract metadata from a media file (local path or URL).
 *
 * Automatically detects if the source is a URL (http:// or https://) or a local file path.
 *
 * @param source - Absolute path to a local file or URL of a remote media file
 * @param options - Optional settings (headers are only used for URLs)
 * @returns Metadata containing duration, timescale, and tags (title, artist, etc.)
 *
 * @example
 * ```typescript
 * // Local file
 * const metadata = await getMetadata('/path/to/video.mp4');
 *
 * // Remote URL
 * const metadata = await getMetadata('https://example.com/video.mp4');
 *
 * // Remote URL with authentication
 * const metadata = await getMetadata('https://example.com/video.mp4', {
 *    headers: { 'Authorization': 'Bearer token123' }
 * });
 *
 * console.log(`Duration: ${metadata.duration / metadata.timescale} seconds`);
 *
 * // Find title
 * const title = metadata.values.find(m => m.name === 'Title');
 * if (title) {
 *    console.log(`Title: ${title.value}`);
 * }
 * ```
 */
export async function getMetadata(
   source: string,
   options?: MetadataOptions,
): Promise<Metadata> {
   return await invoke<Metadata>('plugin:media-parser|get_metadata', {
      source,
      headers: options?.headers,
   });
}

/**
 * Extract tracks from a media file (local path or URL).
 *
 * @param source - Absolute path to a local file or URL of a remote media file
 * @param options - Optional settings (headers are only used for URLs)
 * @returns Track information for video, audio, subtitle, and unknown tracks
 */
export async function getTracks(
   source: string,
   options?: MetadataOptions,
): Promise<TrackInfo[]> {
   return await invoke<TrackInfo[]>('plugin:media-parser|get_tracks', {
      source,
      headers: options?.headers,
   });
}

/**
 * Extract embedded cover artwork from a media file.
 *
 * @param source - Absolute path to a local file or URL of a remote media file
 * @param options - Optional settings (headers are only used for URLs)
 * @returns Cover artwork when present, otherwise null. The `data` bytes are
 * backed by the binary IPC response.
 */
export async function getCover(
   source: string,
   options?: MetadataOptions,
): Promise<CoverInfo | null> {
   const raw = await invoke<ArrayBuffer>('plugin:media-parser|get_cover', {
      source,
      headers: options?.headers,
   });

   const entries = decodeEnvelope<Omit<CoverInfo, 'data'>>(raw);
   return entries.length === 0 ? null : entries[0];
}

/**
 * Extract thumbnails for specific millisecond timestamps.
 *
 * Only MP4/M4V/MOV video tracks encoded as H.264/AVC are supported. MP3 files
 * and video tracks using another codec are rejected by the backend.
 *
 * Fast keyframe extraction is used by default. It returns the preceding
 * keyframe and reports that frame's actual presentation time in
 * `timestampSec`, which may be earlier than the requested millisecond value.
 * Set `accurate` to decode the exact requested frames.
 *
 * Every returned `data` value is a subarray of the same binary IPC response.
 * Retaining one thumbnail therefore retains the complete response buffer; use
 * `new Uint8Array(thumbnail.data)` when a small image must be retained alone.
 * Parsed thumbnail sessions are cached with an eight-entry LRU: remote
 * sessions expire after five minutes and local sessions after one minute.
 *
 * @param source - Absolute path to a local file or URL of a remote media file
 * @param options - Timestamps, optional track, accuracy, JPEG bounds/quality, and URL headers
 * @returns Thumbnails in request order, with actual frame times in seconds
 * @throws TypeError if `timestamps` has more than 4,096 entries, is not an
 *    array of non-negative safe integers, if `quality` is outside 1-100, or
 *    if a thumbnail dimension is outside 1-65535
 */
export async function getThumbnails(
   source: string,
   options: ThumbnailsOptions,
): Promise<ThumbnailInfo[]> {
   validateTimestamps(options.timestamps);
   validateQuality(options.quality);
   validateThumbnailDimensions(options.maxWidth, options.maxHeight);

   const raw = await invoke<ArrayBuffer>('plugin:media-parser|get_thumbnails', {
      source,
      timestamps: options.timestamps,
      trackId: options.trackId,
      accurate: options.accurate,
      quality: options.quality,
      maxWidth: options.maxWidth,
      maxHeight: options.maxHeight,
      headers: options.headers,
   });

   return decodeThumbnailEnvelope(raw);
}

/**
 * Extract subtitle tracks, optionally filtered and restricted to a half-open
 * millisecond range.
 *
 * When `trackId` is present, `language` is ignored. `trackId: 0` selects the
 * first valid supported subtitle track; without either filter, all valid
 * supported subtitle tracks are returned, but only when their combined work
 * fits the aggregate request budgets. Unfiltered and language-filtered
 * requests skip malformed or unsupported tracks; a selector that matches no
 * track returns an empty array.
 *
 * @param source - Absolute path to a local file or URL of a remote media file
 * @param options - Optional track/language filter, paired range, and URL headers
 * @returns Subtitle tracks with absolute source cue times in seconds
 * @throws TypeError if the track ID or paired range cannot be represented
 * @throws Error if an explicitly selected positive trackId is malformed or
 * unsupported, or if aggregate request budgets are exceeded
 */
export async function getSubtitles(
   source: string,
   options: SubtitleOptions = {},
): Promise<SubtitleInfo[]> {
   validateSubtitleOptions(options);
   const raw = await invoke<ArrayBuffer>('plugin:media-parser|get_subtitles', {
      source,
      trackId: options.trackId,
      language: options.language,
      startMs: options.startMs,
      endMs: options.endMs,
      headers: options.headers,
   });

   return decodeSubtitleEnvelope(raw);
}

/**
 * Rejects a quality the JPEG encoder cannot use. The Rust side validates this
 * too, since it is reachable from other callers; checking here turns it into a
 * local `TypeError` instead of a round trip.
 */
function validateQuality(quality: number | undefined): void {
   if (quality === undefined) {
      return;
   }

   if (!Number.isInteger(quality) || quality < 1 || quality > 100) {
      throw new TypeError(
         `Invalid thumbnail quality: ${String(quality)}. ` +
            'Quality must be an integer between 1 and 100.',
      );
   }
}

function decodeThumbnailEnvelope(raw: ArrayBuffer | Uint8Array): ThumbnailInfo[] {
   return decodeEnvelope<Omit<ThumbnailInfo, 'data'>>(raw);
}

// ============================================================================
// Utility Functions
// ============================================================================

/**
 * Calculate the duration in seconds from metadata.
 *
 * @param metadata - The metadata object
 * @returns Duration in seconds
 *
 * @example
 * ```typescript
 * const metadata = await getMetadata('/path/to/video.mp4');
 * const seconds = getDurationInSeconds(metadata);
 * console.log(`Video is ${seconds} seconds long`);
 * ```
 */
export function getDurationInSeconds(metadata: Metadata): number {
   if (metadata.timescale === 0) {
      return 0;
   }
   return metadata.duration / metadata.timescale;
}

/**
 * Get a metadata value by friendly name (case-insensitive).
 *
 * @param metadata - The metadata object
 * @param name - The friendly name to search for (e.g., "Title", "Artist", "Album")
 * @returns The value if found, undefined otherwise
 *
 * @example
 * ```typescript
 * const metadata = await getMetadata('/path/to/video.mp4');
 * const title = getMetadataValue(metadata, 'title');
 * const artist = getMetadataValue(metadata, 'artist');
 * ```
 */
export function getMetadataValue(metadata: Metadata, name: string): string | undefined {
   const lowerName = name.toLowerCase();
   const meta = metadata.values.find((m) => m.name.toLowerCase() === lowerName);
   return meta?.value;
}

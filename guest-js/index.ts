import { invoke } from '@tauri-apps/api/core';

import { decodeEnvelope } from './envelope';
import type {
   CoverInfo,
   Metadata,
   MetadataOptions,
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
 * Fast keyframe extraction is used by default. Set `accurate` to decode the
 * exact requested frames.
 *
 * @param source - Absolute path to a local file or URL of a remote media file
 * @param options - Timestamps, optional track, accuracy, quality, and URL headers
 * @returns Thumbnails in the same order as the requested timestamps
 * @throws TypeError if `timestamps` is not an array of non-negative safe
 *    integers, or if `quality` is outside 1-100
 */
export async function getThumbnails(
   source: string,
   options: ThumbnailsOptions,
): Promise<ThumbnailInfo[]> {
   validateTimestamps(options.timestamps);
   validateQuality(options.quality);

   const raw = await invoke<ArrayBuffer>('plugin:media-parser|get_thumbnails', {
      source,
      timestamps: options.timestamps,
      trackId: options.trackId,
      accurate: options.accurate,
      quality: options.quality,
      headers: options.headers,
   });

   return decodeThumbnailEnvelope(raw);
}

/**
 * Rejects timestamps the backend cannot represent. They are deserialized into
 * a Rust `Vec<u64>`, so a negative, fractional, or non-finite value fails deep
 * inside the IPC layer with an opaque message; a value above
 * `Number.MAX_SAFE_INTEGER` would silently lose precision on the way there.
 */
function validateTimestamps(timestamps: number[]): void {
   if (!Array.isArray(timestamps)) {
      throw new TypeError('Thumbnail timestamps must be an array of numbers.');
   }

   for (const timestamp of timestamps) {
      if (!Number.isSafeInteger(timestamp) || timestamp < 0) {
         throw new TypeError(
            `Invalid thumbnail timestamp: ${String(timestamp)}. ` +
               'Timestamps must be non-negative integers, in milliseconds.',
         );
      }
   }
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

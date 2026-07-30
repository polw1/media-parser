import { invoke } from '@tauri-apps/api/core';

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
 * @returns Cover artwork when present, otherwise null
 */
export async function getCover(
   source: string,
   options?: MetadataOptions,
): Promise<CoverInfo | null> {
   return await invoke<CoverInfo | null>('plugin:media-parser|get_cover', {
      source,
      headers: options?.headers,
   });
}

/**
 * Extract thumbnails for specific millisecond timestamps.
 *
 * Fast keyframe extraction is used by default. Set `accurate` to decode the
 * exact requested frames.
 *
 * @param source - Absolute path to a local file or URL of a remote media file
 * @param options - Timestamps, optional track, accuracy, and URL headers
 * @returns Thumbnails in the same order as the requested timestamps
 */
export async function getThumbnails(
   source: string,
   options: ThumbnailsOptions,
): Promise<ThumbnailInfo[]> {
   const raw = await invoke<ArrayBuffer>('plugin:media-parser|get_thumbnails', {
      source,
      timestamps: options.timestamps,
      trackId: options.trackId,
      accurate: options.accurate,
      headers: options.headers,
   });

   return decodeThumbnailEnvelope(raw);
}

function decodeThumbnailEnvelope(raw: ArrayBuffer | Uint8Array): ThumbnailInfo[] {
   const buffer = raw instanceof Uint8Array ? raw : new Uint8Array(raw);
   const view = new DataView(buffer.buffer, buffer.byteOffset, buffer.byteLength);
   const headerLength = view.getUint32(0, true);
   const headerEnd = 4 + headerLength;
   const header = buffer.subarray(4, headerEnd);

   interface EnvelopeEntry extends Omit<ThumbnailInfo, 'data'> {
      offset: number;
      length: number;
   }

   const entries: EnvelopeEntry[] = JSON.parse(new TextDecoder().decode(header));
   return entries.map(({ offset, length, ...info }) => ({
      ...info,
      data: buffer.subarray(headerEnd + offset, headerEnd + offset + length),
   }));
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

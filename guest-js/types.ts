// ============================================================================
// Metadata Types
// ============================================================================

/**
 * Single extracted metadata item.
 */
export interface Meta {
   /** Raw metadata key (e.g., "@nam" for MP4, "TIT2" for MP3). */
   key: string;
   /** Friendly mapped name (e.g., "Title", "Artist", or "Unknown"). */
   name: string;
   /** Extracted value (UTF-8, trimmed of null padding). */
   value: string;
}

/**
 * Metadata extracted from a media file.
 */
export interface Metadata {
   /** Detected format name (e.g., "MP4/M4A/MOV", "MP3"). */
   format: string;
   /** All metadata items found. */
   values: Meta[];
   /** Time units per second for `duration`. */
   timescale: number;
   /** Total raw duration in `timescale` units. */
   duration: number;
}

/**
 * Options for metadata extraction.
 */
export interface MetadataOptions {
   /** Custom HTTP headers to send with the request (only used for URLs). */
   headers?: Record<string, string>;
}

/**
 * Embedded cover artwork.
 */
export interface CoverInfo {
   format: 'jpeg' | 'png';
   mimeType: 'image/jpeg' | 'image/png';
   /**
    * Image bytes backed by the shared binary IPC response. Retaining this view
    * retains the whole response; copy it with `new Uint8Array(data)` to detach.
    */
   data: Uint8Array;
}

/**
 * Thumbnail extracted from a video track.
 */
export interface ThumbnailInfo {
   trackId: number;
   width: number;
   height: number;
   /** Presentation time of the returned frame, in seconds. */
   timestampSec: number;
   /** Always JPEG: decoded frames are encoded as JPEG by the Rust side. */
   format: 'jpeg';
   mimeType: 'image/jpeg';
   /**
    * Image bytes backed by the batch's shared binary IPC response. Retaining
    * this view retains the whole batch; copy it to detach the thumbnail.
    */
   data: Uint8Array;
}

/**
 * Options for extracting thumbnails.
 */
export interface ThumbnailsOptions extends MetadataOptions {
   /**
    * Timestamps to extract, in milliseconds. Each must be a non-negative
    * safe integer, and at most 4,096 entries may be requested at once;
    * {@link getThumbnails} rejects anything else before the call reaches the
    * backend.
    */
   timestamps: number[];
   /** Track id to extract from. Defaults to the first video track. */
   trackId?: number;
   /**
    * Decode exact requested frames. Defaults to `false`, which returns the
    * preceding keyframe and its actual presentation time.
    */
   accurate?: boolean;
   /**
    * JPEG quality of the returned frames, from 1 to 100. Defaults to 60.
    *
    * This trades size, not time: a 1080p frame costs about the same to encode
    * at 40 as at 85, while the output grows roughly 4×. Note that the encoder
    * drops to 4:2:0 chroma subsampling below 90, so 89 → 90 is a step rather
    * than a smooth increase.
    */
   quality?: number;
   /**
    * Maximum JPEG width. Together with `maxHeight`, forms an
    * aspect-ratio-preserving bounding box. The default box is 320×320;
    * specifying only one dimension leaves the other unconstrained.
    */
   maxWidth?: number;
   /** Maximum JPEG height; see `maxWidth`. */
   maxHeight?: number;
}

/** One timed subtitle cue decoded from the binary IPC response. */
export interface SubtitleCueInfo {
   cueId: number;
   startSec: number;
   endSec: number;
   /**
    * Cue text as decoded from the sample. For `stpp` this is the decoded TTML
    * markup; see `getSubtitles`.
    */
   text: string;
}

/** One subtitle track and the cues selected for the requested range. */
export interface SubtitleInfo {
   id: number;
   codec: string;
   language?: string;
   timescale: number;
   duration: number;
   cues: SubtitleCueInfo[];
}

/** Options for extracting subtitle tracks and cues. */
export interface SubtitleOptions extends MetadataOptions {
   /**
    * Track selector. `0` selects the first valid supported subtitle track.
    * When present, `language` is ignored.
    */
   trackId?: number;
   /** Case-insensitive language selector. An empty string is a valid no-match filter. */
   language?: string;
   /** Inclusive range start in milliseconds; must be paired with `endMs`. */
   startMs?: number;
   /** Exclusive range end in milliseconds; must be paired with `startMs`. */
   endMs?: number;
}

// ============================================================================
// Track Types
// ============================================================================

/**
 * The kind of a track. Used as the discriminant of the {@link TrackInfo} union.
 *
 * The string values match the `kind` field emitted by the Rust command, so the
 * compiler can narrow a {@link TrackInfo} to a specific variant based on `kind`.
 *
 * @example
 * ```ts
 * if (track.kind === TrackKind.Video) {
 *    track.width; // number — TypeScript knows width is present
 * }
 * ```
 */
export enum TrackKind {

   /** Video track (has `width`/`height`). */
   Video = 'video',

   /** Audio track (has `channels`/`sampleRate`). */
   Audio = 'audio',

   /** Subtitle/caption track. */
   Subtitle = 'subtitle',

   /** Track whose handler could not be classified. */
   Unknown = 'unknown',
}

/**
 * Fields common to every track kind.
 */
export interface BaseTrackInfo<K extends TrackKind> {
   kind: K;
   id: number;
   codec: string;
   language?: string;
   timescale: number;
   duration: number;
   /**
    * Format-specific diagnostic values emitted by the parser.
    *
    * Keys and string encodings are unstable and may change between releases.
    */
   properties: Record<string, string>;
}

/** A video track, with pixel dimensions. */
export interface VideoTrackInfo extends BaseTrackInfo<TrackKind.Video> {
   width: number;
   height: number;
}

/** An audio track, with channel and sample-rate information. */
export interface AudioTrackInfo extends BaseTrackInfo<TrackKind.Audio> {
   channels: number;
   sampleRate: number;
}

/** A subtitle/caption track. */
export type SubtitleTrackInfo = BaseTrackInfo<TrackKind.Subtitle>;

/** A track whose handler could not be classified. */
export type UnknownTrackInfo = BaseTrackInfo<TrackKind.Unknown>;

/**
 * Maps each {@link TrackKind} to its variant interface. Building the union from
 * this map guarantees, at compile time, that every kind has a corresponding
 * variant.
 */
interface TrackInfoByKind {
   [TrackKind.Video]: VideoTrackInfo;
   [TrackKind.Audio]: AudioTrackInfo;
   [TrackKind.Subtitle]: SubtitleTrackInfo;
   [TrackKind.Unknown]: UnknownTrackInfo;
}

/**
 * A track of any kind.
 *
 * To narrow the type to a specific kind, use either one of the type guards
 * ({@link isVideoTrack}, {@link isAudioTrack}) or the `kind` field as a
 * discriminator.
 *
 * @example
 * ```ts
 * for (const track of await getTracks('/path/to/video.mp4')) {
 *    if (track.kind === TrackKind.Video) {
 *       console.log(`${track.width}x${track.height}`);
 *    } else if (track.kind === TrackKind.Audio) {
 *       console.log(`${track.channels}ch @ ${track.sampleRate}Hz`);
 *    }
 * }
 * ```
 */
export type TrackInfo = TrackInfoByKind[TrackKind];

/**
 * @returns `true` if the track is a video track (narrows to {@link VideoTrackInfo}).
 */
export function isVideoTrack(track: TrackInfo): track is VideoTrackInfo {
   return track.kind === TrackKind.Video;
}

/**
 * @returns `true` if the track is an audio track (narrows to {@link AudioTrackInfo}).
 */
export function isAudioTrack(track: TrackInfo): track is AudioTrackInfo {
   return track.kind === TrackKind.Audio;
}

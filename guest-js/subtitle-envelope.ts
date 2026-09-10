import { parseEnvelopeHeader } from './envelope';
import type { SubtitleCueInfo, SubtitleInfo } from './types';

const UTF8_DECODER = new TextDecoder('utf-8', { fatal: true });

/** Envelope header shapes the subtitle decoder understands. */
const SUPPORTED_SUBTITLE_ENVELOPE_VERSIONS = new Set([1]);

export function decodeSubtitleEnvelope(
   raw: ArrayBuffer | Uint8Array,
): SubtitleInfo[] {
   const parsed = parseEnvelopeHeader<unknown>(raw, {
      supportedVersions: SUPPORTED_SUBTITLE_ENVELOPE_VERSIONS,
      envelopeKind: 'subtitle',
   });

   return parsed.entries.map((entry) => decodeTrack(
      entry,
      parsed.buffer,
      parsed.payloadStart,
      parsed.payloadLength,
   ));
}

function decodeTrack(
   value: unknown,
   buffer: Uint8Array,
   payloadStart: number,
   payloadLength: number,
): SubtitleInfo {
   const entry = requireObject(value, 'Subtitle envelope track');
   const id = requireSafeInteger(entry.id, 'Subtitle track id');
   const timescale = requireSafeInteger(entry.timescale, 'Subtitle track timescale');
   const duration = requireSafeInteger(entry.duration, 'Subtitle track duration');

   if (typeof entry.codec !== 'string') {
      throw new TypeError('Subtitle track codec must be a string.');
   }
   if (entry.language !== undefined && typeof entry.language !== 'string') {
      throw new TypeError('Subtitle track language must be a string when present.');
   }
   if (!Array.isArray(entry.cues)) {
      throw new TypeError('Subtitle track cues must be an array.');
   }

   const cues = entry.cues.map((cue) => decodeCue(
      cue,
      buffer,
      payloadStart,
      payloadLength,
   ));
   const decoded: SubtitleInfo = {
      id,
      codec: entry.codec,
      timescale,
      duration,
      cues,
   };
   if (entry.language !== undefined) {
      decoded.language = entry.language;
   }
   return decoded;
}

function decodeCue(
   value: unknown,
   buffer: Uint8Array,
   payloadStart: number,
   payloadLength: number,
): SubtitleCueInfo {
   const entry = requireObject(value, 'Subtitle envelope cue');
   const cueId = requireSafeInteger(entry.cueId, 'Subtitle cue id');
   const offset = requireSafeInteger(entry.offset, 'Subtitle cue offset');
   const length = requireSafeInteger(entry.length, 'Subtitle cue length');
   const startSec = requireTime(entry.startSec);
   const endSec = requireTime(entry.endSec);

   if (endSec <= startSec) {
      throw new TypeError('Subtitle cues require finite non-negative times with endSec after startSec.');
   }
   if (offset > payloadLength || length > payloadLength - offset) {
      throw new TypeError('Subtitle cue payload range is outside the envelope.');
   }

   const bytes = buffer.subarray(
      payloadStart + offset,
      payloadStart + offset + length,
   );
   let text: string;
   try {
      text = UTF8_DECODER.decode(bytes);
   } catch {
      throw new TypeError('Subtitle cue text is not valid UTF-8.');
   }

   return { cueId, startSec, endSec, text };
}

function requireObject(value: unknown, field: string): Record<string, unknown> {
   if (typeof value !== 'object' || value === null || Array.isArray(value)) {
      throw new TypeError(`${field} must be an object.`);
   }
   return value as Record<string, unknown>;
}

function requireSafeInteger(value: unknown, field: string): number {
   if (typeof value !== 'number' || !Number.isSafeInteger(value) || value < 0) {
      throw new TypeError(`${field} must be a non-negative safe integer.`);
   }
   return value;
}

function requireTime(value: unknown): number {
   if (typeof value !== 'number' || !Number.isFinite(value) || value < 0) {
      throw new TypeError(
         'Subtitle cues require finite non-negative times with endSec after startSec.',
      );
   }
   return value;
}

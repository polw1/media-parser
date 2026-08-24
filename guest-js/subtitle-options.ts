import type { SubtitleOptions } from './types';

const MAX_U32 = 0xffff_ffff;

/** Rejects subtitle options the Rust command cannot represent. */
export function validateSubtitleOptions(options: SubtitleOptions): void {
   if (
      options.trackId !== undefined &&
      (!Number.isInteger(options.trackId) || options.trackId < 0 || options.trackId > MAX_U32)
   ) {
      throw new TypeError('Subtitle trackId must be an unsigned 32-bit integer.');
   }

   const hasStart = options.startMs !== undefined;
   const hasEnd = options.endMs !== undefined;
   if (hasStart !== hasEnd) {
      throw new TypeError('Subtitle range requires both startMs and endMs.');
   }
   if (!hasStart || !hasEnd) {
      return;
   }

   const startMs = options.startMs as number;
   const endMs = options.endMs as number;
   if (
      !Number.isSafeInteger(startMs) ||
      !Number.isSafeInteger(endMs) ||
      startMs < 0 ||
      endMs < 0
   ) {
      throw new TypeError('Subtitle range endpoints must be non-negative safe integers.');
   }
   if (startMs >= endMs) {
      throw new TypeError('Subtitle range requires startMs to be before endMs.');
   }
}

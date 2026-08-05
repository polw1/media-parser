export const MAX_THUMBNAIL_OUTPUTS = 4_096;

/** Rejects thumbnail requests the backend cannot represent or safely bound. */
export function validateTimestamps(timestamps: number[]): void {
   if (!Array.isArray(timestamps)) {
      throw new TypeError('Thumbnail timestamps must be an array of numbers.');
   }
   if (timestamps.length > MAX_THUMBNAIL_OUTPUTS) {
      throw new TypeError(
         `Thumbnail timestamps must contain at most ${MAX_THUMBNAIL_OUTPUTS} entries.`,
      );
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

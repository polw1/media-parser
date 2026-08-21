export const MAX_THUMBNAIL_OUTPUTS = 4_096;
const MAX_JPEG_DIMENSION = 65_535;

/** Rejects dimensions the JPEG encoder cannot represent. */
export function validateThumbnailDimensions(
   maxWidth: number | undefined,
   maxHeight: number | undefined,
): void {
   for (const dimension of [maxWidth, maxHeight]) {
      if (
         dimension !== undefined &&
         (!Number.isInteger(dimension) || dimension < 1 || dimension > MAX_JPEG_DIMENSION)
      ) {
         throw new TypeError(
            'Thumbnail dimensions must be integers between 1 and 65535.',
         );
      }
   }
}

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

import assert from 'node:assert/strict';
import { test } from 'node:test';

import {
   MAX_THUMBNAIL_OUTPUTS,
   validateThumbnailDimensions,
   validateTimestamps,
} from './thumbnail-options';

test('accepts the maximum thumbnail output count', () => {
   assert.doesNotThrow(() => validateTimestamps(Array(MAX_THUMBNAIL_OUTPUTS).fill(0)));
});

test('rejects output counts above the limit before IPC', () => {
   assert.throws(
      () => validateTimestamps(Array(MAX_THUMBNAIL_OUTPUTS + 1).fill(0)),
      /at most 4096 entries/,
   );
});

test('rejects invalid timestamp values', () => {
   for (const timestamp of [-1, 0.5, Number.NaN, Number.POSITIVE_INFINITY]) {
      assert.throws(() => validateTimestamps([timestamp]), /non-negative integers/);
   }
});

test('accepts valid optional thumbnail dimensions', () => {
   assert.doesNotThrow(() => validateThumbnailDimensions(undefined, undefined));
   assert.doesNotThrow(() => validateThumbnailDimensions(640, 360));
});

test('rejects thumbnail dimensions outside JPEG limits', () => {
   for (const dimension of [0, -1, 1.5, Number.NaN, 65_536]) {
      assert.throws(
         () => validateThumbnailDimensions(dimension, undefined),
         /integers between 1 and 65535/,
      );
   }
});

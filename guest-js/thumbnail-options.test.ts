import assert from 'node:assert/strict';
import { test } from 'node:test';

import { MAX_THUMBNAIL_OUTPUTS, validateTimestamps } from './thumbnail-options';

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

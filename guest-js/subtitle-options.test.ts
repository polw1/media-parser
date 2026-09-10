import assert from 'node:assert/strict';
import { afterEach, test } from 'node:test';

import { clearMocks, mockIPC } from '@tauri-apps/api/mocks';

import { getSubtitles } from './index';
import { validateSubtitleOptions } from './subtitle-options';

Object.assign(globalThis, { window: globalThis });

afterEach(() => clearMocks());

function emptySubtitleEnvelope(): ArrayBuffer {
   const header = new TextEncoder().encode(JSON.stringify({ version: 1, entries: [] }));
   const bytes = new Uint8Array(4 + header.length);
   new DataView(bytes.buffer).setUint32(0, header.length, true);
   bytes.set(header, 4);
   return bytes.buffer;
}

test('accepts omitted and paired subtitle ranges at safe-integer boundaries', () => {
   assert.doesNotThrow(() => validateSubtitleOptions({}));
   assert.doesNotThrow(() => validateSubtitleOptions({ startMs: 0, endMs: 1 }));
   assert.doesNotThrow(() => validateSubtitleOptions({
      startMs: Number.MAX_SAFE_INTEGER - 1,
      endMs: Number.MAX_SAFE_INTEGER,
   }));
});

test('rejects unpaired, empty, and reversed subtitle ranges', () => {
   for (const options of [
      { startMs: 0 },
      { endMs: 1 },
      { startMs: 1, endMs: 1 },
      { startMs: 2, endMs: 1 },
   ]) {
      assert.throws(() => validateSubtitleOptions(options), /subtitle range/i);
   }
});

test('rejects subtitle range endpoints that are not non-negative safe integers', () => {
   for (const endpoint of [
      -1,
      0.5,
      Number.NaN,
      Number.POSITIVE_INFINITY,
      Number.MAX_SAFE_INTEGER + 1,
   ]) {
      assert.throws(
         () => validateSubtitleOptions({ startMs: endpoint, endMs: 2 }),
         /non-negative safe integers/i,
      );
   }
});

test('accepts the complete unsigned 32-bit subtitle track-id range', () => {
   assert.doesNotThrow(() => validateSubtitleOptions({ trackId: 0 }));
   assert.doesNotThrow(() => validateSubtitleOptions({ trackId: 0xffff_ffff }));
});

test('rejects subtitle track IDs outside the unsigned 32-bit integer range', () => {
   for (const trackId of [-1, 0.5, Number.NaN, 0x1_0000_0000]) {
      assert.throws(
         () => validateSubtitleOptions({ trackId }),
         /unsigned 32-bit integer/i,
      );
   }
});

test('allows an empty language and forwards the exact subtitle command arguments', async () => {
   let observedCommand: string | undefined;
   let observedArgs: unknown;
   mockIPC((command, args) => {
      observedCommand = command;
      observedArgs = args;
      return emptySubtitleEnvelope();
   });

   const result = await getSubtitles('/video.mp4', {
      trackId: 7,
      language: '',
      startMs: 10,
      endMs: 20,
      headers: { Authorization: 'Bearer token' },
   });

   assert.deepEqual(result, []);
   assert.equal(observedCommand, 'plugin:media-parser|get_subtitles');
   assert.deepEqual(observedArgs, {
      source: '/video.mp4',
      trackId: 7,
      language: '',
      startMs: 10,
      endMs: 20,
      headers: { Authorization: 'Bearer token' },
   });
});

test('rejects invalid subtitle options before invoking IPC', async () => {
   let invokes = 0;
   mockIPC(() => {
      invokes += 1;
      return emptySubtitleEnvelope();
   });

   await assert.rejects(
      getSubtitles('/video.mp4', { startMs: 2, endMs: 1 }),
      /subtitle range/i,
   );
   assert.equal(invokes, 0);
});

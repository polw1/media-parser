import assert from 'node:assert/strict';
import { test } from 'node:test';

import { decodeSubtitleEnvelope } from './subtitle-envelope';

function buildEnvelope(header: unknown, payload: number[]): Uint8Array {
   return buildEnvelopeFromJson(JSON.stringify(header), payload);
}

function buildEnvelopeFromJson(header: string, payload: number[]): Uint8Array {
   const headerBytes = new TextEncoder().encode(header);
   const envelope = new Uint8Array(4 + headerBytes.length + payload.length);
   new DataView(envelope.buffer).setUint32(0, headerBytes.length, true);
   envelope.set(headerBytes, 4);
   envelope.set(payload, 4 + headerBytes.length);
   return envelope;
}

function cue(overrides: Record<string, unknown> = {}): Record<string, unknown> {
   return {
      cueId: 1,
      startSec: 0.25,
      endSec: 1.5,
      offset: 0,
      length: 5,
      ...overrides,
   };
}

function track(overrides: Record<string, unknown> = {}): Record<string, unknown> {
   return {
      id: 7,
      codec: 'wvtt',
      language: 'eng',
      timescale: 1_000,
      duration: 9_007_199_254_740_991,
      cues: [cue()],
      ...overrides,
   };
}

test('decodes nested subtitle metadata and accepts duplicate payload ranges', () => {
   const header = {
      version: 1,
      entries: [
         track({ cues: [cue(), cue({ cueId: 2 })] }),
         track({ id: 8, language: undefined, cues: [cue({ cueId: 3 })] }),
      ],
   };

   const subtitles = decodeSubtitleEnvelope(buildEnvelope(header, [...Buffer.from('hello')]));

   assert.deepEqual(subtitles, [
      {
         id: 7,
         codec: 'wvtt',
         language: 'eng',
         timescale: 1_000,
         duration: Number.MAX_SAFE_INTEGER,
         cues: [
            { cueId: 1, startSec: 0.25, endSec: 1.5, text: 'hello' },
            { cueId: 2, startSec: 0.25, endSec: 1.5, text: 'hello' },
         ],
      },
      {
         id: 8,
         codec: 'wvtt',
         timescale: 1_000,
         duration: Number.MAX_SAFE_INTEGER,
         cues: [{ cueId: 3, startSec: 0.25, endSec: 1.5, text: 'hello' }],
      },
   ]);
   assert.equal(Object.prototype.hasOwnProperty.call(subtitles[1], 'language'), false);
});

test('reuses one fatal UTF-8 decoder across subtitle cues', () => {
   const NativeTextDecoder = TextDecoder;
   const originalDescriptor = Object.getOwnPropertyDescriptor(globalThis, 'TextDecoder');
   let fatalDecoderConstructions = 0;
   class CountingTextDecoder extends NativeTextDecoder {
      constructor(label?: string, options?: TextDecoderOptions) {
         super(label, options);
         if (options?.fatal === true) {
            fatalDecoderConstructions += 1;
         }
      }
   }

   Object.defineProperty(globalThis, 'TextDecoder', {
      configurable: true,
      writable: true,
      value: CountingTextDecoder,
   });
   const modulePath = require.resolve('./subtitle-envelope');
   delete require.cache[modulePath];

   try {
      const reloadedModule = require('./subtitle-envelope') as typeof import('./subtitle-envelope');
      const header = {
         version: 1,
         entries: [track({ cues: [cue(), cue({ cueId: 2 }), cue({ cueId: 3 })] })],
      };

      reloadedModule.decodeSubtitleEnvelope(
         buildEnvelope(header, [...Buffer.from('hello')]),
      );

      assert.equal(fatalDecoderConstructions, 1);
   } finally {
      delete require.cache[modulePath];
      if (originalDescriptor === undefined) {
         delete (globalThis as { TextDecoder?: typeof TextDecoder }).TextDecoder;
      } else {
         Object.defineProperty(globalThis, 'TextDecoder', originalDescriptor);
      }
   }
});

test('rejects legacy and unknown subtitle envelope versions', () => {
   assert.throws(
      () => decodeSubtitleEnvelope(buildEnvelope([], [])),
      /Unsupported subtitle envelope version: 0/,
   );
   assert.throws(
      () => decodeSubtitleEnvelope(buildEnvelope({ version: 2, entries: [] }, [])),
      /Unsupported subtitle envelope version: 2/,
   );
});

test('rejects malformed nested subtitle shapes', () => {
   const malformedEntries = [
      null,
      track({ codec: 4 }),
      track({ language: null }),
      track({ cues: {} }),
      track({ cues: [null] }),
   ];

   for (const malformed of malformedEntries) {
      assert.throws(
         () => decodeSubtitleEnvelope(buildEnvelope({ version: 1, entries: [malformed] }, [])),
         /subtitle/i,
      );
   }
});

test('rejects unsafe or negative subtitle integer fields', () => {
   const malformedTracks = [
      track({ id: Number.MAX_SAFE_INTEGER + 1 }),
      track({ id: -1 }),
      track({ timescale: 0.5 }),
      track({ duration: Number.MAX_SAFE_INTEGER + 1 }),
      track({ cues: [cue({ cueId: -1 })] }),
      track({ cues: [cue({ offset: Number.MAX_SAFE_INTEGER + 1 })] }),
      track({ cues: [cue({ length: -1 })] }),
   ];

   for (const malformed of malformedTracks) {
      assert.throws(
         () => decodeSubtitleEnvelope(buildEnvelope({ version: 1, entries: [malformed] }, [1, 2, 3, 4, 5])),
         /non-negative safe integer/,
      );
   }
});

test('rejects non-finite, negative, zero-length, and reversed subtitle times', () => {
   const invalidTimes = [
      cue({ startSec: -1 }),
      cue({ startSec: 1, endSec: 1 }),
      cue({ startSec: 2, endSec: 1 }),
      cue({ startSec: null }),
   ];

   for (const invalid of invalidTimes) {
      assert.throws(
         () => decodeSubtitleEnvelope(buildEnvelope({ version: 1, entries: [track({ cues: [invalid] })] }, [1, 2, 3, 4, 5])),
         /finite non-negative times/,
      );
   }

   const infiniteHeader = '{"version":1,"entries":[{"id":1,"codec":"wvtt","timescale":1,"duration":1,"cues":[{"cueId":1,"startSec":0,"endSec":1e309,"offset":0,"length":0}]}]}';
   assert.throws(
      () => decodeSubtitleEnvelope(buildEnvelopeFromJson(infiniteHeader, [])),
      /finite non-negative times/,
   );
});

test('rejects subtitle payload ranges outside or truncated by the envelope', () => {
   const outside = buildEnvelope(
      { version: 1, entries: [track({ cues: [cue({ offset: 2, length: 4 })] })] },
      [1, 2, 3, 4, 5],
   );

   assert.throws(() => decodeSubtitleEnvelope(outside), /payload range is outside/);
});

test('rejects malformed UTF-8 subtitle text instead of replacing bytes', () => {
   const malformed = buildEnvelope(
      { version: 1, entries: [track({ cues: [cue({ length: 1 })] })] },
      [0xff],
   );

   assert.throws(() => decodeSubtitleEnvelope(malformed), /UTF-8/);
});

import assert from 'node:assert/strict';
import { test } from 'node:test';

import { decodeEnvelope } from './envelope';

interface Entry {
   label: string;
   offset: number;
   length: number;
}

function buildEnvelope(header: unknown, payload: number[]): Uint8Array {
   const headerBytes = new TextEncoder().encode(JSON.stringify(header));
   const envelope = new Uint8Array(4 + headerBytes.length + payload.length);
   new DataView(envelope.buffer).setUint32(0, headerBytes.length, true);
   envelope.set(headerBytes, 4);
   envelope.set(payload, 4 + headerBytes.length);
   return envelope;
}

test('decodes a legacy (version 0) bare-array header', () => {
   const envelope = buildEnvelope(
      [{ label: 'a', offset: 0, length: 2 }],
      [1, 2],
   );

   const entries = decodeEnvelope<Omit<Entry, 'offset' | 'length'>>(envelope);

   assert.equal(entries.length, 1);
   assert.equal(entries[0].label, 'a');
   assert.deepEqual([...entries[0].data], [1, 2]);
});

test('decodes a version 1 { version, entries } header', () => {
   const envelope = buildEnvelope(
      { version: 1, entries: [{ label: 'a', offset: 0, length: 2 }, { label: 'b', offset: 2, length: 1 }] },
      [1, 2, 3],
   );

   const entries = decodeEnvelope<Omit<Entry, 'offset' | 'length'>>(envelope);

   assert.equal(entries.length, 2);
   assert.deepEqual([...entries[0].data], [1, 2]);
   assert.deepEqual([...entries[1].data], [3]);
});

test('decodes an empty envelope as zero entries', () => {
   const envelope = buildEnvelope({ version: 1, entries: [] }, []);

   const entries = decodeEnvelope(envelope);

   assert.deepEqual(entries, []);
});

test('rejects an unknown envelope version', () => {
   const envelope = buildEnvelope({ version: 99, entries: [] }, []);

   assert.throws(() => decodeEnvelope(envelope), /Unsupported media envelope version: 99/);
});

test('rejects a version 1 header missing "entries"', () => {
   const envelope = buildEnvelope({ version: 1 }, []);

   assert.throws(() => decodeEnvelope(envelope), /missing an "entries" array/);
});

test('rejects a header that is neither an array nor an object', () => {
   const envelope = buildEnvelope(null, []);

   assert.throws(() => decodeEnvelope(envelope), /must be an array or an object/);
});

test('rejects a version 1 header with a non-numeric version', () => {
   const envelope = buildEnvelope({ version: '1', entries: [] }, []);

   assert.throws(() => decodeEnvelope(envelope), /missing a numeric "version"/);
});

test('rejects an envelope shorter than its length prefix', () => {
   assert.throws(
      () => decodeEnvelope(new Uint8Array([1, 2, 3])),
      /missing the 4-byte header length/,
   );
});

test('rejects a truncated JSON header', () => {
   const envelope = new Uint8Array(6);

   new DataView(envelope.buffer).setUint32(0, 10, true);

   assert.throws(() => decodeEnvelope(envelope), /JSON header is truncated/);
});

test('rejects a payload range outside the envelope', () => {
   const envelope = buildEnvelope(
      { version: 1, entries: [{ label: 'a', offset: 1, length: 3 }] },
      [1, 2],
   );

   assert.throws(() => decodeEnvelope(envelope), /payload range is outside the envelope/);
});

test('rejects non-integer and negative payload ranges', () => {
   const fractional = buildEnvelope(
      { version: 1, entries: [{ label: 'a', offset: 0.5, length: 1 }] },
      [1, 2],
   );
   const negative = buildEnvelope(
      { version: 1, entries: [{ label: 'a', offset: 0, length: -1 }] },
      [1, 2],
   );

   assert.throws(() => decodeEnvelope(fractional), /non-negative safe integers/);
   assert.throws(() => decodeEnvelope(negative), /non-negative safe integers/);
});

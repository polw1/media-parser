/**
 * Decodes binary envelopes returned by `get_cover` and `get_thumbnails`.
 *
 * Layout: a 4-byte little-endian header length, a JSON header, then the
 * concatenated binary payloads. The JSON header is either a bare array of
 * entries (legacy, version 0) or `{ version, entries }` (version 1+).
 */

/** Envelope header shapes this decoder understands. Reject anything else. */
const SUPPORTED_ENVELOPE_VERSIONS = new Set([0, 1]);

interface EnvelopeEntry {
   offset: number;
   length: number;
}

export function decodeEnvelope<T>(raw: ArrayBuffer | Uint8Array): (T & { data: Uint8Array })[] {
   const buffer = raw instanceof Uint8Array ? raw : new Uint8Array(raw);

   if (buffer.byteLength < 4) {
      throw new TypeError('Media envelope is missing the 4-byte header length.');
   }

   const view = new DataView(buffer.buffer, buffer.byteOffset, buffer.byteLength);
   const headerLength = view.getUint32(0, true);

   if (headerLength > buffer.byteLength - 4) {
      throw new TypeError('Media envelope JSON header is truncated.');
   }

   const headerEnd = 4 + headerLength;
   const header = buffer.subarray(4, headerEnd);

   const parsed: unknown = JSON.parse(new TextDecoder().decode(header));
   const { version, entries } = normalizeEnvelopeHeader<T>(parsed);

   if (!SUPPORTED_ENVELOPE_VERSIONS.has(version)) {
      throw new TypeError(`Unsupported media envelope version: ${String(version)}.`);
   }

   const payloadLength = buffer.byteLength - headerEnd;

   return entries.map((entry) => decodeEnvelopeEntry(entry, buffer, headerEnd, payloadLength));
}

function decodeEnvelopeEntry<T>(
   entry: T & EnvelopeEntry,
   buffer: Uint8Array,
   payloadStart: number,
   payloadLength: number,
): T & { data: Uint8Array } {
   if (typeof entry !== 'object' || entry === null || Array.isArray(entry)) {
      throw new TypeError('Media envelope entry must be an object.');
   }

   const offset = entry.offset;
   const length = entry.length;

   if (!Number.isSafeInteger(offset) || offset < 0 ||
       !Number.isSafeInteger(length) || length < 0) {
      throw new TypeError('Media envelope offset and length must be non-negative safe integers.');
   }
   if (offset > payloadLength || length > payloadLength - offset) {
      throw new TypeError('Media envelope payload range is outside the envelope.');
   }

   const decoded: Record<string, unknown> = {};

   Object.keys(entry).forEach((key) => {
      if (key !== 'offset' && key !== 'length') {
         decoded[key] = (entry as Record<string, unknown>)[key];
      }
   });
   decoded.data = buffer.subarray(
      payloadStart + offset,
      payloadStart + offset + length,
   );

   return decoded as T & { data: Uint8Array };
}

function normalizeEnvelopeHeader<T>(
   parsed: unknown,
): { version: number; entries: (T & EnvelopeEntry)[] } {
   if (Array.isArray(parsed)) {
      return { version: 0, entries: parsed as (T & EnvelopeEntry)[] };
   }

   if (typeof parsed !== 'object' || parsed === null) {
      throw new TypeError('Media envelope header must be an array or an object.');
   }

   const header = parsed as { version?: unknown; entries?: unknown };

   if (typeof header.version !== 'number') {
      throw new TypeError('Media envelope header is missing a numeric "version".');
   }
   if (!Array.isArray(header.entries)) {
      throw new TypeError('Media envelope header is missing an "entries" array.');
   }

   return {
      version: header.version,
      entries: header.entries as (T & EnvelopeEntry)[],
   };
}

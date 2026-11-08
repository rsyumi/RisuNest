// Writes a synthetic legacy RisuSave on the host for android-host-measure.mjs. It runs as its own process
// because the encoder must load while Buffer is hidden.
import assert from 'node:assert/strict';
import { createHash } from 'node:crypto';
import { createReadStream } from 'node:fs';
import { open } from 'node:fs/promises';
import path from 'node:path';
import { pathToFileURL } from 'node:url';

// The browser fixture's no-eval encoder must use its browser buffer path on Node too.
const nativeBuffer = globalThis.Buffer;
let fixture;
let Packr;
try {
  globalThis.Buffer = undefined;
  ({ Packr } = await import('msgpackr/index-no-eval'));
  fixture = await import('./fixture.ts');
} finally { globalThis.Buffer = nativeBuffer; }

const pack = new Packr({ useRecords: false });
// The decoded size of everything except a single binary: the smallest fixture planFixture builds.
const SINGLE_BINARY_REST_BYTES = 2_500_000;
const MAX_SINGLE_BINARY_BYTES = 256 * 1024 * 1024;
const CHUNK_BYTES = 1024 * 1024;
const sha256 = value => createHash('sha256').update(value).digest('hex');

// Same bytes as fixture.ts's per-character binary, generated a chunk at a time.
function binaryChunk(offset, length, seed) {
  const values = new Uint8Array(length);
  for (let index = 0; index < length; index++) values[index] = ((offset + index) * 31 + seed) & 0xff;
  return values;
}

// A character packet with `syntheticBinary` between `chatPage` and `chats`, as fixture.ts places it.
function binaryCharacterParts(character, binaryBytes, seed) {
  const { chats, ...head } = character;
  assert.deepEqual(Object.keys(head), ['type', 'chaId', 'name', 'chatPage']);
  const encodedHead = pack.encode(head);
  // The encoder writes an object's member count as a fixmap or a map16; keep its form with two more members.
  const header = encodedHead[0] === 0x84 ? [0x86] : encodedHead[0] === 0xde && encodedHead[1] === 0 && encodedHead[2] === 4
    ? [0xde, 0, 6] : assert.fail('Unexpected character map header');
  const length = new Uint8Array(5);
  length[0] = 0xc6;
  new DataView(length.buffer).setUint32(1, binaryBytes);
  const prefix = Uint8Array.from([...header, ...encodedHead.subarray(header.length), ...pack.encode('syntheticBinary'), ...length]);
  const suffix = Uint8Array.from([...pack.encode('chats'), ...pack.encode(chats)]);
  return { prefix, suffix, binaryBytes, seed };
}

// The streamed packet must equal what the encoder writes for the same object.
function checkBinaryComposition(plan) {
  const character = fixture.fixtureCharacter(plan, plan.fullCharacterCount);
  const bytes = 70_000;
  const parts = binaryCharacterParts(character, bytes, plan.fullCharacterCount);
  const composed = Uint8Array.from([...parts.prefix, ...binaryChunk(0, bytes, parts.seed), ...parts.suffix]);
  const { chats, ...head } = character;
  const encoded = pack.encode({ ...head, syntheticBinary: binaryChunk(0, bytes, parts.seed), chats });
  assert.equal(sha256(composed), sha256(encoded), 'Streamed binary character differs from the encoder');
}

export async function hashFile(file) {
  const digest = createHash('sha256');
  for await (const chunk of createReadStream(file)) digest.update(chunk);
  return digest.digest('hex');
}

export async function writeFixture(file, plan, singleBinaryBytes) {
  const name = new TextEncoder().encode('database.risudat');
  const frame = new Uint8Array(8 + name.length);
  new DataView(frame.buffer).setUint32(0, name.length, true);
  frame.set(name, 4);
  const handle = await open(file, 'wx');
  let databaseBytes = 0;
  let decoded = 0;
  const write = async bytes => { await handle.write(bytes); databaseBytes += bytes.length; };
  try {
    await handle.write(frame);
    await write(Uint8Array.of(0, 82, 73, 83, 85, 83, 65, 86, 69, 0, 7));
    const tailPacket = 3 + plan.fullCharacterCount;
    let packet = 0;
    for (const bytes of fixture.fixturePackets(plan)) {
      if (singleBinaryBytes > 0 && packet === tailPacket) {
        const parts = binaryCharacterParts(fixture.fixtureCharacter(plan, plan.fullCharacterCount), singleBinaryBytes,
          plan.fullCharacterCount);
        await write(parts.prefix);
        for (let offset = 0; offset < singleBinaryBytes; offset += CHUNK_BYTES)
          await write(binaryChunk(offset, Math.min(CHUNK_BYTES, singleBinaryBytes - offset), parts.seed));
        await write(parts.suffix);
        decoded += parts.prefix.length + singleBinaryBytes + parts.suffix.length;
      } else {
        await write(bytes);
        decoded += bytes.length;
      }
      packet++;
    }
    const length = new Uint8Array(4);
    new DataView(length.buffer).setUint32(0, databaseBytes, true);
    await handle.write(length, 0, 4, 4 + name.length);
  } finally { await handle.close(); }
  return { decodedBytes: decoded, sourceBytes: frame.length + databaseBytes };
}

/**
 * A raw legacy fixture for `runId`: a decoded-size case with optional per-character binaries, or with
 * `singleBinaryBytes` one bin of that size in the last character of the smallest fixture.
 */
export async function buildHostFixture({ file, runId, megabytes, binaryBytes = 0, singleBinaryBytes = 0 }) {
  assert.ok(Number.isSafeInteger(singleBinaryBytes) && singleBinaryBytes >= 0 && singleBinaryBytes <= MAX_SINGLE_BINARY_BYTES,
    `Single binary bytes must be between 0 and ${MAX_SINGLE_BINARY_BYTES}`);
  if (singleBinaryBytes > 0) {
    assert.ok(singleBinaryBytes > 65_535, 'A single binary is a bin32 of more than 65,535 bytes');
    assert.ok(!megabytes && !binaryBytes, 'A single binary case sets its own decoded size and no per-character binaries');
  } else assert.ok(fixture.MEASUREMENT_MB.includes(megabytes), 'Explicit legacy MB required');
  const plan = fixture.planFixture(singleBinaryBytes > 0 ? SINGLE_BINARY_REST_BYTES : megabytes * 1_000_000, runId, binaryBytes);
  if (singleBinaryBytes > 0) checkBinaryComposition(plan);
  const written = await writeFixture(file, plan, singleBinaryBytes);
  if (singleBinaryBytes === 0) assert.equal(written.decodedBytes, plan.decodedBytes, 'Decoded byte accounting mismatch');
  const conversations = [];
  for (let index = 0; index < plan.characterCount; index++) {
    const character = fixture.fixtureCharacter(plan, index);
    conversations.push({ characterId: character.chaId, conversationId: character.chats[0].id,
      sha256: sha256(fixture.messageProjection(character.chats[0].message)) });
  }
  const tail = fixture.fixtureCharacter(plan, plan.fullCharacterCount);
  const binary = singleBinaryBytes > 0 ? { mode: 'summary', characterId: tail.chaId, seed: plan.fullCharacterCount }
    : plan.binaryBytes > 0 ? { mode: 'read', characterId: tail.chaId, sha256: sha256(JSON.stringify(tail.syntheticBinary)) }
    : null;
  return { characterCount: plan.characterCount, messageCount: plan.messageCount, binaryBytes: plan.binaryBytes,
    singleBinaryBytes, restDecodedBytes: plan.decodedBytes, decodedBytes: written.decodedBytes,
    sourceBytes: written.sourceBytes, fixtureSha256: await hashFile(file), conversations, binary };
}

if (process.argv[1] && pathToFileURL(path.resolve(process.argv[1])).href === import.meta.url) {
  const [file, runId, megabytes, binaryBytes, singleBinaryBytes] = process.argv.slice(2);
  const result = await buildHostFixture({ file, runId, megabytes: Number(megabytes), binaryBytes: Number(binaryBytes),
    singleBinaryBytes: Number(singleBinaryBytes) });
  process.stdout.write(JSON.stringify(result));
}

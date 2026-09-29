import test from 'node:test';
import assert from 'node:assert/strict';
import { mkdtempSync, readFileSync, statSync, rmSync } from 'node:fs';
import { tmpdir } from 'node:os';
import path from 'node:path';
import { createCapture, describeCommand } from './capture.mjs';

const string = value => { const b = Buffer.from(value), h = Buffer.alloc(3); h[0] = 2; h.writeUInt16BE(b.length, 1); return Buffer.concat([h, b]); };
const number = value => { const b = Buffer.alloc(9); b.writeDoubleBE(value, 1); return b; };
const field = (key, value) => Buffer.concat([string(key).subarray(1), value]);
const object = fields => Buffer.concat([Buffer.from([3]), ...fields, Buffer.from([0, 0, 9])]);
const command = (name, args) => Buffer.concat([string(name), number(1), ...args]);

test('full capture keeps string values and exact AMF bytes, including undecodable frames', () => {
  const directory = mkdtempSync(path.join(tmpdir(), 'vpt-capture-full-'));
  try {
    const file = path.join(directory, 'trace.jsonl');
    const capture = createCapture(file, { full: true });
    const bytes = command('connect', [object([field('swfUrl', string('https://example.invalid/game.swf?pass=test-secret'))]), string('test-account')]);
    capture.record(1, 'client-to-server', bytes);
    const unsupported = Buffer.from([17, 1, 2, 3]);
    capture.record(1, 'server-to-client', unsupported);
    capture.close();
    const rows = readFileSync(file, 'utf8').trim().split('\n').map(JSON.parse);
    assert.equal(rows[0].commandObject.fields[0][1], 'https://example.invalid/game.swf?pass=test-secret');
    assert.equal(rows[0].arguments[0], 'test-account');
    assert.deepEqual(Buffer.from(rows[0].rawBase64, 'base64'), bytes);
    assert.deepEqual(Buffer.from(rows[1].rawBase64, 'base64'), unsupported);
    assert.equal(rows[1].decodeError, 'invalid-command');
  } finally { rmSync(directory, { recursive: true }); }
});

test('captures connect structure while redacting URLs, credentials and nested strings', () => {
  const array = Buffer.concat([Buffer.from([8, 0, 0, 0, 3]),
    field('0', string('private-user')), field('1', string('private-password')),
    field('2', object([field('token', string('private-token'))])), Buffer.from([0, 0, 9])]);
  const result = describeCommand(command('connect', [object([
    field('swfUrl', string('https://example.invalid/game.swf?pass=private-secret')),
    field('objectEncoding', number(0)),
  ]), array]));
  assert.equal(result.command, 'connect');
  assert.equal(result.transaction, 1);
  assert.equal(result.commandObject.fields[0][0], 'swfUrl');
  assert.equal(result.commandObject.fields[0][1].redacted, true);
  assert.equal(result.arguments[0].type, 'ecma-array');
  assert.equal(result.arguments[0].fields.length, 3);
  assert.doesNotMatch(JSON.stringify(result), /private-|https:\/\//);
});

test('records response IDs and fixed statuses, never raw fallback on unsupported or truncated AMF', () => {
  const result = describeCommand(command('_result', [Buffer.from([5]), object([
    field('code', string('NetConnection.Connect.Success')), field('description', string('private-message')),
  ])]));
  assert.equal(result.arguments[0].fields[0][1], 'NetConnection.Connect.Success');
  assert.doesNotMatch(JSON.stringify(result), /private-message/);
  const unknown = command('onData', [Buffer.from([5, 17]), string('private-message')]);
  assert.match(describeCommand(unknown).decodeError, /unsupported-marker/);
  assert.doesNotMatch(JSON.stringify(describeCommand(unknown)), /private-message/);
  assert.equal(describeCommand(Buffer.from([2, 0, 8])).decodeError, 'truncated-amf');
  const large = command('onData', [Buffer.from([5, 10, 255, 255, 255, 255])]);
  assert.equal(describeCommand(large).decodeError, 'decode-limit');
});

test('capture creates a private new file and stops at a bounded size', () => {
  const directory = mkdtempSync(path.join(tmpdir(), 'vpt-capture-'));
  try {
    const file = path.join(directory, 'trace.jsonl');
    const capture = createCapture(file, { maxBytes: 500 });
    const bytes = command('ping', [Buffer.from([5])]);
    for (let i = 0; i < 10; i++) capture.record(2, 'client-to-server', bytes);
    capture.close();
    const lines = readFileSync(file, 'utf8').trim().split('\n').map(JSON.parse);
    assert.equal(lines[0].command, 'ping');
    assert.equal(lines.at(-1).event, 'capture-limit');
    assert.ok(statSync(file).size < 600);
    assert.equal(statSync(file).mode & 0o777, 0o600);
    assert.throws(() => createCapture(file), /EEXIST/);
  } finally { rmSync(directory, { recursive: true }); }
});

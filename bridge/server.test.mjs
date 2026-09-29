import test from 'node:test';
import assert from 'node:assert/strict';
import { mkdtemp, writeFile, readFile, rm } from 'node:fs/promises';
import { tmpdir } from 'node:os';
import path from 'node:path';
import { once } from 'node:events';
import { WebSocket } from 'ws';
import { createBridge, validateTarget, maxConnectionsFromEnv, DEFAULT_MAX_CONNECTIONS } from './server.mjs';

test('default bridge disables full capture while still relaying game frames', async t => {
  const directory = await mkdtemp(path.join(tmpdir(), 'vpt-no-capture-'));
  const worker = path.join(directory, 'echo-worker');
  await writeFile(worker, `#!${process.execPath}\nprocess.stdin.pipe(process.stdout);\n`, { mode: 0o755 });
  const bridge = createBridge({ port: 0, worker });
  t.after(async () => { await bridge.close(); await rm(directory, { recursive: true }); });
  await once(bridge.server, 'listening');
  const base = `127.0.0.1:${bridge.server.address().port}`;
  const health = await (await fetch(`http://${base}/health`)).json();
  assert.equal(health.captureEnabled, false);
  const capture = new WebSocket(`ws://${base}/capture`, { origin: 'http://127.0.0.1:5173' });
  const [error] = await once(capture, 'error');
  assert.match(error.message, /403/);
  const game = new WebSocket(`ws://${base}/rtmp`, { origin: 'http://127.0.0.1:5173' });
  await once(game, 'open');
  game.send('rtmpe://103.116.100.95:80/master/test/');
  const payload = Buffer.from([2, 0, 4, 112, 105, 110, 103, 0, 0]);
  const reply = once(game, 'message');
  game.send(payload);
  assert.deepEqual((await reply)[0], payload);
});

test('target policy keeps RTMPE app paths and rejects non-game destinations', () => {
  assert.equal(validateTarget('rtmpe://103.116.100.95:80/master/test/'), 'rtmpe://103.116.100.95:80/master/test/');
  for (const value of ['rtmpe://103.116.100.95:80', 'rtmpe://103.116.100.95:80/', 'http://103.116.100.95:80/master/test/', 'rtmpe://127.0.0.1:80/app',
    'rtmpe://103.116.100.95:81/app', 'rtmpe://name:secret@103.116.100.95:80/app',
    'rtmpe://103.116.100.95:80/app?pass=secret', 'rtmpe://103.116.100.95:80/app\nconn=foo']) {
    assert.equal(validateTarget(value), null);
  }
});

test('default policy accepts every server in the publisher range and nothing outside it', () => {
  for (const value of ['rtmpe://103.116.100.94:80/master/test/', 'rtmp://103.116.100.94:80/tcn/line3/', 'rtmpe://103.116.100.255:80/master/test/']) {
    assert.equal(validateTarget(value), value);
  }
  for (const value of ['rtmpe://103.116.101.94:80/master/test/', 'rtmpe://103.116.100.94.evil.test:80/app', 'rtmpe://103.116.100.256:80/app',
    'rtmpe://103.116.100.94:1935/app', 'rtmpe://x103.116.100.94:80/app']) {
    assert.equal(validateTarget(value), null);
  }
});

test('explicit targets still match exactly unless they use a wildcard octet', () => {
  assert.equal(validateTarget('rtmpe://10.0.0.5:80/app', ['10.0.0.5:80']), 'rtmpe://10.0.0.5:80/app');
  assert.equal(validateTarget('rtmpe://10.0.0.6:80/app', ['10.0.0.5:80']), null);
  assert.equal(validateTarget('rtmpe://10.0.0.6:80/app', ['10.0.0.*:80']), 'rtmpe://10.0.0.6:80/app');
  assert.equal(validateTarget('rtmpe://10.0.1.6:80/app', ['10.0.0.*:80']), null);
});

test('web tabs share a connection limit that the environment can raise', () => {
  assert.equal(DEFAULT_MAX_CONNECTIONS, 64);
  assert.equal(maxConnectionsFromEnv(undefined), 64);
  assert.equal(maxConnectionsFromEnv('128'), 128);
  for (const bad of ['0', '-1', '1.5', 'abc', '999']) assert.equal(maxConnectionsFromEnv(bad), 64);
});

test('rejects foreign origins before starting a worker', async () => {
  const bridge = createBridge({ port: 0 });
  await once(bridge.server, 'listening');
  const ws = new WebSocket(`ws://127.0.0.1:${bridge.server.address().port}/rtmp`, { origin: 'https://example.com' });
  const [error] = await once(ws, 'error');
  assert.match(error.message, /403/);
  await bridge.close();
});

test('reports missing worker without treating WebSocket open as RTMP success', async () => {
  const bridge = createBridge({ port: 0, worker: '/nonexistent/rtmp-worker' });
  await once(bridge.server, 'listening');
  const ws = new WebSocket(`ws://127.0.0.1:${bridge.server.address().port}/rtmp`, { origin: 'http://127.0.0.1:5173' });
  await once(ws, 'open');
  ws.send('rtmpe://103.116.100.95:80/master/test/');
  const [code] = await once(ws, 'close');
  assert.equal(code, 1011);
  await bridge.close();
});


test('preserves binary commands across split worker frames and cleans up', async (t) => {
  const directory = await mkdtemp(path.join(tmpdir(), 'vpt-bridge-'));
  const worker = path.join(directory, 'echo-worker');
  await writeFile(worker, `#!${process.execPath}
let buffer = Buffer.alloc(0);
process.stdin.on('data', chunk => {
  buffer = Buffer.concat([buffer, chunk]);
  while (buffer.length >= 4 && buffer.length >= buffer.readUInt32BE(0) + 4) {
    const length = buffer.readUInt32BE(0) + 4;
    const frame = Buffer.from(buffer.subarray(0, length));
    buffer = buffer.subarray(length);
    process.stdout.write(frame.subarray(0, 2));
    setImmediate(() => process.stdout.write(frame.subarray(2)));
  }
});
`, { mode: 0o755 });
  const capturePath = path.join(directory, 'capture.jsonl');
  const bridge = createBridge({ port: 0, worker, capturePath, captureFull: true });
  t.after(async () => { await bridge.close(); await rm(directory, { recursive: true }); });
  await once(bridge.server, 'listening');
  const viewer = new WebSocket(`ws://127.0.0.1:${bridge.server.address().port}/capture`, { origin: 'http://127.0.0.1:5173' });
  const observed = [];
  viewer.on('message', bytes => observed.push(JSON.parse(bytes.toString())));
  await once(viewer, 'open');
  const ws = new WebSocket(`ws://127.0.0.1:${bridge.server.address().port}/rtmp`, { origin: 'http://127.0.0.1:5173' });
  await once(ws, 'open');
  ws.send('rtmpe://103.116.100.95:80/master/test/');
  const payload = Buffer.from([2, 0, 4, 116, 101, 115, 116, 0, 255, 128]);
  const reply = once(ws, 'message');
  ws.send(payload);
  const [actual, binary] = await reply;
  assert.equal(binary, true);
  assert.deepEqual(actual, payload);
  const capture = (await readFile(capturePath, 'utf8')).trim().split('\n').map(JSON.parse);
  assert.deepEqual(capture.map(row => row.direction), ['client-to-server', 'server-to-client']);
  assert.ok(capture.every(row => row.connection === 1 && row.bytes === payload.length && row.decodeError));
  assert.deepEqual(Buffer.from(capture[0].rawBase64, 'base64'), payload);
  ws.send('unexpected second control message');
  const [code] = await once(ws, 'close');
  assert.equal(code, 1008);
  assert.deepEqual(observed, capture);
  const denied = new WebSocket(`ws://127.0.0.1:${bridge.server.address().port}/capture`, { origin: 'https://example.com' });
  const [error] = await once(denied, 'error');
  assert.match(error.message, /403/);
  viewer.send('not a command channel');
  const [viewerCode] = await once(viewer, 'close');
  assert.equal(viewerCode, 1008);
});

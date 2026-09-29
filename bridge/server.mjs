import { createServer } from 'node:http';
import { spawn } from 'node:child_process';
import { fileURLToPath } from 'node:url';
import { WebSocketServer, WebSocket } from 'ws';
import { createCapture } from './capture.mjs';

export const MAX_FRAME = 8 * 1024 * 1024;
const defaultTargets = ['103.116.100.95:80'];
const defaultOrigins = ['http://127.0.0.1:5173', 'http://127.0.0.1:4173'];

export function validateTarget(value, targets = defaultTargets) {
  if (typeof value !== 'string' || value.length > 2048 || /[\s\x00-\x1f]/.test(value)) return null;
  try {
    const url = new URL(value);
    if (!['rtmp:', 'rtmpe:'].includes(url.protocol) || url.username || url.password || url.search || url.hash) return null;
    if (!targets.includes(`${url.hostname}:${url.port || '1935'}`) || !url.pathname || url.pathname === '/') return null;
    return url.href;
  } catch { return null; }
}

// Opt-in metadata only: never print arguments, URLs, account data or raw AMF.
function traceCommand(direction, bytes) {
  if (process.env.VPT_RTMP_DIAGNOSTICS !== '1') return;
  const labels = ['connect', '_result', '_error', 'onStatus', 'onLineList', 'close'];
  const size = bytes.length >= 3 && bytes[0] === 2 ? bytes.readUInt16BE(1) : 0;
  const name = bytes.subarray(3, 3 + size).toString();
  const label = labels.includes(name) ? name : 'RPC';
  const codes = ['NetConnection.Connect.Success', 'NetConnection.Connect.Rejected', 'NetConnection.Connect.Failed', 'ERR_LOGIN_FAILED', 'ERR_USER_EXIST', 'SERVER_NOT_READY', 'ERR_LOGINED', 'ERR_LOGIN_BANNED', 'ERR_IP_BAN'];
  const code = codes.filter(value => bytes.includes(Buffer.from(value))).join(' ');
  console.info(`${direction} ${label} (${bytes.length} bytes) ${code}`);
}

export function createBridge({ port = 8181, targets = defaultTargets, origins = defaultOrigins,
  capturePath, captureFull = false,
  worker = fileURLToPath(new URL('./bin/rtmp-worker', import.meta.url)) } = {}) {
  const capture = capturePath ? createCapture(capturePath, { full: captureFull }) : null;
  let connectionId = 0;
  const server = createServer((request, response) => {
    response.writeHead(request.url === '/health' ? 200 : 404, { 'Content-Type': 'application/json' });
    response.end(JSON.stringify({ service: 'vpt-rtmpe-bridge', protocol: 1, captureEnabled: !!capture }));
  });
  const sockets = new WebSocketServer({ noServer: true, maxPayload: MAX_FRAME, perMessageDeflate: false });
  const viewers = new WebSocketServer({ noServer: true, maxPayload: 1024, perMessageDeflate: false });
  const recordCommand = (connection, direction, bytes) => {
    const record = capture?.record(connection, direction, bytes);
    if (!record) return;
    const message = JSON.stringify(record);
    for (const viewer of viewers.clients) {
      if (viewer.readyState === WebSocket.OPEN && viewer.bufferedAmount < 2 * MAX_FRAME) viewer.send(message);
      else viewer.close(1013, 'Capture viewer too slow; full data remains in file');
    }
  };
  server.on('upgrade', (request, socket, head) => {
    if (request.url === '/capture' && capture && origins.includes(request.headers.origin) && viewers.clients.size < 8) {
      viewers.handleUpgrade(request, socket, head, viewer => {
        viewer.on('error', () => {});
        viewer.on('message', () => viewer.close(1008, 'Capture is read-only'));
      });
      return;
    }
    if (request.url !== '/rtmp' || !origins.includes(request.headers.origin) || sockets.clients.size >= 8) {
      socket.end('HTTP/1.1 403 Forbidden\r\nConnection: close\r\n\r\n');
      return;
    }
    sockets.handleUpgrade(request, socket, head, ws => sockets.emit('connection', ws));
  });
  sockets.on('connection', ws => {
    const connection = ++connectionId;
    let child, buffer = Buffer.alloc(0), initialized = false;
    const deadline = setTimeout(() => ws.close(1008, 'Connection timed out'), 20000);
    const cleanup = () => { clearTimeout(deadline); child?.kill('SIGTERM'); };
    ws.on('error', cleanup);
    ws.on('close', cleanup);
    ws.on('message', (data, binary) => {
      if (!initialized) {
        const url = !binary && validateTarget(data.toString(), targets);
        if (!url) { ws.close(1008, 'Destination is not allowed'); return; }
        initialized = true;
        child = spawn(worker, [url], { stdio: ['pipe', 'pipe', 'pipe'] });
        child.on('error', () => ws.close(1011, 'RTMP worker unavailable'));
        child.stdin.on('error', () => ws.close(1011, 'RTMP worker stopped'));
        child.on('close', () => ws.close(1000, 'RTMP connection closed'));
        child.stderr.on('data', chunk => {
          // Only emit our fixed diagnostics; never forward library output or AMF.
          for (const line of chunk.toString().split('\n')) {
            if (['RTMP handshake complete', 'RTMP handshake failed'].includes(line)) console.info(line);
          }
        });
        child.stdout.on('data', chunk => {
          buffer = Buffer.concat([buffer, chunk]);
          while (buffer.length >= 4) {
            const length = buffer.readUInt32BE(0);
            if (!length || length > MAX_FRAME) { ws.close(1009, 'Invalid RTMP frame'); cleanup(); return; }
            if (buffer.length < length + 4) break;
            if (ws.readyState !== WebSocket.OPEN || ws.bufferedAmount > MAX_FRAME) { cleanup(); ws.close(1011, 'Receiver too slow'); return; }
            clearTimeout(deadline);
            traceCommand("server → client", buffer.subarray(4, length + 4));
            recordCommand(connection, 'server-to-client', buffer.subarray(4, length + 4));
            ws.send(buffer.subarray(4, length + 4));
            buffer = buffer.subarray(length + 4);
          }
        });
        return;
      }
      if (!binary || data.length === 0 || child.stdin.writableLength + data.length > MAX_FRAME) {
        ws.close(1008, 'Expected bounded AMF command'); return;
      }
      const header = Buffer.alloc(4);
      header.writeUInt32BE(data.length);
      traceCommand("client → server", data);
      recordCommand(connection, 'client-to-server', data);
      child.stdin.write(Buffer.concat([header, data]));
    });
  });
  server.listen(port, '127.0.0.1');
  return { server, sockets, close: () => {
    for (const client of sockets.clients) client.terminate();
    for (const viewer of viewers.clients) viewer.terminate();
    sockets.close();
    viewers.close();
    capture?.close();
    return new Promise(resolve => server.close(resolve));
  } };
}

if (process.argv[1] === fileURLToPath(import.meta.url)) {
  const bridge = createBridge({ targets: process.env.VPT_RTMP_TARGETS?.split(',') || defaultTargets });
  console.info('Log toàn bộ đã tắt. Dùng Record trên web để ghi thao tác.');
  bridge.server.on('listening', () => console.info('RTMPE bridge: ws://127.0.0.1:8181/rtmp'));
  bridge.server.on('error', error => { console.error(error.code); process.exitCode = 1; });
  for (const signal of ['SIGINT', 'SIGTERM']) process.once(signal, async () => { await bridge.close(); });
}

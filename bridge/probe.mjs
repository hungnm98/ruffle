// Probe transport only: no account or credential is sent.
import { spawn } from 'node:child_process';
import { fileURLToPath } from 'node:url';
import { validateTarget } from './server.mjs';
const url = validateTarget(process.argv[2] || 'rtmpe://103.116.100.95:80/master/test/');
if (!url) throw new Error('Destination not allowed');
function string(value) {
  const bytes = Buffer.from(value); const length = Buffer.alloc(2); length.writeUInt16BE(bytes.length);
  return Buffer.concat([Buffer.from([2]), length, bytes]);
}
function number(value) { const bytes = Buffer.alloc(9); bytes.writeDoubleBE(value, 1); return bytes; }
function field(name, value) { return Buffer.concat([string(name).subarray(1), value]); }
const object = Buffer.concat([Buffer.from([3]),
  field('app', string(new URL(url).pathname.replace(/^\/+|\/+$/g, ''))),
  field('tcUrl', string(url)), field('flashVer', string('WIN 32,0,0,465')),
  field('objectEncoding', number(0)), Buffer.from([0, 0, 9])]);
const body = Buffer.concat([string('connect'), number(1), object]);
const length = Buffer.alloc(4); length.writeUInt32BE(body.length);
const child = spawn(fileURLToPath(new URL('./bin/rtmp-worker', import.meta.url)), [url]);
child.stdin.on('error', () => {});
child.stderr.pipe(process.stderr);
child.stdin.write(Buffer.concat([length, body]));
const timeout = setTimeout(() => { console.error('No command reply within 20s'); child.kill(); process.exitCode = 1; }, 20000);
let received = Buffer.alloc(0);
child.stdout.on('data', chunk => {
  received = Buffer.concat([received, chunk]);
  if (received.length >= 4 && received.length >= received.readUInt32BE(0) + 4) {
    const message = received.subarray(4);
    const name = message[0] === 2 ? message.subarray(3, 3 + message.readUInt16BE(1)).toString() : 'unknown';
    console.log('Received AMF command:', name);
    const transactionOffset = 3 + message.readUInt16BE(1);
    if (message[transactionOffset] === 0 && transactionOffset + 9 <= message.length) console.log('Transaction:', message.readDoubleBE(transactionOffset + 1));
    // This probe sends no account information, so its rejection description is public diagnostic data.
    for (const key of ['code', 'description', 'application']) {
      const marker = Buffer.concat([Buffer.from([0, key.length]), Buffer.from(key), Buffer.from([2])]);
      const offset = message.indexOf(marker);
      if (offset >= 0 && offset + marker.length + 2 <= message.length) {
        const start = offset + marker.length;
        const size = message.readUInt16BE(start);
        if (size <= 512 && start + 2 + size <= message.length) console.log(key + ':', message.subarray(start + 2, start + 2 + size).toString());
      }
    }
    console.log('Transport response received; authentication has not been tested.');
    clearTimeout(timeout); child.stdin.end();
  }
});
child.on('close', (code, signal) => { console.log('Worker exit:', code, signal); clearTimeout(timeout); if (!received.length) process.exitCode = code || 1; });

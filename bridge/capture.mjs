import { openSync, writeSync, closeSync } from 'node:fs';

const statuses = new Set(['NetConnection.Connect.Success', 'NetConnection.Connect.Rejected',
  'NetConnection.Connect.Failed', 'NetConnection.Connect.Closed', 'NetConnection.Call.Failed',
  'SERVER_NOT_READY', 'ERR_LOGIN_FAILED', 'ERR_LOGINED', 'ERR_USER_EXIST']);

// Diagnostic AMF0 decoder; full mode retains strings exactly as received.
export function describeCommand(bytes, { full = false } = {}) {
  let offset = 0, nodes = 0;
  const take = size => {
    if (size > bytes.length - offset) throw new Error('truncated-amf');
    const data = bytes.subarray(offset, offset + size); offset += size; return data;
  };
  const u16 = () => take(2).readUInt16BE(0);
  const u32 = () => take(4).readUInt32BE(0);
  const text = size => take(size).toString('utf8');
  const string = size => {
    const value = text(size);
    return full || statuses.has(value) ? value : { type: 'string', bytes: size, redacted: true };
  };
  const fields = depth => {
    const entries = [];
    while (true) {
      const size = u16();
      if (!size && bytes[offset] === 9) { offset++; return entries; }
      const key = text(size);
      entries.push([full || /^[\w.]{1,64}$/.test(key) ? key : '[redacted-key]', value(depth + 1)]);
    }
  };
  const value = depth => {
    if (++nodes > 10000 || depth > 32) throw new Error('decode-limit');
    const marker = take(1)[0];
    switch (marker) {
      case 0: { const number = take(8).readDoubleBE(0); return Number.isFinite(number) ? number : { type: 'number', value: String(number) }; }
      case 1: return take(1)[0] !== 0;
      case 2: return string(u16());
      case 3: return { type: 'object', fields: fields(depth) };
      case 5: return null;
      case 6: return { type: 'undefined' };
      case 7: return { type: 'reference', index: u16() };
      case 8: return { type: 'ecma-array', declaredLength: u32(), fields: fields(depth) };
      case 10: {
        const length = u32();
        if (length > 10000) throw new Error('decode-limit');
        return { type: 'strict-array', items: Array.from({ length }, () => value(depth + 1)) };
      }
      case 11: return { type: 'date', milliseconds: take(8).readDoubleBE(0), timezone: take(2).readInt16BE(0) };
      case 12: return string(u32());
      case 13: return { type: 'unsupported' };
      case 15: { const size = u32(); const xml = text(size); return full ? { type: 'xml', value: xml } : { type: 'xml', bytes: size, redacted: true }; }
      case 16: { const className = string(u16()); return { type: 'typed-object', className, fields: fields(depth) }; }
      default: throw new Error(`unsupported-marker-${marker}`);
    }
  };
  const result = {};
  try {
    if (take(1)[0] !== 2) throw new Error('invalid-command');
    const name = text(u16());
    result.command = full || /^[A-Za-z_][\w./]{0,127}$/.test(name) ? name : '[redacted-command]';
    result.transaction = value(0);
    result.commandObject = value(0);
    result.arguments = [];
    while (offset < bytes.length) result.arguments.push(value(0));
  } catch (error) {
    // Keep a bounded diagnostic, never raw bytes or partial string contents.
    result.decodeError = error.message;
  }
  return result;
}

export function createCapture(path, { maxBytes = Infinity, full = false } = {}) {
  // Exclusive creation avoids overwriting an earlier capture or following a symlink.
  let fd = openSync(path, 'wx', 0o600), written = 0, sequence = 0;
  const close = () => { if (fd !== null) { closeSync(fd); fd = null; } };
  return {
    record(connection, direction, bytes) {
      if (fd === null) return;
      try {
        const record = { timestamp: new Date().toISOString(), sequence: ++sequence,
          connection, direction, bytes: bytes.length, ...describeCommand(bytes, { full }),
          ...(full ? { rawBase64: bytes.toString('base64') } : {}) };
        const line = JSON.stringify(record) + '\n';
        if (written + Buffer.byteLength(line) > maxBytes) {
          writeSync(fd, JSON.stringify({ event: 'capture-limit', maxBytes }) + '\n');
          close(); return;
        }
        writeSync(fd, line); written += Buffer.byteLength(line);
        return record;
      } catch {
        // Capture failure must not break the game connection.
        close(); console.error('RTMP capture stopped: file write failed');
      }
    },
    close,
  };
}

import { execFileSync } from 'node:child_process';
import { existsSync, mkdirSync, readFileSync, writeFileSync } from 'node:fs';
import { fileURLToPath } from 'node:url';
import path from 'node:path';
const directory = fileURLToPath(new URL('.', import.meta.url));
const source = path.resolve(directory, '../.tools/rtmpdump');
const revision = '138fdb258d9fc26f1843fd1b891180416c9dc575';
mkdirSync(`${directory}bin`, { recursive: true });
if (!existsSync(source)) {
  execFileSync('git', ['clone', 'https://git.ffmpeg.org/rtmpdump.git', source], { stdio: 'inherit' });
}
execFileSync('git', ['-C', source, 'checkout', '--detach', revision], { stdio: 'inherit' });
// Upstream passes uninitialized BIGNUM pointers to BN_hex2bn. Initialize
// them before OpenSSL reads *bn; keep this local fix reproducible.
const dhPath = `${source}/librtmp/dh.h`;
const dh = readFileSync(dhPath, 'utf8');
const patchedDh = dh.replace('MP_t g, p;', 'MP_t g = NULL, p = NULL;').replace('MP_t q1;', 'MP_t q1 = NULL;')
  // OpenSSL 3 rejects a private exponent longer than the safe-prime subgroup.
  .replace('MP_setlength(dh, nKeyBits);', 'MP_setlength(dh, nKeyBits - 1);');
if (dh !== patchedDh) writeFileSync(dhPath, patchedDh);
const env = { ...process.env };
// Windows: run from an MSYS2 MINGW64 shell (gcc, make, pkg-config, mingw-w64 OpenSSL/zlib).
// The worker is linked statically so rtmp-worker.exe needs only Windows system DLLs.
const windows = process.platform === 'win32';
if (process.platform === 'darwin') {
  const prefix = execFileSync('brew', ['--prefix', 'openssl@3'], { encoding: 'utf8' }).trim();
  env.PKG_CONFIG_PATH = `${prefix}/lib/pkgconfig${env.PKG_CONFIG_PATH ? ':' + env.PKG_CONFIG_PATH : ''}`;
}
const flags = kind => execFileSync('pkg-config', [...(windows ? ['--static'] : []), kind, 'openssl'], { encoding: 'utf8', env }).trim().split(/\s+/).filter(Boolean);
// Build a pinned static librtmp against the same crypto library as the worker.
// The older system library may have an incompatible OpenSSL ABI.
if (windows) execFileSync('make', ['-C', `${source}/librtmp`, 'clean'], { stdio: 'inherit', env });
execFileSync('make', ['-C', `${source}/librtmp`, 'SHARED=no', `SYS=${windows ? 'mingw' : process.platform === 'darwin' ? 'darwin' : 'posix'}`, `INC=${flags('--cflags').join(' ')}`, 'librtmp.a'], { stdio: 'inherit', env });
execFileSync(windows ? 'gcc' : 'cc', ['-std=c11', ...(windows ? ['-static'] : ['-D_POSIX_C_SOURCE=200809L']), '-Wall', '-Wextra', '-Werror', '-O2', `-I${source}`, ...flags('--cflags'),
  `${directory}rtmp-worker.c`, `${source}/librtmp/librtmp.a`, '-o', `${directory}bin/rtmp-worker${windows ? '.exe' : ''}`, ...flags('--libs'), '-lz',
  ...(windows ? ['-lws2_32', '-lwinmm', '-lgdi32', '-lcrypt32', '-luser32', '-ladvapi32'] : [])], { stdio: 'inherit', env });
console.log('RTMP worker built');

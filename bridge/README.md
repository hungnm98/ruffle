# VPT RTMPE bridge

Local experimental fork for the Vua Pháp Thuật AVM2 client. Base: Ruffle v0.6.0 (`cac5c99ce4a17e606f4ee3090389bb878f852055`), branch `vpt-rtmpe`. This directory and the Ruffle changes belong to the nested Git repository; the parent web project consumes its compiled self-hosted runtime.

## Architecture

```
SWF NetConnection / AMF0
  → Ruffle core (connect, RPC, Responder, client callbacks, NetStatus)
  → web navigator (explicit rtmpProxy option)
  → ws://127.0.0.1:8181/rtmp
  → Node bridge → native librtmp worker → RTMPE game server
```

Browsers cannot open the raw TCP connection used by RTMPE. The fork implements the missing NetConnection semantics; the native worker performs chunking, acknowledgements, Diffie–Hellman handshake and RTMPE encryption using librtmp. WebSocket opening is not treated as NetConnection success: only the server's AMF status changes `connected`.

WebSocket protocol: first frame is text containing the destination URL, then one binary frame per raw AMF0 command. Worker stdin/stdout uses a four-byte unsigned big-endian length followed by the same bytes. Neither transport adds HTTP AMF-remoting headers. Credentials remain inside AMF payloads (arguments and the original launch URL in `swfUrl`), never process arguments or destination queries. Each WebSocket owns one worker; closing it terminates the worker.

## Build and run

From the parent web directory, use the commands in its README. Runtime builds use Rust 1.97.1, wasm32-unknown-unknown, wasm-bindgen-cli 0.2.127, Node 24 and a JDK. The optional wasm-opt tool improves output size/performance; it was unavailable in this build.

For the bridge alone (macOS/Linux):

```sh
npm ci
npm run build
npm start
npm test
node probe.mjs
```

Build requires git, a C compiler, make, pkg-config, OpenSSL and zlib headers/libraries. On macOS the build resolves Homebrew `openssl@3`. `build.mjs` checks out librtmp from the official rtmpdump repository at `138fdb258d9fc26f1843fd1b891180416c9dc575` into ignored `../.tools/rtmpdump`, then links `bin/rtmp-worker` against that static library and the same OpenSSL ABI.

The pinned librtmp source needs two local OpenSSL fixes, applied reproducibly by the build script: initialize BIGNUM pointers before BN_hex2bn reads them, and set DH private exponent length to `nKeyBits - 1` to fit the safe-prime subgroup accepted by OpenSSL 3. These changes leave the protocol's prime and generator intact. Dependency source and its license remain in `.tools/rtmpdump`; Ruffle's licenses remain at the repository root. No global librtmp installation is modified.

## Scope and boundaries

- Implements the AVM2 AMF0 NetConnection path used by this game: connect arguments, transaction IDs, result/status responders and server callbacks.
- Desktop/other navigator backends do not gain RTMP transport. AMF3 object encoding, NetStream media, SharedObject and RTMPT/RTMPTE tunneling are not implemented by this bridge.
- The loopback server permits only `http://127.0.0.1:5173` and `http://127.0.0.1:4173`, eight connections, 8 MiB messages and bounded queued bytes. It denies URL userinfo, queries, fragments, whitespace and empty app paths.
- Default destination is only `103.116.100.95:80`, as observed in the game's config. To permit a verified game endpoint explicitly, set `VPT_RTMP_TARGETS=host:port,host:port`. Do not expose this local bridge publicly.
- `VPT_RTMP_DIAGNOSTICS=1 npm start` prints command categories, lengths and fixed known status codes. Native library logging is suppressed because it can expose AMF arguments.
- Normal startup disables full AMF capture: no automatic JSONL file and no `/capture` subscription in the web app. `/health` reports `captureEnabled: false`. `VPT_RTMP_CAPTURE` no longer enables it in the CLI. Existing log files remain unchanged.
- Use the web Companion's Record controls to collect a bounded action trace and save it explicitly to `logs/game-actions-*.json`. Metadata updates continue outside recording. HTTP asset requests and RTMPE handshake packets are outside this recording.
- The library's explicit `createBridge({capturePath, captureFull})` option remains for protocol tests. `/capture` is only available on a bridge explicitly created with that option; normal startup rejects that path.
- `probe.mjs` performs a no-account transport probe. An AMF `_error` proves encrypted transport and a response, not successful authentication.

## Verification

- Rust unit tests: raw AMF0 encoding/decoding, preserving nested connect arguments, server callbacks, transport failure, and success/EOF ordering.
- Node tests: destination policy, foreign origin rejection, missing worker, binary fidelity through split native frames, invalid control messages.
- Browser uses the built JS/WASM fork and connects through this bridge. See parent `notes.md` for the observed game result and remaining blockers.

Run the core tests from the Ruffle repository root:

```sh
cargo +1.97.1 test -p ruffle_core --lib net_connection::rtmp::tests --locked
```

## Preserving the launch URL

The web fork exposes the core's existing `with_spoofed_url` setting as `spoofedUrl`. The parent harness can fetch a credential-free asset URL while supplying the original root SWF identity and extracted FlashVars. It does not spoof the page origin or every child SWF URL. Direct mode disables HTTP URL rewrite rules and needs browser/server CORS support; the RTMPE bridge remains required in either mode.

The RTMP `connect` object includes this logical root movie URL as `swfUrl`. A controlled live comparison identified this as the missing field behind `SERVER_NOT_READY`: the same credentials and AMF0 ECMA array were rejected without it and received `NetConnection.Connect.Success` with it. Changing only the array type or trailing slash did not resolve the rejection. The metadata currently identifies the root application; arbitrary nested-SWF connection identity has not been checked against Flash Player.

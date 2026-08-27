# Tempest Moonlight addon

Moonlight / Sunshine game streaming for [Tempest](https://gotempest.app),
shipped as a separate downloadable component rather than built into the app.

## Why this is its own repository

`moonlight-common-c` is GPLv3. Tempest is proprietary. Linking the two into one
binary is not something a proprietary application can do, so the Moonlight
engine runs as a **separate process** that Tempest launches and talks to over a
pipe — a documented protocol between two independent programs, which is the
arrangement the GPL is explicit about permitting.

Everything GPL-licensed lives here, under the GPL, with full source. Nothing in
this repository is linked into Tempest's own binary.

## Not a plugin API

This repository is public because GPL §3 requires that the source be sufficient
to build the binaries we distribute. That is the only reason.

**The protocol in `PROTOCOL.md` is Tempest's internal interface, not a plugin
API.** It has no stability guarantee and changes without notice or deprecation
period; an addon must match the host's `ADDON_ABI` exactly or it is refused at
load. We do not publish an SDK, and we do not accept compatibility bug reports
against the protocol. If you build something against it, expect it to break at
the next ABI bump.

## Building

```sh
git submodule update --init --recursive
cargo build --release        # -> target/release/moonlight
```

Requires a C toolchain and CMake. OpenSSL is **not** a prerequisite — it is
compiled from source by `openssl-src` and handed to `moonlight-common-c`'s
CMake build, so no system OpenSSL is involved on any platform.

Targets: macOS, Windows and Linux, x64 and arm64. There is no mobile or wasm
build; addons are a desktop mechanism.

```sh
cargo test                   # includes a link check against the vendored C core
```

To produce a distributable package:

```sh
./scripts/package.sh         # -> dist/moonlight-<version>-<target>.tpx
```

That `.tpx` is a container — an outer zip holding `payload.zip` — and it comes
out **unsigned**. Signing is a separate step run from the Tempest repository
(`scripts/addon-pki/ci-sign.sh`), which adds `payload.sig` and `payload.ts` to
the same file. Tempest refuses to install a container missing either, so an
unsigned build is only useful for local inspection.

## Layout

```
crates/moonlight-core/     the engine: NVHTTP pairing, RTSP/ENet session, video
crates/moonlight-addon/    the sidecar binary: protocol codec + wiring
vendor/moonlight-common-c/ upstream protocol core (GPLv3, submodule)
PROTOCOL.md                the host interface
```

`crates/moonlight-core/src/transport.rs` and `host.rs` are hand-maintained
copies of the contract Tempest declares on its side. They are duplicated rather
than shared: a shared crate would either have to be published — turning an
internal protocol into a public API — or stay private, which would make this
repository unbuildable from source and defeat the point of releasing it.

## Licence

GPL-3.0-only. See `LICENSE`.

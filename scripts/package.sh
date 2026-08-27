#!/bin/bash
# Build a .tpx package for the current host target.
#
#   ./scripts/package.sh [--debug]
#     -> dist/moonlight-<version>-<target>.tpx
#
# Signing happens in the Tempest host repository (scripts/addon-pki), not here:
# this repository is public and never sees a signing key. CI builds the .tpx,
# then a separate job asks Vault to sign it.

set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
PROFILE_DIR=release
CARGO_ARGS=(--release)
if [ "${1:-}" = "--debug" ]; then
  PROFILE_DIR=debug
  CARGO_ARGS=()
fi

VERSION="$(grep -m1 '^version' "$ROOT/Cargo.toml" | sed -E 's/.*"(.*)".*/\1/')"

# `${platform}-${arch}` in Node's vocabulary, which is what the host's
# descriptor and download URLs use — not Rust's target triple.
case "$(uname -s)" in
  Darwin) PLATFORM=darwin ;;
  Linux)  PLATFORM=linux ;;
  MINGW*|MSYS*|CYGWIN*) PLATFORM=win32 ;;
  *) echo "unsupported platform: $(uname -s)" >&2; exit 1 ;;
esac
case "$(uname -m)" in
  arm64|aarch64) ARCH=arm64 ;;
  x86_64|amd64)  ARCH=x64 ;;
  *) echo "unsupported arch: $(uname -m)" >&2; exit 1 ;;
esac
TARGET="$PLATFORM-$ARCH"

EXE=moonlight
[ "$PLATFORM" = win32 ] && EXE=moonlight.exe

cargo build "${CARGO_ARGS[@]}" -p moonlight-addon --manifest-path "$ROOT/Cargo.toml"
BIN="$ROOT/target/$PROFILE_DIR/$EXE"
[ -f "$BIN" ] || { echo "error: $BIN not built" >&2; exit 1; }

STAGE="$(mktemp -d)"
trap 'rm -rf "$STAGE"' EXIT
mkdir -p "$STAGE/bin"
cp "$BIN" "$STAGE/bin/$EXE"
cp "$ROOT/LICENSE" "$ROOT/README.md" "$STAGE/"

# `abi` must equal the host's ADDON_ABI — see PROTOCOL.md. It is a hand-bumped
# integer, deliberately not the version above: most releases do not touch the
# protocol, and tying the two would force a redownload on every patch.
cat > "$STAGE/descriptor.json" <<JSON
{
  "id": "moonlight",
  "version": "$VERSION",
  "abi": 1,
  "target": "$TARGET",
  "displayName": "Moonlight",
  "license": "GPL-3.0-only",
  "repository": "https://github.com/gotempest/tempest-addon-moonlight",
  "exec": "bin/$EXE",
  "provides": { "remoteDesktop": ["moonlight"] }
}
JSON

mkdir -p "$ROOT/dist"
OUT="$ROOT/dist/moonlight-$VERSION-$TARGET.tpx"
rm -f "$OUT"

# A .tpx is a container, not the package itself:
#
#   moonlight-1.0.0-darwin-arm64.tpx   (outer zip, STORED)
#   ├── payload.zip   the actual package
#   ├── payload.sig   detached CMS over every byte of payload.zip   (added by ci-sign.sh)
#   └── payload.ts    RFC 3161 timestamp over payload.sig           (added by ci-sign.sh)
#
# One file, so the signature cannot be separated from what it signs — the
# offline-install case is someone copying a single file onto a USB stick. And
# because the signature covers the payload *as a whole*, there is no manifest
# of per-entry digests and therefore none of the "entry not listed in the
# manifest is silently unverified" failure mode that JAR-style signing has.
#
# -X: no extra attributes. Resource forks and .DS_Store entries would change
# the archive bytes per build machine, and the digest is what gets signed.
(cd "$STAGE" && zip -qrX payload.zip .)
# -0: the payload is already deflated; compressing it again costs time and
# saves nothing.
(cd "$STAGE" && zip -qX0 "$OUT" payload.zip)

echo "$OUT"
echo "  unsigned — run scripts/addon-pki/ci-sign.sh (tempest-desktop) to add payload.sig + payload.ts"
ls -lh "$OUT" | awk '{print "  size:  " $5}'
shasum -a 256 "$OUT" | awk '{print "  sha256:" $1}'

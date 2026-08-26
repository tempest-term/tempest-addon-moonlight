# Host protocol

What Tempest speaks to this sidecar. **Internal interface, no stability
guarantee** — see the README.

The host's own copy of this document is `docs/addons.md` in the Tempest
repository; the two are kept in step by hand. Where they disagree, the host is
right, because the host is what refuses to load a mismatched addon.

## Transport

The host spawns `bin/moonlight` and speaks over its stdin and stdout.

- **stdout** — framed protocol, binary. Nothing else may ever be written here.
- **stdin** — framed protocol, host → addon.
- **stderr** — plain text log lines, `LEVEL message`, forwarded into the host's
  logger. This is the only channel for diagnostics.

Every message:

```
u32  length   little-endian, counting everything after this field
u8   kind     0x01 CONTROL (UTF-8 JSON) | 0x02 FRAME (binary)
...  payload
```

Only frames are binary. Input, resize and acks stay JSON — a few hundred
messages a second at worst, and readable in a log is worth more than the bytes.

`ADDON_ABI` is currently **1**. The host sends it in `hello`; a mismatch is
fatal on both sides. In practice the host refuses to load an addon whose
descriptor declares the wrong ABI, so the check here is a backstop.

## Control messages

Tagged by `t`, camelCase fields.

### Host → addon

| `t` | Fields |
| --- | --- |
| `hello` | `abi`, `hostVersion` — always first |
| `connect` | `session`, `doc` |
| `input` | `session`, `kind`, … |
| `resize` | `session`, `width`, `height` — *accepted and ignored* |
| `frameAck` | `session`, `frameId` — *accepted and ignored* |
| `close` | `session` |
| `shutdown` | drain and exit 0 |

`resize` and `frameAck` are no-ops here: Moonlight streams at a fixed
negotiated resolution, and its video path never stamps a frame id, so there is
no pacer to feed. They are still part of the contract because the host sends
one message shape to every engine.

`doc` (all fields but `host` optional):

```jsonc
{
  "host": "10.0.0.2",
  "httpPort": 47989,        // NVHTTP plain HTTP, pairing
  "httpsPort": 47984,       // serverinfo / applist / launch
  "width": 1920, "height": 1080, "fps": 60,
  "bitrateKbps": 20000,
  "useHevc": false,
  "clientUniqueId": "", "clientCertPem": "", "clientKeyPem": ""
}
```

All three identity fields empty ⇒ generate a fresh identity and report it back
in an `identity` event. **The host must persist it**: the Sunshine host
remembers the certificate, so losing it un-pairs the client. Clearing them
host-side is exactly how "forget pairing" is implemented.

`input` kinds: `pointer` (`x`,`y`,`buttons`), `wheel` (`x`,`y`,`dx`,`dy`),
`key` (`down`,`code`,`ch?`,`mods{ctrl,alt,shift,meta}`), `clipboard` (`text`).
`code` is a `KeyboardEvent.code` — physical, layout independent. `ch` is a
string because JSON has no char type; only its first scalar is read.

### Addon → host

| `t` | Fields |
| --- | --- |
| `ready` | `addonVersion`, `provides` — reply to `hello` |
| `sessionReady` | `session`, `width`, `height` |
| `sessionClosed` | `session`, `error?` — `error` absent ⇒ clean close |
| `event` | `session`, `kind`, … |

`event` kinds:

- `serviceMessage` — `line`. A user-facing status line.
- `pairPrompt` — `pin`, `url`. Show both. The `/pair` call is already in flight
  and only completes when the user types the PIN at `url`, so this cannot block.
- `pairFinished` — `error?`. Takes the dialog down.
- `identity` — `uniqueId`, `certPem`, `keyPem`. Persist, as above.

## Frame messages

Addon → host only. 26-byte little-endian header, then the payload:

```
u32  session
u32  frameId    always 0 here — Moonlight does not pace
u8   codec      0 raw · 1 jpeg · 2 png · 3 copy · 4 h264 · 5 hevc
u8   flags      bit0 = keyframe
u16  width      full surface
u16  height
u16  x, y, w, h dirty rect — for video, the whole surface
u16  sx, sy     `copy` only; 0 here
```

Moonlight only ever emits `h264` / `hevc` whole-surface access units. The
other codecs exist so this header is identical to the one the host already
uses for RDP and VNC, rather than Moonlight having a second near-identical
format of its own.

## Lifecycle

- One process, many sessions. The host spawns one sidecar per addon.
- Exit 0 on `shutdown` or on a clean stdin EOF (the host quitting without
  saying goodbye is normal). Either way every live session is closed first.
- Exit 1 on a protocol error, with the reason on stderr.
- A crash means every session it owned is gone. The host does not respawn in a
  loop — restart happens on the next user action, with backoff.
- The process inherits a minimal environment: no tokens, no keychain, no cloud
  session. Everything a session needs arrives in `connect`.

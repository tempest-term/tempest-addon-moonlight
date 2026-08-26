//! The wire protocol between the host and this sidecar.
//!
//! Normative description: `PROTOCOL.md`. This file is the Rust half; the host
//! declares the same shapes in TypeScript. They are kept in step by hand — see
//! the note at the top of `moonlight_core::transport` for why there is no
//! shared package.
//!
//! Framing, every message in both directions:
//!
//! ```text
//! u32  length   little-endian, counting everything after this field
//! u8   kind     0x01 CONTROL (UTF-8 JSON) | 0x02 FRAME (binary)
//! ...  payload
//! ```

use std::io::{self, Read, Write};

use serde::{Deserialize, Serialize};

pub const KIND_CONTROL: u8 = 0x01;
pub const KIND_FRAME: u8 = 0x02;

/// Bumped whenever anything in this file changes shape. The host refuses to
/// load an addon whose descriptor does not carry the exact value it expects,
/// so there is no negotiation and no compatibility window — see
/// `docs/addons.md`, "Not a plugin system".
pub const ABI: u32 = 1;

/// A single message is capped so a desynchronised stream fails fast instead of
/// trying to allocate whatever the next four bytes happened to say. Well above
/// any real frame: a 1080p IDR is tens of KB, not tens of MB.
const MAX_FRAME_BYTES: u32 = 32 * 1024 * 1024;

// ── Host → addon ─────────────────────────────────────────────────────────

#[derive(Debug, Deserialize)]
#[serde(tag = "t", rename_all = "camelCase", rename_all_fields = "camelCase")]
pub enum HostMsg {
    Hello {
        abi: u32,
        #[serde(default)]
        host_version: String,
    },
    Connect {
        session: u32,
        doc: ConnectDoc,
    },
    Input {
        session: u32,
        #[serde(flatten)]
        input: InputMsg,
    },
    /// Accepted and ignored — Moonlight streams at a fixed negotiated
    /// resolution. Carried anyway so the host can send one message shape to
    /// every engine; the fields are part of the contract even though this
    /// addon has nothing to do with them.
    #[allow(dead_code)]
    Resize {
        session: u32,
        width: u16,
        height: u16,
    },
    /// Accepted and ignored — Moonlight's video path never stamps a frame id,
    /// so there is no pacer to feed. Same reasoning as `Resize`.
    #[allow(dead_code)]
    FrameAck {
        session: u32,
        frame_id: u32,
    },
    Close {
        session: u32,
    },
    Shutdown,
}

/// Mirrors `MoonlightConnectDoc`. Every field has a default so the host can
/// send only what the user actually configured, and so adding an option here
/// does not break a host that predates it.
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ConnectDoc {
    pub host: String,
    #[serde(default = "d_http_port")]
    pub http_port: u16,
    #[serde(default = "d_https_port")]
    pub https_port: u16,
    #[serde(default = "d_width")]
    pub width: u16,
    #[serde(default = "d_height")]
    pub height: u16,
    #[serde(default = "d_fps")]
    pub fps: u16,
    #[serde(default = "d_bitrate")]
    pub bitrate_kbps: u32,
    #[serde(default)]
    pub use_hevc: bool,
    /// All three empty → generate a fresh identity and report it back through
    /// an `identity` event. Clearing them host-side is how a user re-pairs.
    #[serde(default)]
    pub client_unique_id: String,
    #[serde(default)]
    pub client_cert_pem: String,
    #[serde(default)]
    pub client_key_pem: String,
}

fn d_http_port() -> u16 { 47989 }
fn d_https_port() -> u16 { 47984 }
fn d_width() -> u16 { 1920 }
fn d_height() -> u16 { 1080 }
fn d_fps() -> u16 { 60 }
fn d_bitrate() -> u32 { 20000 }

#[derive(Debug, Deserialize)]
#[serde(tag = "kind", rename_all = "camelCase", rename_all_fields = "camelCase")]
pub enum InputMsg {
    Pointer { x: u16, y: u16, buttons: u8 },
    Wheel { x: u16, y: u16, dx: f32, dy: f32 },
    Key {
        down: bool,
        /// `KeyboardEvent.code` — physical key, layout independent.
        code: String,
        /// Resolved character when printable. A string rather than a char
        /// because JSON has no char type and an empty string is a cleaner
        /// "none" than a sentinel.
        #[serde(default)]
        ch: Option<String>,
        #[serde(default)]
        mods: Mods,
    },
    Clipboard { text: String },
}

#[derive(Debug, Default, Deserialize)]
pub struct Mods {
    #[serde(default)]
    pub ctrl: bool,
    #[serde(default)]
    pub alt: bool,
    #[serde(default)]
    pub shift: bool,
    #[serde(default)]
    pub meta: bool,
}

// ── Addon → host ─────────────────────────────────────────────────────────

#[derive(Debug, Serialize)]
#[serde(tag = "t", rename_all = "camelCase", rename_all_fields = "camelCase")]
pub enum AddonMsg {
    Ready {
        addon_version: String,
        /// Which engines this addon provides, e.g. `["moonlight"]`.
        provides: Vec<String>,
    },
    SessionReady {
        session: u32,
        width: u16,
        height: u16,
    },
    SessionClosed {
        session: u32,
        #[serde(skip_serializing_if = "Option::is_none")]
        error: Option<String>,
    },
    Event {
        session: u32,
        #[serde(flatten)]
        event: EventMsg,
    },
}

#[derive(Debug, Serialize)]
#[serde(tag = "kind", rename_all = "camelCase", rename_all_fields = "camelCase")]
pub enum EventMsg {
    ServiceMessage { line: String },
    PairPrompt { pin: String, url: String },
    PairFinished {
        #[serde(skip_serializing_if = "Option::is_none")]
        error: Option<String>,
    },
    Identity {
        unique_id: String,
        cert_pem: String,
        key_pem: String,
    },
}

// ── Framing ──────────────────────────────────────────────────────────────

/// Read one message. `Ok(None)` means the host closed the pipe cleanly, which
/// is a normal shutdown, not an error.
pub fn read_message(r: &mut impl Read) -> io::Result<Option<(u8, Vec<u8>)>> {
    let mut len_buf = [0u8; 4];
    match r.read_exact(&mut len_buf) {
        Ok(()) => {}
        Err(e) if e.kind() == io::ErrorKind::UnexpectedEof => return Ok(None),
        Err(e) => return Err(e),
    }
    let len = u32::from_le_bytes(len_buf);
    if len < 1 || len > MAX_FRAME_BYTES {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("implausible message length {len} — stream desynchronised"),
        ));
    }
    let mut kind = [0u8; 1];
    r.read_exact(&mut kind)?;
    let mut payload = vec![0u8; (len - 1) as usize];
    r.read_exact(&mut payload)?;
    Ok(Some((kind[0], payload)))
}

/// Serialised access to stdout.
///
/// Frames are produced on the engine thread while control messages come from
/// the reader thread, and a message interleaved with a frame's bytes would
/// desynchronise the host permanently. One lock, held only for the duration of
/// a single `write_all`.
pub struct Writer<W: Write> {
    inner: std::sync::Mutex<W>,
}

impl<W: Write> Writer<W> {
    pub fn new(inner: W) -> Self {
        Self { inner: std::sync::Mutex::new(inner) }
    }

    pub fn control(&self, msg: &AddonMsg) -> io::Result<()> {
        let json = serde_json::to_vec(msg)
            .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
        self.raw(KIND_CONTROL, &json)
    }

    /// One frame: the 26-byte header from `PROTOCOL.md`, then the payload.
    #[allow(clippy::too_many_arguments)]
    pub fn frame(
        &self,
        session: u32,
        frame_id: u32,
        codec: u8,
        key: bool,
        width: u16,
        height: u16,
        rect: (u16, u16, u16, u16),
        src: (u16, u16),
        data: &[u8],
    ) -> io::Result<()> {
        let mut buf = Vec::with_capacity(26 + data.len());
        buf.extend_from_slice(&session.to_le_bytes());
        buf.extend_from_slice(&frame_id.to_le_bytes());
        buf.push(codec);
        buf.push(u8::from(key));
        for v in [width, height, rect.0, rect.1, rect.2, rect.3, src.0, src.1] {
            buf.extend_from_slice(&v.to_le_bytes());
        }
        buf.extend_from_slice(data);
        self.raw(KIND_FRAME, &buf)
    }

    fn raw(&self, kind: u8, payload: &[u8]) -> io::Result<()> {
        let len = (payload.len() as u32)
            .checked_add(1)
            .filter(|n| *n <= MAX_FRAME_BYTES)
            .ok_or_else(|| {
                io::Error::new(io::ErrorKind::InvalidData, "outbound message too large")
            })?;
        let mut out = self
            .inner
            .lock()
            .map_err(|_| io::Error::other("writer mutex poisoned"))?;
        out.write_all(&len.to_le_bytes())?;
        out.write_all(&[kind])?;
        out.write_all(payload)?;
        out.flush()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A message written by `Writer` must be readable by `read_message` —
    /// the two halves of the framing are the one thing that cannot be tested
    /// against the host, so they are tested against each other.
    #[test]
    fn framing_round_trips() {
        let w = Writer::new(Vec::new());
        w.control(&AddonMsg::SessionReady { session: 7, width: 1920, height: 1080 })
            .unwrap();
        w.frame(7, 3, 4, true, 1920, 1080, (0, 0, 1920, 1080), (0, 0), b"nal").unwrap();
        let buf = w.inner.into_inner().unwrap();

        let mut cursor = std::io::Cursor::new(buf);
        let (kind, payload) = read_message(&mut cursor).unwrap().unwrap();
        assert_eq!(kind, KIND_CONTROL);
        assert!(String::from_utf8_lossy(&payload).contains("sessionReady"));

        let (kind, payload) = read_message(&mut cursor).unwrap().unwrap();
        assert_eq!(kind, KIND_FRAME);
        assert_eq!(payload.len(), 26 + 3);
        assert_eq!(&payload[0..4], &7u32.to_le_bytes());
        assert_eq!(payload[8], 4); // h264
        assert_eq!(payload[9], 1); // keyframe
        assert_eq!(&payload[26..], b"nal");

        assert!(read_message(&mut cursor).unwrap().is_none(), "clean EOF");
    }

    #[test]
    fn a_desynchronised_length_fails_instead_of_allocating() {
        let mut bogus = std::io::Cursor::new(u32::MAX.to_le_bytes().to_vec());
        assert!(read_message(&mut bogus).is_err());
    }

    /// Pins the *wire* spelling of every multi-word field.
    ///
    /// The mistake here is subtle: `rename_all` on an enum renames its
    /// variants, not the fields inside them — that needs `rename_all_fields`.
    /// Without it `addonVersion` goes out as `addon_version` and the host
    /// silently reads `undefined`, which is exactly what happened. Single-word
    /// fields hide the bug, so these assertions target the multi-word ones.
    #[test]
    fn outbound_field_names_are_camel_case() {
        let ready = serde_json::to_string(&AddonMsg::Ready {
            addon_version: "1.0.0".into(),
            provides: vec!["moonlight".into()],
        })
        .unwrap();
        assert!(ready.contains(r#""addonVersion":"1.0.0""#), "{ready}");
        assert!(!ready.contains("addon_version"), "{ready}");

        let identity = serde_json::to_string(&AddonMsg::Event {
            session: 1,
            event: EventMsg::Identity {
                unique_id: "abc".into(),
                cert_pem: "cert".into(),
                key_pem: "key".into(),
            },
        })
        .unwrap();
        for expected in [
            r#""kind":"identity""#,
            r#""uniqueId":"abc""#,
            r#""certPem":"cert""#,
            r#""keyPem":"key""#,
        ] {
            assert!(identity.contains(expected), "missing {expected} in {identity}");
        }
    }

    #[test]
    fn host_messages_parse() {
        let m: HostMsg = serde_json::from_str(r#"{"t":"hello","abi":1,"hostVersion":"3.16.0"}"#).unwrap();
        // Assert the value, not just the shape: `#[serde(default)]` turns a
        // name mismatch into an empty string rather than an error, so matching
        // on the variant alone would pass while silently dropping the field.
        match m {
            HostMsg::Hello { abi, ref host_version } => {
                assert_eq!(abi, 1);
                assert_eq!(host_version, "3.16.0");
            }
            _ => panic!("expected hello"),
        }

        let m: HostMsg =
            serde_json::from_str(r#"{"t":"input","session":2,"kind":"pointer","x":10,"y":20,"buttons":1}"#)
                .unwrap();
        assert!(matches!(
            m,
            HostMsg::Input { session: 2, input: InputMsg::Pointer { x: 10, y: 20, buttons: 1 } }
        ));

        // A doc carrying only what the user configured must still parse.
        let m: HostMsg =
            serde_json::from_str(r#"{"t":"frameAck","session":1,"frameId":9}"#).unwrap();
        assert!(matches!(m, HostMsg::FrameAck { frame_id: 9, .. }));

        let m: HostMsg =
            serde_json::from_str(r#"{"t":"connect","session":1,"doc":{"host":"10.0.0.2"}}"#).unwrap();
        match m {
            HostMsg::Connect { doc, .. } => {
                assert_eq!(doc.https_port, 47984);
                assert_eq!(doc.fps, 60);
            }
            _ => panic!("expected connect"),
        }
    }
}

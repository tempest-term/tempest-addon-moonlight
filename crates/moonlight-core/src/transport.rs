//! The frame and input vocabulary shared with the host.
//!
//! This is a **local copy of a contract, not a shared library**. The host
//! declares the same shapes in `tempest_core::transport`, and the two are kept
//! in step by hand. That is deliberate: a shared crate would either have to be
//! published (turning an internal protocol into a public API) or be private
//! (making this GPL repository unbuildable from source, which GPL §3 does not
//! allow). Duplicating ~100 lines of plain data is the cheaper of the three
//! options.
//!
//! See `docs/addons.md` in the host repository, and `PROTOCOL.md` here.

use std::sync::Arc;

/// Pixel / video transport for one [`FrameUpdate`]. Discriminants are the wire
/// values in the FRAME header — do not renumber.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum FrameCodec {
    Raw = 0,
    Jpeg = 1,
    Png = 2,
    Copy = 3,
    H264 = 4,
    Hevc = 5,
}

/// One frame update. Moonlight only ever produces `H264` / `Hevc` whole-surface
/// access units; the rest of the enum exists so the wire header is identical to
/// the one RDP and VNC already use on the host side, rather than Moonlight
/// having a second, nearly-identical format of its own.
#[derive(Debug, Clone)]
pub struct FrameUpdate {
    pub codec: FrameCodec,
    /// Batch id, stamped by the producer and echoed back by the renderer once
    /// painted. Bounds how many frames sit un-painted in the pipe.
    pub frame_id: u32,
    /// Full surface size.
    pub width: u16,
    pub height: u16,
    /// Dirty rect (video: the whole surface).
    pub x: u16,
    pub y: u16,
    pub w: u16,
    pub h: u16,
    /// Keyframe / IDR. A decoder cannot start without one.
    pub key: bool,
    /// `Copy` only: source top-left.
    pub sx: u16,
    pub sy: u16,
    /// `H264`/`Hevc`: one Annex-B access unit. `Raw`: packed RGBA.
    pub data: Vec<u8>,
}

/// Where produced frames go. The sidecar binds this to its stdout writer.
pub type FrameSink = Arc<dyn Fn(Vec<FrameUpdate>) + Send + Sync>;

/// Pointer button bitmask — low 3 bits match RFB / RDP / web button order.
pub mod pointer_button {
    pub const LEFT: u8 = 1;
    pub const MIDDLE: u8 = 2;
    pub const RIGHT: u8 = 4;
}

#[derive(Debug, Clone, Copy, Default)]
pub struct KeyMods {
    pub ctrl: bool,
    pub alt: bool,
    pub shift: bool,
    pub meta: bool,
}

/// An input event from the host. Coordinates are framebuffer pixels, origin
/// top-left.
#[derive(Debug, Clone)]
pub enum RemoteInput {
    Pointer {
        x: u16,
        y: u16,
        buttons: u8,
    },
    Wheel {
        x: u16,
        y: u16,
        dx: f32,
        dy: f32,
    },
    Key {
        down: bool,
        /// `KeyboardEvent.code` — physical key, layout independent.
        code: String,
        ch: Option<char>,
        mods: KeyMods,
    },
    ClipboardText(String),
}

//! Moonlight / Sunshine game-streaming client.
//!
//! Wraps the vendored `moonlight-common-c` (GPLv3) protocol core — RTSP
//! handshake, ENet control channel, RTP video/audio with Reed-Solomon FEC —
//! built as a static library in `build.rs`. Moonlight's video is already
//! H.264/HEVC, so once a session is live the Annex-B NAL units go straight to
//! a [`transport::FrameSink`] with no transcode.
//!
//! Layering:
//!   - [`sys`]      — raw bindgen FFI for `Limelight.h`
//!   - [`identity`] — the persistent RSA client certificate
//!   - [`nvhttp`]   — NVHTTP pairing + app launch (pure Rust, reqwest + rustls)
//!   - [`session`]  — `LiStartConnection` lifecycle and callback bridge
//!   - [`stream`]   — decoder/audio callback shims handed to the C core
//!
//! This crate was extracted from Tempest's `tempest-core` so that GPLv3 code
//! stops being linked into a proprietary binary. It is consumed only by
//! `moonlight-addon`, the sidecar process the host talks to over a pipe.

// bindgen output follows C naming, not Rust conventions.
#![allow(non_upper_case_globals, non_camel_case_types, non_snake_case)]

pub mod host;
pub mod identity;
pub mod nvhttp;
pub mod session;
pub mod stream;
pub mod sys;
pub mod transport;

pub use host::{MoonlightCallbacks, WorkerHandle};
pub use identity::MoonlightIdentity;
pub use session::{MoonlightConnectDoc, MoonlightSession};

use std::ffi::CStr;

/// Structured log line. In-tree this was `tempest_core::tlog!`, which routed to
/// a platform sink; here there is exactly one sink — stderr — because the host
/// reads this process's stderr line by line and forwards it into its own
/// logger. Levels match the host's: `0=debug 1=info 2=warn 3=error`.
///
/// Nothing else may be written to stderr, and *nothing* may be written to
/// stdout, which carries the framed protocol.
#[macro_export]
macro_rules! tlog {
    ($level:expr, $($arg:tt)*) => {{
        // Ignore the write result: Rust's stdio panics on write failure, and a
        // dead stderr (the host exited first) must not take the session with
        // it.
        use std::io::Write as _;
        let _ = writeln!(
            std::io::stderr(),
            "{} {}",
            match $level { 0 => "DEBUG", 1 => "INFO", 2 => "WARN", _ => "ERROR" },
            format_args!($($arg)*)
        );
    }};
}

/// moonlight-common-c's human-readable name for a connection stage
/// (e.g. `"RTSP handshake"`). Also the smoke test that the vendored static
/// library actually linked.
pub fn stage_name(stage: i32) -> String {
    // SAFETY: `LiGetStageName` returns a pointer to a static NUL-terminated
    // string (or a valid string for unknown stages); never null, never freed.
    unsafe {
        let p = sys::LiGetStageName(stage);
        if p.is_null() {
            return String::new();
        }
        CStr::from_ptr(p).to_string_lossy().into_owned()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Proves the vendored moonlight-common-c static lib links and a C entry
    /// point is callable. Upstream returns "none" for STAGE_NONE; any
    /// non-empty string is enough here.
    #[test]
    fn links_and_calls_into_moonlight_common_c() {
        assert!(!stage_name(0).is_empty(), "LiGetStageName returned empty");
    }
}

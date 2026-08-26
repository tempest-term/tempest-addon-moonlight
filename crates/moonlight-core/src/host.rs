//! What the engine needs from whoever is driving it.
//!
//! In-tree this used to be `tempest_core::remote_desktop`'s
//! `RemoteDesktopCallbacks` — a trait shared by RDP, VNC and Moonlight. Out
//! here there is exactly one engine, so this is only the six methods Moonlight
//! actually calls, plus the thread handle. Trimming it is the point: a trait
//! carrying RDP's clipboard and certificate-consent methods would imply this
//! process can do things it cannot.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;

/// Lifecycle and out-of-band events, all fire-and-forget.
///
/// Every one of these becomes a CONTROL message on stdout — see `PROTOCOL.md`.
pub trait MoonlightCallbacks: Send + Sync {
    /// User-facing status line ("Connecting…", "Paired").
    fn on_service_message(&self, _line: String) {}

    /// Handshake done, the stream is live, the first frame is imminent.
    fn on_ready(&self) {}

    /// Session ended, gracefully or not. `reason` is human-readable.
    fn on_closed(&self, _reason: String) {}

    /// Show `pin` and `url`. The `/pair` call is already in flight and only
    /// completes once the user types the PIN at `url`, so this cannot block.
    fn on_pair_prompt(&self, _pin: String, _url: String) {}

    /// Pairing settled — `None` on success. Always follows `on_pair_prompt`
    /// and takes its dialog down.
    fn on_pair_finished(&self, _error: Option<String>) {}

    /// A freshly generated client identity the host MUST persist onto the
    /// saved connection. The Sunshine host remembers this certificate, so
    /// losing it un-pairs the client.
    fn on_identity_learned(&self, _unique_id: String, _cert_pem: String, _key_pem: String) {}
}

/// Owns the engine thread and the flag it polls to exit.
///
/// `stop` never joins: joining from a teardown path that may run on the
/// caller's hot thread is how a UI wedges. The thread observes the flag and
/// unwinds on its own.
#[derive(Default)]
pub struct WorkerHandle {
    stop: Arc<AtomicBool>,
    /// Only ever taken, to release the handle. A `Mutex` rather than
    /// `&mut self` because teardown is `&self`.
    handle: Mutex<Option<JoinHandle<()>>>,
}

impl WorkerHandle {
    /// The flag to hand to the engine thread; it polls this to exit.
    pub fn stop_flag(&self) -> Arc<AtomicBool> {
        Arc::clone(&self.stop)
    }

    /// Adopt the spawned thread.
    pub fn set(&self, handle: JoinHandle<()>) {
        if let Ok(mut slot) = self.handle.lock() {
            *slot = Some(handle);
        }
    }

    /// Ask the thread to exit and drop its handle. Idempotent; never blocks.
    pub fn stop(&self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Ok(mut slot) = self.handle.lock() {
            slot.take();
        }
    }
}

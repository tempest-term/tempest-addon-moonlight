//! `MoonlightSession` — the Moonlight engine behind the shared
//! [`MoonlightCallbacks`] contract the sidecar implements.
//!
//! A worker thread runs the whole flow: restore the persistent client identity
//! from the connect doc (or generate one and report it back through
//! [`MoonlightCallbacks::on_identity_learned`] so the caller can store it),
//! (re)pair if needed (raising the PIN through
//! [`MoonlightCallbacks::on_pair_prompt`] for the user to type into
//! Sunshine's web UI), `/launch` the Desktop app, then `LiStartConnection` and
//! pump each decoded H.264/HEVC [`stream::VideoUnit`] into the [`FrameSink`] as
//! a `FrameUpdate` — the same canvas/WebCodecs path RDP/VNC feed.
//!
//! Input is not wired yet (display-first); `send_input` is a no-op.

use std::os::raw::{c_char, c_int, c_short};
use std::sync::atomic::{AtomicBool, AtomicU8, Ordering};
use std::sync::mpsc::{self, RecvTimeoutError};
use std::sync::Arc;
use std::time::Duration;

use crate::host::{MoonlightCallbacks, WorkerHandle};
use crate::tlog;
use crate::transport::{FrameCodec, FrameSink, FrameUpdate, RemoteInput};

use crate::stream::{self, VideoCodec};
use crate::identity::MoonlightIdentity;
use crate::{nvhttp, sys};

/// Sunshine's web UI, where the user types the pairing PIN.
const SUNSHINE_WEB_UI_PORT: u16 = 47990;

/// Everything `MoonlightSession::connect_with_doc` needs.
#[derive(Debug, Clone)]
pub struct MoonlightConnectDoc {
    pub host: String,
    /// NVHTTP plain-HTTP port (default 47989) — pairing phases 1-4.
    pub http_port: u16,
    /// NVHTTP HTTPS port (default 47984) — serverinfo/applist/launch.
    pub https_port: u16,
    pub width: u16,
    pub height: u16,
    pub fps: u16,
    pub bitrate_kbps: u32,
    pub use_hevc: bool,
    /// Persisted client identity (see [`MoonlightIdentity`]). All three empty
    /// → generate a fresh one and hand it to the caller through
    /// [`MoonlightCallbacks::on_identity_learned`] to store on the doc.
    /// Clearing them on the doc is how the user forces a re-pair.
    pub client_unique_id: String,
    pub client_cert_pem: String,
    pub client_key_pem: String,
}

impl MoonlightConnectDoc {
    pub fn new(host: impl Into<String>) -> Self {
        Self {
            host: host.into(),
            http_port: 47989,
            https_port: 47984,
            width: 1920,
            height: 1080,
            fps: 60,
            bitrate_kbps: 20000,
            use_hevc: false,
            client_unique_id: String::new(),
            client_cert_pem: String::new(),
            client_key_pem: String::new(),
        }
    }
}

pub struct MoonlightSession {
    worker: WorkerHandle,
    /// Stream dims — the reference frame for absolute mouse positioning.
    ref_width: u16,
    ref_height: u16,
    /// Last pointer button bitmask, to emit press/release on change.
    last_buttons: AtomicU8,
}

impl MoonlightSession {
    pub fn connect_with_doc(
        doc: MoonlightConnectDoc,
        callbacks: Arc<dyn MoonlightCallbacks>,
        frame_sink: FrameSink,
    ) -> MoonlightSession {
        let (ref_width, ref_height) = (doc.width, doc.height);
        let worker = WorkerHandle::default();
        let stop_thread = worker.stop_flag();

        let spawned = std::thread::Builder::new()
            .name("moonlight-session".to_owned())
            .spawn(move || {
                let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                    run_moonlight(&doc, &callbacks, &frame_sink, &stop_thread)
                }));
                let reason = match outcome {
                    Ok(Ok(())) => "session ended".to_owned(),
                    Ok(Err(e)) => {
                        tlog!(3, "[moonlight] {e}");
                        e
                    }
                    Err(panic) => {
                        let msg = if let Some(s) = panic.downcast_ref::<&str>() {
                            (*s).to_owned()
                        } else if let Some(s) = panic.downcast_ref::<String>() {
                            s.clone()
                        } else {
                            "unknown panic".to_owned()
                        };
                        tlog!(3, "[moonlight] session thread panicked: {msg}");
                        format!("internal error: {msg}")
                    }
                };
                callbacks.on_closed(reason);
            })
            .ok();
        if let Some(h) = spawned { worker.set(h); }

        MoonlightSession {
            worker,
            ref_width,
            ref_height,
            last_buttons: AtomicU8::new(0),
        }
    }
}

impl MoonlightSession {
    pub fn send_input(&self, input: RemoteInput) {
        // moonlight's LiSend* are no-ops until the connection is live, so it's
        // safe to call them unconditionally.
        match input {
            RemoteInput::Pointer { x, y, buttons } => unsafe {
                sys::LiSendMousePositionEvent(
                    x as c_short,
                    y as c_short,
                    self.ref_width as c_short,
                    self.ref_height as c_short,
                );
                // RemotePointerButton bitmask (shared with RDP/VNC): 1=left, 2=middle, 4=right.
                let prev = self.last_buttons.swap(buttons, Ordering::Relaxed);
                for (mask, button) in [
                    (1u8, sys::BUTTON_LEFT),
                    (2, sys::BUTTON_MIDDLE),
                    (4, sys::BUTTON_RIGHT),
                ] {
                    let was = prev & mask != 0;
                    let now = buttons & mask != 0;
                    if now != was {
                        let action = if now {
                            sys::BUTTON_ACTION_PRESS
                        } else {
                            sys::BUTTON_ACTION_RELEASE
                        };
                        sys::LiSendMouseButtonEvent(action as c_char, button as c_int);
                    }
                }
            },
            RemoteInput::Wheel { dy, .. } => {
                // Browser deltaY > 0 = scroll down; moonlight positive = up.
                let amount = (-dy).clamp(-32767.0, 32767.0) as c_short;
                if amount != 0 {
                    unsafe { sys::LiSendHighResScrollEvent(amount) };
                }
            }
            RemoteInput::Key { down, code, .. } => {
                if let Some(vk) = vk_from_code(&code) {
                    let action = if down {
                        sys::KEY_ACTION_DOWN
                    } else {
                        sys::KEY_ACTION_UP
                    };
                    // Modifiers ride in as their own key events (VK 0x10/0x11/…),
                    // so the host tracks state itself — pass 0 here.
                    unsafe { sys::LiSendKeyboardEvent(vk, action as c_char, 0) };
                }
            }
            // Moonlight has no clipboard channel in the GameStream protocol.
            RemoteInput::ClipboardText(_) => {}
        }
    }

    pub fn close(&self) {
        stream::stop_stream();
        self.worker.stop();
    }
}

impl Drop for MoonlightSession {
    fn drop(&mut self) {
        self.close();
    }
}

fn run_moonlight(
    doc: &MoonlightConnectDoc,
    callbacks: &Arc<dyn MoonlightCallbacks>,
    frame_sink: &FrameSink,
    stop: &AtomicBool,
) -> Result<(), String> {
    let id = match restore_identity(doc) {
        Some(id) => id,
        None => {
            let id = MoonlightIdentity::generate()?;
            tlog!(1, "[moonlight] generated a new client identity — pairing required");
            callbacks.on_identity_learned(
                id.unique_id.clone(),
                String::from_utf8_lossy(&id.cert_pem).into_owned(),
                String::from_utf8_lossy(&id.key_pem).into_owned(),
            );
            id
        }
    };
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(|e| format!("tokio runtime: {e}"))?;

    callbacks.on_service_message(format!("Connecting to {} …", doc.host));

    // HTTPS serverinfo gives version + the real pair status; fall back to plain
    // HTTP (pair_status reads 0 there) if the TLS call fails.
    let si = rt
        .block_on(nvhttp::server_info_https(&id, &doc.host, doc.https_port))
        .or_else(|_| rt.block_on(nvhttp::server_info(&doc.host, doc.http_port, &id.unique_id)))?;

    if si.pair_status == 0 {
        let pin = random_pin();
        let url = format!("https://{}:{SUNSHINE_WEB_UI_PORT}", doc.host);
        callbacks.on_service_message(format!("Pairing — open {url} and enter PIN {pin}"));
        // Raise the dialog *before* pairing: `/pair` blocks until the PIN is
        // typed into the host's web UI, so the user needs to see it meanwhile.
        callbacks.on_pair_prompt(pin.clone(), url);
        let salt = nvhttp::random_salt();
        let key = nvhttp::derive_aes_key(&salt, &pin);
        let client = nvhttp::PairingClient::new(&id, doc.host.clone(), doc.http_port);
        match rt.block_on(client.pair(&salt, &key)) {
            Ok(()) => {
                callbacks.on_pair_finished(None);
                callbacks.on_service_message("Paired".to_owned());
            }
            Err(e) => {
                callbacks.on_pair_finished(Some(e.clone()));
                return Err(e);
            }
        }
    }
    if stop.load(Ordering::Relaxed) {
        return Ok(());
    }

    let apps = rt.block_on(nvhttp::app_list(&id, &doc.host, doc.https_port))?;
    let app = apps
        .iter()
        .find(|a| a.title.eq_ignore_ascii_case("Desktop"))
        .or_else(|| apps.first())
        .ok_or("host exposes no launchable apps")?
        .clone();
    callbacks.on_service_message(format!("Launching {} …", app.title));

    let launch = rt.block_on(nvhttp::launch(
        &id,
        &doc.host,
        doc.https_port,
        app.id,
        doc.width as u32,
        doc.height as u32,
        doc.fps as u32,
    ))?;

    let (tx, rx) = mpsc::channel();
    let opts = stream::StreamOptions {
        host: doc.host.clone(),
        app_version: si.app_version,
        gfe_version: si.gfe_version,
        codec_mode_support: si.server_codec_mode_support as i32,
        width: doc.width as i32,
        height: doc.height as i32,
        fps: doc.fps as i32,
        bitrate_kbps: doc.bitrate_kbps as i32,
        use_hevc: doc.use_hevc,
        aes_key: launch.aes_key,
        aes_iv: launch.aes_iv(),
    };
    // `/launch` just allocated a video session on the host; from here on, any
    // exit *we* initiate (a failed handshake, or the user disconnecting) must
    // release it with `/cancel`, or the host stays "busy" and the next
    // connect attempt comes back ALREADY_RUNNING. Not called when the host
    // itself ended the session (`is_terminated`) — it's already gone there.
    let release_host = || {
        if let Err(e) = rt.block_on(nvhttp::cancel(&id, &doc.host, doc.https_port)) {
            tlog!(1, "[moonlight] cancel: {e}");
        }
    };

    // Blocks through the RTSP/control handshake; returns once the stream is live.
    if let Err(e) = stream::start_stream(opts, tx) {
        release_host();
        return Err(e);
    }
    callbacks.on_ready();
    callbacks.on_service_message("Connected".to_owned());

    let (w, h) = (doc.width, doc.height);
    loop {
        if stop.load(Ordering::Relaxed) {
            break;
        }
        if stream::is_terminated() {
            stream::stop_stream();
            return Err("host terminated the session".to_owned());
        }
        match rx.recv_timeout(Duration::from_millis(100)) {
            Ok(unit) => {
                let codec = match unit.codec {
                    VideoCodec::H264 => FrameCodec::H264,
                    VideoCodec::Hevc => FrameCodec::Hevc,
                };
                frame_sink(vec![FrameUpdate {
                    codec,
                    frame_id: 0, // video path drops in the decoder, not via acks
                    width: w,
                    height: h,
                    x: 0,
                    y: 0,
                    w,
                    h,
                    key: unit.key_frame,
                    sx: 0,
                    sy: 0,
                    data: unit.data,
                }]);
            }
            Err(RecvTimeoutError::Timeout) => {}
            Err(RecvTimeoutError::Disconnected) => break,
        }
    }
    stream::stop_stream();
    release_host();
    Ok(())
}

/// Rebuild the persisted identity from the connect doc. `None` when the doc
/// carries no (complete) identity — the caller then generates one and reports
/// it back for storage. Note this is per-saved-host, mirroring how RDP/VNC keep
/// `fingerprint_v2` on their own doc.
fn restore_identity(doc: &MoonlightConnectDoc) -> Option<MoonlightIdentity> {
    if doc.client_unique_id.is_empty()
        || doc.client_cert_pem.is_empty()
        || doc.client_key_pem.is_empty()
    {
        return None;
    }
    Some(MoonlightIdentity::from_pem(
        doc.client_unique_id.clone(),
        doc.client_cert_pem.as_bytes().to_vec(),
        doc.client_key_pem.as_bytes().to_vec(),
    ))
}

/// Map a browser `KeyboardEvent.code` to a Windows virtual-key code (what
/// `LiSendKeyboardEvent` expects). Covers letters/digits/common keys; unmapped
/// keys are dropped.
fn vk_from_code(code: &str) -> Option<c_short> {
    // KeyA..KeyZ → 'A'..'Z' (VK == ASCII uppercase).
    if let Some(rest) = code.strip_prefix("Key") {
        let b = rest.as_bytes();
        if b.len() == 1 && b[0].is_ascii_uppercase() {
            return Some(b[0] as c_short);
        }
    }
    // Digit0..Digit9 → '0'..'9'.
    if let Some(rest) = code.strip_prefix("Digit") {
        let b = rest.as_bytes();
        if b.len() == 1 && b[0].is_ascii_digit() {
            return Some(b[0] as c_short);
        }
    }
    let vk: u16 = match code {
        "Enter" | "NumpadEnter" => 0x0D,
        "Space" => 0x20,
        "Backspace" => 0x08,
        "Tab" => 0x09,
        "Escape" => 0x1B,
        "ArrowLeft" => 0x25,
        "ArrowUp" => 0x26,
        "ArrowRight" => 0x27,
        "ArrowDown" => 0x28,
        "Delete" => 0x2E,
        "Insert" => 0x2D,
        "Home" => 0x24,
        "End" => 0x23,
        "PageUp" => 0x21,
        "PageDown" => 0x22,
        "CapsLock" => 0x14,
        "ShiftLeft" | "ShiftRight" => 0x10,
        "ControlLeft" | "ControlRight" => 0x11,
        "AltLeft" | "AltRight" => 0x12,
        "MetaLeft" | "MetaRight" => 0x5B,
        "Minus" => 0xBD,
        "Equal" => 0xBB,
        "BracketLeft" => 0xDB,
        "BracketRight" => 0xDD,
        "Backslash" => 0xDC,
        "Semicolon" => 0xBA,
        "Quote" => 0xDE,
        "Backquote" => 0xC0,
        "Comma" => 0xBC,
        "Period" => 0xBE,
        "Slash" => 0xBF,
        "F1" => 0x70,
        "F2" => 0x71,
        "F3" => 0x72,
        "F4" => 0x73,
        "F5" => 0x74,
        "F6" => 0x75,
        "F7" => 0x76,
        "F8" => 0x77,
        "F9" => 0x78,
        "F10" => 0x79,
        "F11" => 0x7A,
        "F12" => 0x7B,
        _ => return None,
    };
    Some(vk as c_short)
}

fn random_pin() -> String {
    let mut b = [0u8; 2];
    let _ = openssl::rand::rand_bytes(&mut b);
    format!("{:04}", u16::from_be_bytes(b) % 10000)
}

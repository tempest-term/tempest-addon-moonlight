//! The Moonlight addon sidecar.
//!
//! Reads framed control messages on stdin, writes control messages and video
//! frames on stdout, logs to stderr. It holds no credentials and inherits no
//! host state: everything a session needs arrives in its `connect` message.
//!
//! One process may own several sessions — the host spawns one sidecar per
//! addon, not per connection.

mod proto;

use std::collections::HashMap;
use std::io::{self, Read};
use std::sync::{Arc, Mutex};

use moonlight_core::session::{MoonlightConnectDoc, MoonlightSession};
use moonlight_core::transport::{FrameUpdate, KeyMods, RemoteInput};
use moonlight_core::{tlog, MoonlightCallbacks};

use proto::{AddonMsg, EventMsg, HostMsg, InputMsg, Writer};

type Out = Writer<io::Stdout>;

fn main() {
    // Exit code is the only thing the host can read once stdout is framed, so
    // keep the mapping simple: 0 clean, 1 anything else.
    match run() {
        Ok(()) => {}
        Err(e) => {
            tlog!(3, "[addon] {e}");
            std::process::exit(1);
        }
    }
}

fn run() -> Result<(), String> {
    let writer: Arc<Out> = Arc::new(Writer::new(io::stdout()));
    let sessions: Arc<Mutex<HashMap<u32, Arc<MoonlightSession>>>> = Arc::default();
    let mut stdin = io::stdin().lock();

    // The host always speaks first. Anything else means we are not talking to
    // a Tempest host at all, so fail rather than guess.
    match read_control(&mut stdin)? {
        Some(HostMsg::Hello { abi, host_version }) => {
            if abi != proto::ABI {
                return Err(format!(
                    "ABI mismatch: host speaks {abi}, this addon speaks {}. \
                     The host is expected to refuse to load us before we get here.",
                    proto::ABI
                ));
            }
            tlog!(1, "[addon] hello from host {host_version}, abi {abi}");
        }
        Some(other) => return Err(format!("expected hello, got {other:?}")),
        None => return Ok(()),
    }

    send(
        &writer,
        &AddonMsg::Ready {
            addon_version: env!("CARGO_PKG_VERSION").to_owned(),
            provides: vec!["moonlight".to_owned()],
        },
    );

    while let Some(msg) = read_control(&mut stdin)? {
        match msg {
            HostMsg::Hello { .. } => tlog!(2, "[addon] duplicate hello ignored"),

            HostMsg::Connect { session, doc } => {
                let engine = MoonlightSession::connect_with_doc(
                    to_core_doc(doc),
                    Arc::new(Callbacks { session, writer: Arc::clone(&writer) }),
                    frame_sink(session, Arc::clone(&writer)),
                );
                sessions.lock().map_err(poisoned)?.insert(session, Arc::new(engine));
            }

            HostMsg::Input { session, input } => {
                if let Some(engine) = lookup(&sessions, session)? {
                    engine.send_input(to_core_input(input));
                }
            }

            // Moonlight streams at a fixed negotiated resolution — the C core
            // has no mid-session renegotiation — and its video path never
            // acknowledges frames (`frame_id` is always 0), so there is no
            // pacer to feed. Both are accepted and dropped rather than
            // rejected: the host sends them uniformly to every engine, and an
            // error here would be noise, not information.
            HostMsg::Resize { .. } | HostMsg::FrameAck { .. } => {}

            HostMsg::Close { session } => {
                if let Some(engine) = sessions.lock().map_err(poisoned)?.remove(&session) {
                    engine.close();
                }
            }

            HostMsg::Shutdown => break,
        }
    }

    // Reached on `shutdown` and on a clean EOF — the host exiting without
    // saying goodbye is normal (it was killed, the user quit). Either way the
    // engine threads must be told, or this process lingers holding a stream.
    for (_, engine) in sessions.lock().map_err(poisoned)?.drain() {
        engine.close();
    }
    Ok(())
}

fn read_control(r: &mut impl Read) -> Result<Option<HostMsg>, String> {
    loop {
        let Some((kind, payload)) = proto::read_message(r).map_err(|e| e.to_string())? else {
            return Ok(None);
        };
        if kind != proto::KIND_CONTROL {
            // The host never sends frames. Skip rather than abort: a future
            // host may add a kind we predate, and dropping it is survivable
            // where killing the session is not.
            tlog!(2, "[addon] ignoring unexpected message kind {kind:#x}");
            continue;
        }
        return serde_json::from_slice(&payload)
            .map(Some)
            .map_err(|e| format!("undecodable control message: {e}"));
    }
}

/// A dead stdout means the host is gone. Log it, never panic: unwinding here
/// would abort mid-stream and lose the chance to close sessions cleanly.
fn send(writer: &Out, msg: &AddonMsg) {
    if let Err(e) = writer.control(msg) {
        tlog!(3, "[addon] could not write to host: {e}");
    }
}

fn lookup(
    sessions: &Mutex<HashMap<u32, Arc<MoonlightSession>>>,
    session: u32,
) -> Result<Option<Arc<MoonlightSession>>, String> {
    // Clone the Arc out and release the lock before touching the engine —
    // holding a shared map locked across an engine call is how a stall in one
    // session becomes a stall in all of them.
    Ok(sessions.lock().map_err(poisoned)?.get(&session).cloned())
}

fn poisoned<T>(_: T) -> String {
    "session map mutex poisoned".to_owned()
}

fn frame_sink(session: u32, writer: Arc<Out>) -> moonlight_core::transport::FrameSink {
    Arc::new(move |batch: Vec<FrameUpdate>| {
        for u in batch {
            let FrameUpdate { codec, frame_id, width, height, x, y, w, h, key, sx, sy, data } = u;
            if let Err(e) = writer.frame(
                session,
                frame_id,
                codec as u8,
                key,
                width,
                height,
                (x, y, w, h),
                (sx, sy),
                &data,
            ) {
                // One log line per failed frame would flood a broken pipe with
                // thousands of lines a second; the reader loop notices the
                // same EOF and shuts down in order.
                tlog!(0, "[addon] dropped frame for session {session}: {e}");
                return;
            }
        }
    })
}

struct Callbacks {
    session: u32,
    writer: Arc<Out>,
}

impl Callbacks {
    fn event(&self, event: EventMsg) {
        send(&self.writer, &AddonMsg::Event { session: self.session, event });
    }
}

impl MoonlightCallbacks for Callbacks {
    fn on_service_message(&self, line: String) {
        self.event(EventMsg::ServiceMessage { line });
    }

    fn on_ready(&self) {
        // Moonlight negotiates the stream size from the connect doc rather
        // than discovering it, so the host already knows it; reporting it
        // again keeps `sessionReady` identical in shape to what RDP and VNC
        // send, where the size genuinely is discovered.
        send(
            &self.writer,
            &AddonMsg::SessionReady { session: self.session, width: 0, height: 0 },
        );
    }

    fn on_closed(&self, reason: String) {
        send(
            &self.writer,
            &AddonMsg::SessionClosed {
                session: self.session,
                error: (reason != "session ended").then_some(reason),
            },
        );
    }

    fn on_pair_prompt(&self, pin: String, url: String) {
        self.event(EventMsg::PairPrompt { pin, url });
    }

    fn on_pair_finished(&self, error: Option<String>) {
        self.event(EventMsg::PairFinished { error });
    }

    fn on_identity_learned(&self, unique_id: String, cert_pem: String, key_pem: String) {
        self.event(EventMsg::Identity { unique_id, cert_pem, key_pem });
    }
}

fn to_core_doc(d: proto::ConnectDoc) -> MoonlightConnectDoc {
    MoonlightConnectDoc {
        host: d.host,
        http_port: d.http_port,
        https_port: d.https_port,
        width: d.width,
        height: d.height,
        fps: d.fps,
        bitrate_kbps: d.bitrate_kbps,
        use_hevc: d.use_hevc,
        client_unique_id: d.client_unique_id,
        client_cert_pem: d.client_cert_pem,
        client_key_pem: d.client_key_pem,
    }
}

fn to_core_input(input: InputMsg) -> RemoteInput {
    match input {
        InputMsg::Pointer { x, y, buttons } => RemoteInput::Pointer { x, y, buttons },
        InputMsg::Wheel { x, y, dx, dy } => RemoteInput::Wheel { x, y, dx, dy },
        InputMsg::Key { down, code, ch, mods } => RemoteInput::Key {
            down,
            code,
            // JSON has no char type, so the host sends a string. Take the
            // first scalar: anything longer is not a keystroke.
            ch: ch.and_then(|s| s.chars().next()),
            mods: KeyMods {
                ctrl: mods.ctrl,
                alt: mods.alt,
                shift: mods.shift,
                meta: mods.meta,
            },
        },
        InputMsg::Clipboard { text } => RemoteInput::ClipboardText(text),
    }
}

/// Keeps the codec discriminants honest: `FrameCodec as u8` is written
/// straight into the wire header, so a reordering of the enum would silently
/// change what the host decodes.
#[cfg(test)]
mod tests {
    use moonlight_core::transport::FrameCodec;

    #[test]
    fn frame_codec_discriminants_match_the_wire() {
        assert_eq!(FrameCodec::Raw as u8, 0);
        assert_eq!(FrameCodec::Jpeg as u8, 1);
        assert_eq!(FrameCodec::Png as u8, 2);
        assert_eq!(FrameCodec::Copy as u8, 3);
        assert_eq!(FrameCodec::H264 as u8, 4);
        assert_eq!(FrameCodec::Hevc as u8, 5);
    }
}

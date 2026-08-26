//! Moonlight streaming bridge — `LiStartConnection` + the C callback glue.
//!
//! After [`super::nvhttp::launch`] starts a session on the host, this drives
//! moonlight-common-c's RTSP/control/RTP machinery via `LiStartConnection`,
//! populating `SERVER_INFORMATION` + `STREAM_CONFIGURATION` and registering the
//! decoder/audio/connection callbacks. The video decode callback reassembles
//! each `DECODE_UNIT`'s NAL list and forwards it as a [`VideoUnit`]; the caller
//! (napi → FrameSink, or the dev example) renders/decodes the H.264/HEVC.
//!
//! moonlight's `DecoderRendererSubmitDecodeUnit` takes **no context pointer**,
//! so the forward target is a process-global sink installed for the lifetime of
//! the connection. Only one moonlight session may be active at a time (which
//! matches moonlight-common-c's own global-state design).

use std::ffi::CString;
use std::os::raw::{c_char, c_int, c_void};
use std::sync::atomic::{AtomicBool, AtomicI32, Ordering};
use std::sync::mpsc::Sender;
use std::sync::Mutex;

use crate::sys;
use crate::tlog;

/// Which elementary stream the host negotiated.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum VideoCodec {
    H264,
    Hevc,
}

/// One reassembled video access unit (Annex-B NAL bytes) from the host.
#[derive(Debug, Clone)]
pub struct VideoUnit {
    pub codec: VideoCodec,
    pub key_frame: bool,
    pub data: Vec<u8>,
}

/// Everything `start_stream` needs. The crypto comes from `/launch`
/// ([`super::nvhttp::LaunchSession`]); the version strings + codec support come
/// from `/serverinfo` ([`super::nvhttp::ServerInfo`]).
pub struct StreamOptions {
    pub host: String,
    pub app_version: String,
    pub gfe_version: String,
    pub codec_mode_support: i32,
    pub width: i32,
    pub height: i32,
    pub fps: i32,
    pub bitrate_kbps: i32,
    pub use_hevc: bool,
    pub aes_key: [u8; 16],
    pub aes_iv: [u8; 16],
}

// submitDecodeUnit has no context arg → route NALs through a global sink.
static VIDEO_SINK: Mutex<Option<Sender<VideoUnit>>> = Mutex::new(None);
static NEGOTIATED_FORMAT: AtomicI32 = AtomicI32::new(0);
static TERMINATED: AtomicBool = AtomicBool::new(false);

/// Start a session (blocks through the RTSP/control handshake, returns once the
/// connection is live). Video then flows to `sink` on moonlight's threads until
/// [`stop_stream`]. Returns the moonlight error code on failure (the
/// `stageFailed` log line says which stage).
pub fn start_stream(opts: StreamOptions, sink: Sender<VideoUnit>) -> Result<(), String> {
    *VIDEO_SINK.lock().unwrap() = Some(sink);
    TERMINATED.store(false, Ordering::SeqCst);

    // SERVER_INFORMATION holds borrowed pointers — keep the CStrings alive
    // until LiStartConnection (synchronous) returns.
    let c_addr = CString::new(opts.host.as_str()).map_err(|_| "host has NUL")?;
    let c_appver = CString::new(opts.app_version.as_str()).map_err(|_| "appver has NUL")?;
    let c_gfever = CString::new(opts.gfe_version.as_str()).map_err(|_| "gfever has NUL")?;

    let mut si: sys::SERVER_INFORMATION = unsafe { std::mem::zeroed() };
    si.address = c_addr.as_ptr();
    si.serverInfoAppVersion = c_appver.as_ptr();
    si.serverInfoGfeVersion = c_gfever.as_ptr();
    si.rtspSessionUrl = std::ptr::null(); // moonlight builds rtsp://host:48010
    si.serverCodecModeSupport = opts.codec_mode_support;

    let mut cfg: sys::STREAM_CONFIGURATION = unsafe { std::mem::zeroed() };
    unsafe { sys::LiInitializeStreamConfiguration(&mut cfg) };
    cfg.width = opts.width;
    cfg.height = opts.height;
    cfg.fps = opts.fps;
    cfg.bitrate = opts.bitrate_kbps;
    cfg.packetSize = 1392;
    cfg.streamingRemotely = sys::STREAM_CFG_AUTO as c_int;
    // AUDIO_CONFIGURATION_STEREO = MAKE_AUDIO_CONFIGURATION(2, 0x3).
    cfg.audioConfiguration = (0x3 << 16) | (2 << 8) | 0xCA;
    cfg.supportedVideoFormats = if opts.use_hevc {
        sys::VIDEO_FORMAT_H265 as c_int
    } else {
        sys::VIDEO_FORMAT_H264 as c_int
    };
    cfg.clientRefreshRateX100 = opts.fps * 100;
    cfg.colorSpace = sys::COLORSPACE_REC_709 as c_int;
    cfg.colorRange = 0; // limited range
    cfg.encryptionFlags = sys::ENCFLG_AUDIO as c_int;
    for i in 0..16 {
        cfg.remoteInputAesKey[i] = opts.aes_key[i] as c_char;
        cfg.remoteInputAesIv[i] = opts.aes_iv[i] as c_char;
    }

    let mut dr: sys::DECODER_RENDERER_CALLBACKS = unsafe { std::mem::zeroed() };
    unsafe { sys::LiInitializeVideoCallbacks(&mut dr) };
    dr.setup = Some(dr_setup);
    dr.start = Some(dr_start);
    dr.stop = Some(dr_stop);
    dr.cleanup = Some(dr_cleanup);
    dr.submitDecodeUnit = Some(dr_submit);
    dr.capabilities = 0; // we reassemble whole frames; no DIRECT_SUBMIT

    let mut ar: sys::AUDIO_RENDERER_CALLBACKS = unsafe { std::mem::zeroed() };
    unsafe { sys::LiInitializeAudioCallbacks(&mut ar) };
    ar.init = Some(ar_init);
    ar.start = Some(ar_start);
    ar.stop = Some(ar_stop);
    ar.cleanup = Some(ar_cleanup);
    ar.decodeAndPlaySample = Some(ar_play);

    let mut cl: sys::CONNECTION_LISTENER_CALLBACKS = unsafe { std::mem::zeroed() };
    unsafe { sys::LiInitializeConnectionCallbacks(&mut cl) };
    cl.stageStarting = Some(cl_stage_starting);
    cl.stageComplete = Some(cl_stage_complete);
    cl.stageFailed = Some(cl_stage_failed);
    cl.connectionStarted = Some(cl_connection_started);
    cl.connectionTerminated = Some(cl_connection_terminated);

    let rc = unsafe {
        sys::LiStartConnection(
            &mut si,
            &mut cfg,
            &mut cl,
            &mut dr,
            &mut ar,
            std::ptr::null_mut(),
            0,
            std::ptr::null_mut(),
            0,
        )
    };
    drop((c_addr, c_appver, c_gfever)); // outlived LiStartConnection

    if rc != 0 {
        *VIDEO_SINK.lock().unwrap() = None;
        return Err(format!("LiStartConnection failed (code {rc})"));
    }
    Ok(())
}

/// Tear down the active session and drop the global video sink.
pub fn stop_stream() {
    unsafe { sys::LiStopConnection() };
    *VIDEO_SINK.lock().unwrap() = None;
}

/// `true` once the host has terminated the connection.
pub fn is_terminated() -> bool {
    TERMINATED.load(Ordering::SeqCst)
}

// ── C callbacks (run on moonlight's threads) ─────────────────────────────────

unsafe extern "C" fn dr_setup(
    video_format: c_int,
    w: c_int,
    h: c_int,
    _redraw_rate: c_int,
    _ctx: *mut c_void,
    _flags: c_int,
) -> c_int {
    NEGOTIATED_FORMAT.store(video_format, Ordering::SeqCst);
    tlog!(1, "[moonlight] video setup: format=0x{video_format:x} {w}x{h}");
    sys::DR_OK as c_int
}

unsafe extern "C" fn dr_start() {
    tlog!(1, "[moonlight] video start");
}
unsafe extern "C" fn dr_stop() {
    tlog!(1, "[moonlight] video stop");
}
unsafe extern "C" fn dr_cleanup() {
    tlog!(1, "[moonlight] video cleanup");
}

unsafe extern "C" fn dr_submit(du: sys::PDECODE_UNIT) -> c_int {
    if du.is_null() {
        return sys::DR_OK as c_int;
    }
    let unit = &*du;

    // Concat the LENTRY NAL chain into one Annex-B buffer.
    let mut data: Vec<u8> = Vec::with_capacity(unit.fullLength.max(0) as usize);
    let mut entry_ptr = unit.bufferList;
    while !entry_ptr.is_null() {
        let entry = &*entry_ptr;
        if !entry.data.is_null() && entry.length > 0 {
            data.extend_from_slice(std::slice::from_raw_parts(
                entry.data as *const u8,
                entry.length as usize,
            ));
        }
        entry_ptr = entry.next;
    }

    let codec = if NEGOTIATED_FORMAT.load(Ordering::SeqCst) & (sys::VIDEO_FORMAT_H265 as c_int) != 0 {
        VideoCodec::Hevc
    } else {
        VideoCodec::H264
    };
    let key_frame = unit.frameType == sys::FRAME_TYPE_IDR as c_int;

    if let Some(tx) = VIDEO_SINK.lock().unwrap().as_ref() {
        let _ = tx.send(VideoUnit {
            codec,
            key_frame,
            data,
        });
    }
    sys::DR_OK as c_int
}

// Audio: no-op renderer (discard samples). Moonlight always streams audio; we
// don't play it here (the desktop/remote-desktop surface is video-only for now).
unsafe extern "C" fn ar_init(
    _cfg: c_int,
    _opus: sys::POPUS_MULTISTREAM_CONFIGURATION,
    _ctx: *mut c_void,
    _flags: c_int,
) -> c_int {
    0
}
unsafe extern "C" fn ar_start() {}
unsafe extern "C" fn ar_stop() {}
unsafe extern "C" fn ar_cleanup() {}
unsafe extern "C" fn ar_play(_data: *mut c_char, _len: c_int) {}

unsafe extern "C" fn cl_stage_starting(stage: c_int) {
    tlog!(1, "[moonlight] stage starting: {}", super::stage_name(stage));
}
unsafe extern "C" fn cl_stage_complete(stage: c_int) {
    tlog!(1, "[moonlight] stage complete: {}", super::stage_name(stage));
}
unsafe extern "C" fn cl_stage_failed(stage: c_int, error_code: c_int) {
    tlog!(
        3,
        "[moonlight] stage FAILED: {} (error {error_code})",
        super::stage_name(stage)
    );
}
unsafe extern "C" fn cl_connection_started() {
    tlog!(1, "[moonlight] connection started");
}
unsafe extern "C" fn cl_connection_terminated(error_code: c_int) {
    TERMINATED.store(true, Ordering::SeqCst);
    tlog!(2, "[moonlight] connection terminated (error {error_code})");
}

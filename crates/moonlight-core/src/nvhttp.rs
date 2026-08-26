//! NVHTTP — the GameStream/Sunshine HTTP control protocol.
//!
//! This is the half of Moonlight that lives *outside* moonlight-common-c: the
//! pre-stream HTTP(S) dance that a client app implements itself (moonlight-qt's
//! `NvHTTP` / `NvPairingManager`; libgamestream's `client.c`). We implement it
//! in pure Rust.
//!
//!   - [`server_info`] — `GET /serverinfo`: host name, version, pair status,
//!     codec support, current game. Plain HTTP (port 47989) when unpaired.
//!   - [`PairingClient::pair`] — the PIN-based pairing handshake. The host's
//!     AES key is derived from `SHA-256(salt || PIN)`; the user types the PIN
//!     into Sunshine's web UI while phase 1 blocks. Four HTTP phases negotiate
//!     and mutually verify a shared secret; a fifth confirms over HTTPS.
//!
//! Crypto (GameStream gen 7+ = Sunshine): SHA-256 (32-byte hashes), AES-128-ECB
//! **without padding** (data is block-aligned), RSA-2048 sign/verify with the
//! certs from [`super::MoonlightIdentity`].

use openssl::hash::MessageDigest;
use openssl::sign::{Signer, Verifier};
use openssl::symm::{Cipher, Crypter, Mode};
use openssl::x509::X509;

use crate::identity::MoonlightIdentity;

/// Parsed `/serverinfo` response (the fields we care about).
#[derive(Debug, Clone)]
pub struct ServerInfo {
    pub hostname: String,
    pub app_version: String,
    pub gfe_version: String,
    /// `0` = not paired with us, `1` = paired.
    pub pair_status: u8,
    /// App id currently running, `0` = idle.
    pub current_game: u32,
    pub state: String,
    pub https_port: u16,
    pub server_codec_mode_support: u32,
}

/// `GET /serverinfo` over plain HTTP (works unpaired). `host` is an IP/hostname.
pub async fn server_info(host: &str, http_port: u16, unique_id: &str) -> Result<ServerInfo, String> {
    // All param values are URL-safe (hex / uuid / ascii), so no escaping needed.
    let url = format!(
        "http://{host}:{http_port}/serverinfo?uniqueid={unique_id}&uuid={}",
        new_uuid()
    );
    let body = reqwest::Client::new()
        .get(&url)
        .send()
        .await
        .map_err(|e| format!("serverinfo request failed: {e}"))?
        .text()
        .await
        .map_err(|e| format!("serverinfo read failed: {e}"))?;

    if !body.contains("status_code=\"200\"") {
        return Err(format!("serverinfo non-200: {body}"));
    }
    Ok(ServerInfo {
        hostname: xml_tag(&body, "hostname").unwrap_or_default().to_string(),
        app_version: xml_tag(&body, "appversion").unwrap_or_default().to_string(),
        gfe_version: xml_tag(&body, "GfeVersion").unwrap_or_default().to_string(),
        pair_status: xml_tag(&body, "PairStatus").and_then(|s| s.parse().ok()).unwrap_or(0),
        current_game: xml_tag(&body, "currentgame").and_then(|s| s.parse().ok()).unwrap_or(0),
        state: xml_tag(&body, "state").unwrap_or_default().to_string(),
        https_port: xml_tag(&body, "HttpsPort").and_then(|s| s.parse().ok()).unwrap_or(47984),
        server_codec_mode_support: xml_tag(&body, "ServerCodecModeSupport")
            .and_then(|s| s.parse().ok())
            .unwrap_or(0),
    })
}

/// One installed app on the host (`/applist` entry). `id` is what `/launch`
/// takes; Sunshine always exposes a "Desktop" app for full-desktop streaming.
#[derive(Debug, Clone)]
pub struct HostApp {
    pub id: u32,
    pub title: String,
}

/// Build an HTTPS reqwest client that presents our pinned client certificate
/// and accepts the host's self-signed server cert. Required for every paired
/// call — `/serverinfo` (to read the real PairStatus), `/applist`, `/launch`.
fn https_client(identity: &MoonlightIdentity) -> Result<reqwest::Client, String> {
    // The rustls backend takes a single PEM bundle (cert chain + private key).
    let mut bundle = identity.cert_pem.clone();
    bundle.push(b'\n');
    bundle.extend_from_slice(&identity.key_pem);
    let id = reqwest::Identity::from_pem(&bundle)
        .map_err(|e| format!("client identity: {e}"))?;
    reqwest::Client::builder()
        .danger_accept_invalid_certs(true) // Sunshine's server cert is self-signed
        .identity(id)
        .build()
        .map_err(|e| format!("https client build: {e}"))
}

/// `GET /applist` over HTTPS (paired). Lists the host's launchable apps.
pub async fn app_list(
    identity: &MoonlightIdentity,
    host: &str,
    https_port: u16,
) -> Result<Vec<HostApp>, String> {
    let url = format!(
        "https://{host}:{https_port}/applist?uniqueid={}&uuid={}",
        identity.unique_id,
        new_uuid()
    );
    let body = https_client(identity)?
        .get(&url)
        .send()
        .await
        .map_err(|e| format!("applist request failed: {e}"))?
        .text()
        .await
        .map_err(|e| format!("applist read failed: {e}"))?;
    if !body.contains("status_code=\"200\"") {
        return Err(format!("applist non-200: {body}"));
    }
    // Each app is an <App>...<AppTitle>..</AppTitle>..<ID>..</ID>..</App> block.
    Ok(body
        .split("<App>")
        .skip(1)
        .filter_map(|chunk| {
            let title = xml_tag(chunk, "AppTitle")?.to_string();
            let id = xml_tag(chunk, "ID")?.trim().parse().ok()?;
            Some(HostApp { id, title })
        })
        .collect())
}

/// `GET /serverinfo` over HTTPS with the client cert — reports the *real*
/// `PairStatus` for us (plain-HTTP serverinfo always reports 0).
pub async fn server_info_https(
    identity: &MoonlightIdentity,
    host: &str,
    https_port: u16,
) -> Result<ServerInfo, String> {
    let url = format!(
        "https://{host}:{https_port}/serverinfo?uniqueid={}&uuid={}",
        identity.unique_id,
        new_uuid()
    );
    let body = https_client(identity)?
        .get(&url)
        .send()
        .await
        .map_err(|e| format!("serverinfo(https) request failed: {e}"))?
        .text()
        .await
        .map_err(|e| format!("serverinfo(https) read failed: {e}"))?;
    if !body.contains("status_code=\"200\"") {
        return Err(format!("serverinfo(https) non-200: {body}"));
    }
    Ok(ServerInfo {
        hostname: xml_tag(&body, "hostname").unwrap_or_default().to_string(),
        app_version: xml_tag(&body, "appversion").unwrap_or_default().to_string(),
        gfe_version: xml_tag(&body, "GfeVersion").unwrap_or_default().to_string(),
        pair_status: xml_tag(&body, "PairStatus").and_then(|s| s.parse().ok()).unwrap_or(0),
        current_game: xml_tag(&body, "currentgame").and_then(|s| s.parse().ok()).unwrap_or(0),
        state: xml_tag(&body, "state").unwrap_or_default().to_string(),
        https_port: xml_tag(&body, "HttpsPort").and_then(|s| s.parse().ok()).unwrap_or(https_port),
        server_codec_mode_support: xml_tag(&body, "ServerCodecModeSupport")
            .and_then(|s| s.parse().ok())
            .unwrap_or(0),
    })
}

/// The result of `/launch` — the per-session stream crypto that both `/launch`
/// (sent as `rikey`/`rikeyid`) and `LiStartConnection` (as
/// `STREAM_CONFIGURATION.remoteInputAesKey`/`Iv`) need to agree on.
#[derive(Debug, Clone)]
pub struct LaunchSession {
    /// 16-byte AES key for the control/input channel (the `rikey`).
    pub aes_key: [u8; 16],
    /// The `rikeyid`; its big-endian bytes seed the AES IV.
    pub aes_key_id: i32,
}

impl LaunchSession {
    /// `remoteInputAesIv` = the rikeyid as a big-endian int in the first 4
    /// bytes of a 16-byte buffer (matches moonlight-qt).
    pub fn aes_iv(&self) -> [u8; 16] {
        let mut iv = [0u8; 16];
        iv[0..4].copy_from_slice(&self.aes_key_id.to_be_bytes());
        iv
    }
}

/// `GET /launch` over HTTPS — starts streaming `app_id` on the host at the
/// requested mode and returns the session crypto. Sunshine's "Desktop" app id
/// gives a full-desktop stream. NOTE: this allocates a video session on the
/// host; it must be followed by `LiStartConnection` (or `/cancel`), else the
/// host stays "busy".
pub async fn launch(
    identity: &MoonlightIdentity,
    host: &str,
    https_port: u16,
    app_id: u32,
    width: u32,
    height: u32,
    fps: u32,
) -> Result<LaunchSession, String> {
    let aes_key = rand16()?;
    // rikeyid is a random positive 31-bit int.
    let mut idb = [0u8; 4];
    openssl::rand::rand_bytes(&mut idb).map_err(|e| e.to_string())?;
    let aes_key_id = (i32::from_be_bytes(idb) & 0x7fff_ffff).max(1);

    let url = format!(
        "https://{host}:{https_port}/launch?uniqueid={uid}&uuid={uuid}\
         &appid={app_id}&mode={width}x{height}x{fps}&additionalStates=1&sops=0\
         &rikey={rikey}&rikeyid={aes_key_id}&localAudioPlayMode=0\
         &surroundAudioInfo=196610&remoteControllersBitmap=0&gcmap=0&hdrMode=0",
        uid = identity.unique_id,
        uuid = new_uuid(),
        rikey = to_hex(&aes_key),
    );
    let body = https_client(identity)?
        .get(&url)
        .send()
        .await
        .map_err(|e| format!("launch request failed: {e}"))?
        .text()
        .await
        .map_err(|e| format!("launch read failed: {e}"))?;
    if !body.contains("status_code=\"200\"") {
        return Err(format!("launch non-200: {body}"));
    }
    // gamesession (fresh launch) or resume (already running) — either is OK.
    let ok = xml_tag(&body, "gamesession").map(|s| s.trim() != "0").unwrap_or(false)
        || xml_tag(&body, "resume").map(|s| s.trim() != "0").unwrap_or(false);
    if !ok {
        return Err(format!("launch did not start a session: {body}"));
    }
    Ok(LaunchSession {
        aes_key,
        aes_key_id,
    })
}

/// `GET /cancel` over HTTPS — releases the video session `/launch` allocated
/// on the host without ever starting `LiStartConnection`. Call this whenever
/// a connect attempt fails *after* a successful `/launch` but before the
/// stream comes up; otherwise the host stays "busy" and the next `/launch`
/// comes back `status_code="419"` (ALREADY_RUNNING) even though nothing is
/// actually streaming.
pub async fn cancel(identity: &MoonlightIdentity, host: &str, https_port: u16) -> Result<(), String> {
    let url = format!(
        "https://{host}:{https_port}/cancel?uniqueid={}&uuid={}",
        identity.unique_id,
        new_uuid()
    );
    let body = https_client(identity)?
        .get(&url)
        .send()
        .await
        .map_err(|e| format!("cancel request failed: {e}"))?
        .text()
        .await
        .map_err(|e| format!("cancel read failed: {e}"))?;
    if !body.contains("status_code=\"200\"") {
        return Err(format!("cancel non-200: {body}"));
    }
    Ok(())
}

/// Drives the GameStream PIN pairing handshake against one host.
pub struct PairingClient<'a> {
    identity: &'a MoonlightIdentity,
    host: String,
    http_port: u16,
    http: reqwest::Client,
}

impl<'a> PairingClient<'a> {
    pub fn new(identity: &'a MoonlightIdentity, host: impl Into<String>, http_port: u16) -> Self {
        Self {
            identity,
            host: host.into(),
            http_port,
            http: reqwest::Client::new(),
        }
    }

    /// One `GET /pair?...` HTTP phase. Returns the raw XML body (status checked).
    async fn pair_req(&self, params: &[(&str, &str)]) -> Result<String, String> {
        let uuid = new_uuid();
        let mut q: Vec<(&str, &str)> = vec![
            ("uniqueid", self.identity.unique_id.as_str()),
            ("uuid", uuid.as_str()),
        ];
        q.extend_from_slice(params);
        // All param values are URL-safe (hex / ascii), so no escaping needed.
        let qs = q
            .iter()
            .map(|(k, v)| format!("{k}={v}"))
            .collect::<Vec<_>>()
            .join("&");
        let url = format!("http://{}:{}/pair?{qs}", self.host, self.http_port);
        let body = self
            .http
            .get(&url)
            .send()
            .await
            .map_err(|e| format!("pair request failed: {e}"))?
            .text()
            .await
            .map_err(|e| format!("pair read failed: {e}"))?;
        if !body.contains("status_code=\"200\"") {
            return Err(format!("pair phase non-200: {body}"));
        }
        Ok(body)
    }

    /// Run the full PIN handshake. The caller has already shown `pin` to the
    /// user; phase 1 blocks server-side until they enter it in Sunshine's UI.
    /// `aes_key` is the 16-byte AES-128 key derived from the salt + PIN.
    pub async fn pair(&self, salt: &[u8; 16], aes_key: &[u8; 16]) -> Result<(), String> {
        // ── Phase 1: getservercert (blocks until the user enters the PIN) ────
        let body = self
            .pair_req(&[
                ("devicename", "tempest"),
                ("updateState", "1"),
                ("phrase", "getservercert"),
                ("salt", &to_hex(salt)),
                ("clientcert", &self.identity.cert_hex()),
            ])
            .await?;
        if xml_tag(&body, "paired").map(|s| s.trim()) != Some("1") {
            return Err("phase 1: host declined pairing (paired != 1)".into());
        }
        let server_cert_pem = from_hex(
            xml_tag(&body, "plaincert").ok_or("phase 1: no plaincert")?,
        )?;
        let server_cert =
            X509::from_pem(&server_cert_pem).map_err(|e| format!("bad server cert: {e}"))?;
        let server_cert_sig = server_cert.signature().as_slice().to_vec();

        // ── Phase 2: clientchallenge ─────────────────────────────────────────
        let client_challenge = rand16()?;
        let enc_challenge = aes_ecb(Mode::Encrypt, aes_key, &client_challenge)?;
        let body = self
            .pair_req(&[
                ("clientchallenge", &to_hex(&enc_challenge)),
            ])
            .await?;
        let challenge_resp = aes_ecb(
            Mode::Decrypt,
            aes_key,
            &from_hex(xml_tag(&body, "challengeresponse").ok_or("phase 2: no challengeresponse")?)?,
        )?;
        if challenge_resp.len() < 48 {
            return Err("phase 2: short challenge response".into());
        }
        let server_response = &challenge_resp[0..32]; // SHA-256 digest
        let server_challenge = &challenge_resp[32..48]; // 16 bytes

        // ── Phase 3: serverchallengeresp ─────────────────────────────────────
        // The response we generate uses OUR (client) cert signature — the
        // server recomputes it with the client cert it stored in phase 1.
        // (Verifying the *server's* response in phase 2 used the server cert
        // sig; the convention is "responder hashes in its own cert signature".)
        let client_cert_sig = self
            .identity
            .certificate()?
            .signature()
            .as_slice()
            .to_vec();
        let client_secret = rand16()?;
        let mut to_hash = Vec::new();
        to_hash.extend_from_slice(server_challenge);
        to_hash.extend_from_slice(&client_cert_sig);
        to_hash.extend_from_slice(&client_secret);
        let challenge_resp_hash = sha256(&to_hash);
        let enc = aes_ecb(Mode::Encrypt, aes_key, &challenge_resp_hash)?;
        let body = self
            .pair_req(&[("serverchallengeresp", &to_hex(&enc))])
            .await?;
        let pairing_secret =
            from_hex(xml_tag(&body, "pairingsecret").ok_or("phase 3: no pairingsecret")?)?;
        if pairing_secret.len() < 16 + 256 {
            return Err("phase 3: short pairing secret".into());
        }
        let server_secret = &pairing_secret[0..16];
        let server_signature = &pairing_secret[16..16 + 256];

        // Verify the host knew the PIN: its response hash must match what we
        // expect from our challenge + its cert signature + its secret.
        let mut expect = Vec::new();
        expect.extend_from_slice(&client_challenge);
        expect.extend_from_slice(&server_cert_sig);
        expect.extend_from_slice(server_secret);
        if sha256(&expect) != server_response {
            return Err("pairing failed: wrong PIN (server response mismatch)".into());
        }
        // Verify the host owns its certificate (signed its secret).
        let server_pub = server_cert
            .public_key()
            .map_err(|e| format!("server pubkey: {e}"))?;
        let mut v = Verifier::new(MessageDigest::sha256(), &server_pub)
            .map_err(|e| format!("verifier: {e}"))?;
        v.update(server_secret).map_err(|e| e.to_string())?;
        if !v.verify(server_signature).map_err(|e| e.to_string())? {
            return Err("pairing failed: bad server signature".into());
        }

        // ── Phase 4: clientpairingsecret = clientSecret || sign(clientSecret) ─
        let pkey = self.identity.private_key()?;
        let mut signer =
            Signer::new(MessageDigest::sha256(), &pkey).map_err(|e| format!("signer: {e}"))?;
        signer.update(&client_secret).map_err(|e| e.to_string())?;
        let client_sig = signer.sign_to_vec().map_err(|e| e.to_string())?;
        let mut client_pairing_secret = client_secret.to_vec();
        client_pairing_secret.extend_from_slice(&client_sig);
        let body = self
            .pair_req(&[("clientpairingsecret", &to_hex(&client_pairing_secret))])
            .await?;
        if xml_tag(&body, "paired").map(|s| s.trim()) != Some("1") {
            return Err("phase 4: host rejected client pairing secret".into());
        }
        Ok(())
    }
}

/// Derive the 16-byte AES-128 key from a random salt + the user's PIN
/// (GameStream gen 7+: first 16 bytes of `SHA-256(salt || pin_ascii)`).
pub fn derive_aes_key(salt: &[u8; 16], pin: &str) -> [u8; 16] {
    let mut buf = salt.to_vec();
    buf.extend_from_slice(pin.as_bytes());
    let digest = sha256(&buf);
    let mut key = [0u8; 16];
    key.copy_from_slice(&digest[0..16]);
    key
}

// ── crypto + wire helpers ────────────────────────────────────────────────────

fn sha256(data: &[u8]) -> [u8; 32] {
    openssl::sha::sha256(data)
}

/// AES-128-ECB with **no padding** (GameStream encrypts block-aligned data).
fn aes_ecb(mode: Mode, key: &[u8], data: &[u8]) -> Result<Vec<u8>, String> {
    let cipher = Cipher::aes_128_ecb();
    let mut c = Crypter::new(cipher, mode, key, None).map_err(|e| format!("aes init: {e}"))?;
    c.pad(false);
    let mut out = vec![0u8; data.len() + cipher.block_size()];
    let n = c.update(data, &mut out).map_err(|e| format!("aes update: {e}"))?;
    let m = c.finalize(&mut out[n..]).map_err(|e| format!("aes final: {e}"))?;
    out.truncate(n + m);
    Ok(out)
}

fn rand16() -> Result<[u8; 16], String> {
    let mut b = [0u8; 16];
    openssl::rand::rand_bytes(&mut b).map_err(|e| format!("rand: {e}"))?;
    Ok(b)
}

/// A fresh 16-byte pairing salt (mixed with the PIN to derive the AES key).
pub fn random_salt() -> [u8; 16] {
    let mut b = [0u8; 16];
    let _ = openssl::rand::rand_bytes(&mut b);
    b
}

fn to_hex(b: &[u8]) -> String {
    b.iter().map(|x| format!("{x:02X}")).collect()
}

fn from_hex(s: &str) -> Result<Vec<u8>, String> {
    let s = s.trim();
    if s.len() % 2 != 0 {
        return Err("odd-length hex".into());
    }
    (0..s.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(&s[i..i + 2], 16).map_err(|e| format!("bad hex: {e}")))
        .collect()
}

fn xml_tag<'a>(xml: &'a str, tag: &str) -> Option<&'a str> {
    let open = format!("<{tag}>");
    let close = format!("</{tag}>");
    let start = xml.find(&open)? + open.len();
    let end = xml[start..].find(&close)? + start;
    Some(&xml[start..end])
}

/// A throwaway random UUID (v4-ish) for the `uuid` query param.
fn new_uuid() -> String {
    let mut b = [0u8; 16];
    let _ = openssl::rand::rand_bytes(&mut b);
    b[6] = (b[6] & 0x0f) | 0x40;
    b[8] = (b[8] & 0x3f) | 0x80;
    let h = to_hex(&b).to_lowercase();
    format!(
        "{}-{}-{}-{}-{}",
        &h[0..8],
        &h[8..12],
        &h[12..16],
        &h[16..20],
        &h[20..32]
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn aes_ecb_round_trips_without_padding() {
        let key = [7u8; 16];
        let data = [9u8; 32];
        let enc = aes_ecb(Mode::Encrypt, &key, &data).unwrap();
        assert_eq!(enc.len(), 32, "no padding block added");
        let dec = aes_ecb(Mode::Decrypt, &key, &enc).unwrap();
        assert_eq!(dec, data);
    }

    #[test]
    fn aes_key_derivation_is_16_bytes() {
        let key = derive_aes_key(&[0u8; 16], "0000");
        assert_eq!(key.len(), 16);
    }

    #[test]
    fn hex_round_trips() {
        let b = [0xDE, 0xAD, 0xBE, 0xEF];
        assert_eq!(to_hex(&b), "DEADBEEF");
        assert_eq!(from_hex("DEADBEEF").unwrap(), b);
    }
}

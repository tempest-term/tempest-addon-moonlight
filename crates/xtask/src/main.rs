//! `cargo xtask package` — build the sidecar and write the `.tpx`.
//!
//! In Rust rather than a shell script because this has to run on all five
//! targets and the Windows runner has neither `bash` nor `zip`. The obvious
//! alternative — a `.ps1` beside the `.sh` — would put the descriptor fields
//! and the container layout in two places that must never disagree, and the
//! app rejects a package that gets either wrong. One implementation, in the
//! language every runner already has because it is what we are building.
//!
//! Signing is not here and never will be: this repository is public and holds
//! no key. CI builds the `.tpx`, then a separate job asks Vault to sign it
//! (`scripts/addon-pki/ci-sign.sh` in tempest-desktop).

use std::env;
use std::fs;
use std::io::{Cursor, Read, Write};
use std::path::{Path, PathBuf};
use std::process::Command;

use zip::write::SimpleFileOptions;
use zip::{CompressionMethod, ZipWriter};

type Err = Box<dyn std::error::Error>;

fn main() -> Result<(), Err> {
    let task = env::args().nth(1).unwrap_or_default();
    if task != "package" {
        eprintln!("usage: cargo xtask package [--debug]");
        std::process::exit(2);
    }
    package(env::args().any(|a| a == "--debug"))
}

fn package(debug: bool) -> Result<(), Err> {
    let root = workspace_root();
    let version = env!("CARGO_PKG_VERSION"); // shared through workspace.package
    let profile_dir = if debug { "debug" } else { "release" };

    // `${platform}-${arch}` in Node's vocabulary — what the host's descriptor
    // and download URLs use, not Rust's target triple.
    //
    // Cross-compiling is the norm, not the exception: there is no arm64 runner
    // anywhere, so every arm64 artifact is built on an x64 host. TPX_TARGET
    // names what is being built when that differs from what is building it;
    // without it the artifact would be named after the wrong machine and the
    // app would refuse it as `wrong-target`.
    let target = match env::var("TPX_TARGET") {
        Ok(t) if !t.is_empty() => t,
        _ => format!("{}-{}", host_platform()?, host_arch()?),
    };
    let platform = target
        .split_once('-')
        .ok_or_else(|| format!("TPX_TARGET must look like 'linux-arm64', got '{target}'"))?
        .0
        .to_string();

    let exe = if platform == "win32" { "moonlight.exe" } else { "moonlight" };

    let mut cargo = Command::new(env::var("CARGO").unwrap_or_else(|_| "cargo".into()));
    cargo.arg("build").arg("-p").arg("moonlight-addon");
    if !debug {
        cargo.arg("--release");
    }
    let cargo_target = env::var("CARGO_TARGET").ok().filter(|t| !t.is_empty());
    if let Some(t) = &cargo_target {
        cargo.arg("--target").arg(t);
    }
    cargo.current_dir(&root);
    let status = cargo.status()?;
    if !status.success() {
        return Err("cargo build failed".into());
    }

    // Where cargo actually writes. CARGO_TARGET_DIR is not a nicety on
    // Windows: CI moves it to a short path because vendored OpenSSL's .obj
    // paths overflow MSVC's MAX_PATH under the default target/ in-workspace.
    // A cross build lands under <target-dir>/<triple>/, not <target-dir>/<profile>/.
    let target_dir = env::var("CARGO_TARGET_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|_| root.join("target"));
    let bin = match &cargo_target {
        Some(t) => target_dir.join(t).join(profile_dir).join(exe),
        None => target_dir.join(profile_dir).join(exe),
    };
    if !bin.is_file() {
        return Err(format!("{} not built", bin.display()).into());
    }

    // `abi` must equal the host's ADDON_ABI — see PROTOCOL.md. A hand-bumped
    // integer, deliberately not the version above: most releases do not touch
    // the protocol, and tying the two would force a redownload on every patch.
    // CI overrides it so the number lives in one place there, and its sign job
    // asserts that place against the host's own source before signing.
    //
    // `maxSessions: 1` because moonlight-common-c keeps its connection in C
    // file-scope globals — one stream per process. The host starts a second
    // process for a second session rather than refusing it.
    let abi = env::var("ADDON_ABI").unwrap_or_else(|_| "1".into());
    let descriptor = format!(
        r#"{{
  "id": "moonlight",
  "version": "{version}",
  "abi": {abi},
  "target": "{target}",
  "displayName": "Moonlight",
  "license": "GPL-3.0-only",
  "repository": "https://github.com/tempest-term/tempest-addon-moonlight",
  "exec": "bin/{exe}",
  "provides": {{ "remoteDesktop": ["moonlight"] }},
  "maxSessions": 1
}}
"#
    );

    // The payload: deflated, because it is mostly a native binary.
    let mut payload = ZipWriter::new(Cursor::new(Vec::new()));
    let deflated = SimpleFileOptions::default().compression_method(CompressionMethod::Deflated);
    // Executable bit: the host runs `exec` straight out of the unpacked tree,
    // and a zip that loses the mode makes it fail with EACCES at spawn.
    let exec_mode = deflated.unix_permissions(0o755);

    payload.start_file(format!("bin/{exe}"), exec_mode)?;
    payload.write_all(&fs::read(&bin)?)?;
    payload.start_file("descriptor.json", deflated)?;
    payload.write_all(descriptor.as_bytes())?;
    for name in ["LICENSE", "README.md"] {
        payload.start_file(name, deflated)?;
        payload.write_all(&fs::read(root.join(name))?)?;
    }
    let payload = payload.finish()?.into_inner();

    // A .tpx is a container, not the package itself:
    //
    //   moonlight-1.0.0-darwin-arm64.tpx   (outer zip, STORED)
    //   ├── payload.zip   the actual package
    //   ├── payload.sig   detached CMS over every byte of payload.zip
    //   └── payload.ts    RFC 3161 timestamp over payload.sig
    //
    // One file, so the signature cannot be separated from what it signs — the
    // offline-install case is someone copying a single file onto a USB stick.
    // And because the signature covers the payload *as a whole*, there is no
    // manifest of per-entry digests and therefore none of the "entry not listed
    // in the manifest is silently unverified" failure mode JAR signing has.
    //
    // STORED: the payload is already deflated, so compressing it again costs
    // time and saves nothing.
    let dist = root.join("dist");
    fs::create_dir_all(&dist)?;
    let out = dist.join(format!("moonlight-{version}-{target}.tpx"));
    let _ = fs::remove_file(&out);

    let mut container = ZipWriter::new(fs::File::create(&out)?);
    container.start_file(
        "payload.zip",
        SimpleFileOptions::default().compression_method(CompressionMethod::Stored),
    )?;
    container.write_all(&payload)?;
    container.finish()?;

    let bytes = fs::metadata(&out)?.len();
    println!("{}", out.display());
    println!("  unsigned — run scripts/addon-pki/ci-sign.sh (tempest-desktop) to add payload.sig + payload.ts");
    println!("  size:   {} bytes", bytes);
    println!("  sha256: {}", sha256_hex(&out)?);
    Ok(())
}

/// The workspace root: this crate lives at `<root>/crates/xtask`.
fn workspace_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .ancestors()
        .nth(2)
        .expect("crates/xtask is two levels below the workspace root")
        .to_path_buf()
}

fn host_platform() -> Result<&'static str, Err> {
    Ok(match env::consts::OS {
        "macos" => "darwin",
        "linux" => "linux",
        "windows" => "win32",
        other => return Err(format!("unsupported platform: {other}").into()),
    })
}

fn host_arch() -> Result<&'static str, Err> {
    Ok(match env::consts::ARCH {
        "aarch64" => "arm64",
        "x86_64" => "x64",
        other => return Err(format!("unsupported arch: {other}").into()),
    })
}

/// Reported so a human can match the artifact against what CI signed, and
/// against the digest pinned in the host's `ADDON_CATALOG`. Written out rather
/// than shelling to `sha256sum`/`shasum`, which is the portability problem
/// this whole file exists to avoid.
fn sha256_hex(path: &Path) -> Result<String, Err> {
    let mut f = fs::File::open(path)?;
    let mut h = Sha256::new();
    let mut buf = [0u8; 64 * 1024];
    loop {
        let n = f.read(&mut buf)?;
        if n == 0 {
            break;
        }
        h.update(&buf[..n]);
    }
    Ok(h.finish().iter().map(|b| format!("{b:02x}")).collect())
}

/// FIPS 180-4 SHA-256. Small enough to write out, and it keeps this crate's
/// dependency list to the one thing it cannot write itself (zip).
struct Sha256 {
    state: [u32; 8],
    buf: [u8; 64],
    buffered: usize,
    len: u64,
}

const K: [u32; 64] = [
    0x428a2f98, 0x71374491, 0xb5c0fbcf, 0xe9b5dba5, 0x3956c25b, 0x59f111f1, 0x923f82a4, 0xab1c5ed5,
    0xd807aa98, 0x12835b01, 0x243185be, 0x550c7dc3, 0x72be5d74, 0x80deb1fe, 0x9bdc06a7, 0xc19bf174,
    0xe49b69c1, 0xefbe4786, 0x0fc19dc6, 0x240ca1cc, 0x2de92c6f, 0x4a7484aa, 0x5cb0a9dc, 0x76f988da,
    0x983e5152, 0xa831c66d, 0xb00327c8, 0xbf597fc7, 0xc6e00bf3, 0xd5a79147, 0x06ca6351, 0x14292967,
    0x27b70a85, 0x2e1b2138, 0x4d2c6dfc, 0x53380d13, 0x650a7354, 0x766a0abb, 0x81c2c92e, 0x92722c85,
    0xa2bfe8a1, 0xa81a664b, 0xc24b8b70, 0xc76c51a3, 0xd192e819, 0xd6990624, 0xf40e3585, 0x106aa070,
    0x19a4c116, 0x1e376c08, 0x2748774c, 0x34b0bcb5, 0x391c0cb3, 0x4ed8aa4a, 0x5b9cca4f, 0x682e6ff3,
    0x748f82ee, 0x78a5636f, 0x84c87814, 0x8cc70208, 0x90befffa, 0xa4506ceb, 0xbef9a3f7, 0xc67178f2,
];

impl Sha256 {
    fn new() -> Self {
        Self {
            state: [
                0x6a09e667, 0xbb67ae85, 0x3c6ef372, 0xa54ff53a, 0x510e527f, 0x9b05688c, 0x1f83d9ab,
                0x5be0cd19,
            ],
            buf: [0; 64],
            buffered: 0,
            len: 0,
        }
    }

    fn update(&mut self, mut data: &[u8]) {
        self.len = self.len.wrapping_add(data.len() as u64);
        while !data.is_empty() {
            let n = (64 - self.buffered).min(data.len());
            self.buf[self.buffered..self.buffered + n].copy_from_slice(&data[..n]);
            self.buffered += n;
            data = &data[n..];
            if self.buffered == 64 {
                let block = self.buf;
                self.compress(&block);
                self.buffered = 0;
            }
        }
    }

    fn finish(mut self) -> [u8; 32] {
        let bits = self.len.wrapping_mul(8);
        self.update(&[0x80]);
        while self.buffered != 56 {
            self.update(&[0]);
        }
        self.update(&bits.to_be_bytes());
        let mut out = [0u8; 32];
        for (i, w) in self.state.iter().enumerate() {
            out[i * 4..i * 4 + 4].copy_from_slice(&w.to_be_bytes());
        }
        out
    }

    fn compress(&mut self, block: &[u8; 64]) {
        let mut w = [0u32; 64];
        for i in 0..16 {
            w[i] = u32::from_be_bytes([
                block[i * 4],
                block[i * 4 + 1],
                block[i * 4 + 2],
                block[i * 4 + 3],
            ]);
        }
        for i in 16..64 {
            let s0 = w[i - 15].rotate_right(7) ^ w[i - 15].rotate_right(18) ^ (w[i - 15] >> 3);
            let s1 = w[i - 2].rotate_right(17) ^ w[i - 2].rotate_right(19) ^ (w[i - 2] >> 10);
            w[i] = w[i - 16]
                .wrapping_add(s0)
                .wrapping_add(w[i - 7])
                .wrapping_add(s1);
        }
        let [mut a, mut b, mut c, mut d, mut e, mut f, mut g, mut h] = self.state;
        for i in 0..64 {
            let s1 = e.rotate_right(6) ^ e.rotate_right(11) ^ e.rotate_right(25);
            let ch = (e & f) ^ (!e & g);
            let t1 = h
                .wrapping_add(s1)
                .wrapping_add(ch)
                .wrapping_add(K[i])
                .wrapping_add(w[i]);
            let s0 = a.rotate_right(2) ^ a.rotate_right(13) ^ a.rotate_right(22);
            let maj = (a & b) ^ (a & c) ^ (b & c);
            let t2 = s0.wrapping_add(maj);
            h = g;
            g = f;
            f = e;
            e = d.wrapping_add(t1);
            d = c;
            c = b;
            b = a;
            a = t1.wrapping_add(t2);
        }
        for (s, v) in self.state.iter_mut().zip([a, b, c, d, e, f, g, h]) {
            *s = s.wrapping_add(v);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::Sha256;

    fn hex(data: &[u8]) -> String {
        let mut h = Sha256::new();
        h.update(data);
        h.finish().iter().map(|b| format!("{b:02x}")).collect()
    }

    /// FIPS 180-4 vectors, plus one that crosses the 64-byte block boundary and
    /// one whose length lands exactly on the padding edge — the two places a
    /// hand-written compression loop goes wrong.
    #[test]
    fn matches_the_published_vectors() {
        assert_eq!(
            hex(b""),
            "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
        );
        assert_eq!(
            hex(b"abc"),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
        assert_eq!(
            hex(b"abcdbcdecdefdefgefghfghighijhijkijkljklmklmnlmnomnopnopq"),
            "248d6a61d20638b8e5c026930c3e6039a33ce45964ff2167f6ecedd419db06c1"
        );
        assert_eq!(
            hex(&vec![b'a'; 1_000_000]),
            "cdc76e5c9914fb9281a1c7e284d73e67f1809a48a497200e046d39ccc7112cd0"
        );
    }

    /// 55, 56 and 64 bytes: the last block that still fits its length field,
    /// the first that does not, and an exact multiple.
    #[test]
    fn handles_the_padding_boundaries() {
        for n in [55usize, 56, 64] {
            let data = vec![b'x'; n];
            // Fed whole, then one byte at a time — the buffering path must
            // agree with the bulk path.
            let mut byte_at_a_time = Sha256::new();
            for b in &data {
                byte_at_a_time.update(&[*b]);
            }
            let a: String = byte_at_a_time
                .finish()
                .iter()
                .map(|b| format!("{b:02x}"))
                .collect();
            assert_eq!(a, hex(&data), "length {n}");
        }
    }
}

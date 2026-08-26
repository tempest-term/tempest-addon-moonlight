//! Builds vendored moonlight-common-c as a static library and links it.
//!
//! moonlight-common-c is the Moonlight / Sunshine protocol core: RTSP
//! handshake, ENet control channel (AES-GCM), RTP video/audio depacketization
//! with Reed-Solomon FEC. It is a CMake project that bundles `enet` via
//! `add_subdirectory` and compiles `nanors` straight in through
//! `rswrapper.c`'s SIMD-variant `#include`s, and it links libcrypto for the
//! stream crypto.
//!
//! OpenSSL comes from `openssl-sys`'s vendored build (`openssl-src` compiles
//! it from source), which exports `DEP_OPENSSL_ROOT` / `DEP_OPENSSL_INCLUDE`
//! to dependents' build scripts. That is the whole reason this extraction was
//! cheap: in-tree it looked like Moonlight was borrowing libssh's OpenSSL, but
//! the dependency actually runs the other way, so nothing here needs a system
//! OpenSSL or a second build of it.
//!
//! Desktop targets only (macOS / Windows / Linux, x64 + arm64). There is no
//! mobile or wasm story: addons are a desktop mechanism.

use std::path::{Path, PathBuf};
use std::{env, fs};

fn main() {
    let target = env::var("TARGET").unwrap_or_default();
    let openssl_root = PathBuf::from(
        env::var("DEP_OPENSSL_ROOT")
            .expect("DEP_OPENSSL_ROOT unset — openssl-sys must be a direct dependency"),
    );
    let openssl_include = env::var("DEP_OPENSSL_INCLUDE")
        .map(PathBuf::from)
        .unwrap_or_else(|_| openssl_root.join("include"));

    build_moonlight(&openssl_include, &openssl_root, &target);
}

fn build_moonlight(openssl_include: &Path, openssl_root: &Path, target: &str) {
    let dir = Path::new("../../vendor/moonlight-common-c");
    assert!(
        dir.join("CMakeLists.txt").exists(),
        "vendor/moonlight-common-c missing. Run:\n  \
         git submodule update --init --recursive vendor/moonlight-common-c"
    );

    drop_werror(dir);

    let mut cfg = cmake::Config::new(dir);
    cfg.define("BUILD_SHARED_LIBS", "OFF")
        .define("USE_MBEDTLS", "OFF")
        .define("OPENSSL_USE_STATIC_LIBS", "TRUE")
        .define("OPENSSL_ROOT_DIR", openssl_root)
        .define("OPENSSL_INCLUDE_DIR", openssl_include);
    let dst = cfg.build();

    // moonlight-common-c has no install() rules — it is designed to be
    // consumed via add_subdirectory — so cmake's `--install` is a no-op and
    // the archives stay in the build tree. *Where* in the build tree depends
    // on the generator: Ninja and Make are single-config and drop archives
    // beside the project files, while MSVC's Visual Studio generator is
    // multi-config and nests them under the configuration name. Predicting
    // that is what made every Windows build fail to link upstream, so find
    // them on disk instead.
    let build_dir = dst.join("build");
    let mut archives = Vec::new();
    collect_static_archives(&build_dir, &mut archives);

    for lib in ["moonlight-common-c", "enet"] {
        let found = archives.iter().find(|path| {
            path.file_stem()
                .and_then(|s| s.to_str())
                // `moonlight-common-c.lib` (MSVC) or `libmoonlight-common-c.a`.
                .is_some_and(|stem| stem == lib || stem.trim_start_matches("lib") == lib)
        });
        match found.and_then(|path| path.parent()) {
            Some(parent) => println!("cargo:rustc-link-search=native={}", parent.display()),
            // Not fatal: rustc's own "could not find native static library"
            // names the missing lib just as clearly, and panicking here would
            // bury the rest of the diagnostics.
            None => println!(
                "cargo:warning=moonlight: no archive for {lib} under {}",
                build_dir.display()
            ),
        }
        println!("cargo:rustc-link-lib=static={lib}");
    }

    // Winsock / multimedia timers the moonlight + enet socket code needs.
    if target.contains("windows") {
        println!("cargo:rustc-link-lib=ws2_32");
        println!("cargo:rustc-link-lib=winmm");
    }

    println!("cargo:rerun-if-changed=../../vendor/moonlight-common-c/src");
}

/// Upstream builds with `-Wall -Wextra -Werror`. Newer Apple/LLVM clang finds
/// benign warnings upstream's CI compilers do not, so a fresh build fails on
/// warnings that are neither ours nor actionable here. Rewrite the flag in
/// place, idempotently.
fn drop_werror(dir: &Path) {
    let path = dir.join("CMakeLists.txt");
    let Ok(text) = fs::read_to_string(&path) else { return };
    const MARKER: &str = "# tempest: dropped -Werror";
    if text.contains(MARKER) {
        return;
    }
    const NEEDLE: &str = "-Wno-unused-parameter -Werror)";
    if !text.contains(NEEDLE) {
        println!("cargo:warning=moonlight: -Werror flag not found; upstream may have changed it");
        return;
    }
    let patched = format!("{MARKER}\n{}", text.replace(NEEDLE, "-Wno-unused-parameter)"));
    if let Err(e) = fs::write(&path, patched) {
        println!("cargo:warning=moonlight: could not drop -Werror: {e}");
    }
}

fn collect_static_archives(dir: &Path, out: &mut Vec<PathBuf>) {
    let Ok(entries) = fs::read_dir(dir) else { return };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            collect_static_archives(&path, out);
        } else if matches!(path.extension().and_then(|e| e.to_str()), Some("a" | "lib")) {
            out.push(path);
        }
    }
}

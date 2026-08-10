//! Builds the vendored `faster-enhancer` C runtime (FastEnhancer-Medium, 48 kHz
//! W8A8 int8 denoiser) via its upstream CMake, then links `libfe` into
//! lumen-voice. See vendor/faster-enhancer/ for the source and its license.
//!
//! fe requires GCC/Clang-style per-file ISA flags and explicitly rejects
//! MSVC/clang-cl. On Windows-MSVC targets it is cross-compiled with MinGW gcc
//! (fe's own CMake documents "MinGW or Clang GNU-driver Windows" as supported;
//! gcc accepts `-mavx2` and produces a COFF GNU archive) and linked via
//! lld-link — the CI sets `RUSTFLAGS="-C linker=lld-link"`, which reads both
//! the MSVC .lib files (webrtc-audio-processing, which requires MSVC) and the
//! GNU .a archive (fe). If gcc/ninja are unavailable the build degrades to
//! the WebRTC NS-only tier with a warning — never a hard failure.
use std::env;
use std::path::PathBuf;
use std::process::Command;

fn cmake(args: &[&str]) -> Result<(), String> {
    let st = Command::new("cmake").args(args).status();
    match st {
        Ok(s) if s.success() => Ok(()),
        Ok(s) => Err(format!("cmake {:?} exited with {s}", &args[..2.min(args.len())])),
        Err(e) => Err(format!("failed to run cmake {:?}: {e}", &args[..2.min(args.len())])),
    }
}

fn main() {
    let root = PathBuf::from(env::var("CARGO_MANIFEST_DIR").unwrap())
        .join("vendor/faster-enhancer");
    println!("cargo:rerun-if-changed={}", root.display());

    let out = PathBuf::from(env::var("OUT_DIR").unwrap());
    let target_env = env::var("CARGO_CFG_TARGET_ENV").unwrap_or_default();

    // Windows-MSVC: the app links with MSVC (webrtc-audio-processing requires
    // it), so fe is cross-compiled with MinGW gcc + Ninja here.
    if target_env.contains("msvc") {
        let build_dir = out.join("fe-build-mingw");
        let st = cmake(&[
            "-S",
            root.to_str().unwrap(),
            "-B",
            build_dir.to_str().unwrap(),
            "-G",
            "Ninja",
            "-DCMAKE_C_COMPILER=gcc",
            "-DCMAKE_BUILD_TYPE=Release",
            "-DFE_BUILD_TESTS=OFF",
            "-DFE_ENABLE_PROFILE=OFF",
        ])
        .and_then(|_| cmake(&["--build", build_dir.to_str().unwrap(), "--target", "fe"]));
        match st {
            Ok(()) => {
                println!("cargo:rustc-link-search=native={}", build_dir.display());
                println!("cargo:rustc-link-lib=static=fe");
            }
            Err(e) => println!(
                "cargo:warning=lumen-voice: faster-enhancer C runtime not built on \
                 MSVC ({e}); WebRTC NS-only tier in effect. On CI ensure MinGW gcc \
                 + ninja are on PATH and RUSTFLAGS=\"-C linker=lld-link\"."
            ),
        }
        return;
    }

    let build_dir = out.join("fe-build");
    cmake(&[
        "-S",
        root.to_str().unwrap(),
        "-B",
        build_dir.to_str().unwrap(),
        "-DCMAKE_BUILD_TYPE=Release",
        "-DFE_BUILD_TESTS=OFF",
        "-DFE_ENABLE_PROFILE=OFF",
    ])
    .unwrap_or_else(|e| panic!("{e}"));
    cmake(&["--build", build_dir.to_str().unwrap(), "--target", "fe", "--config", "Release"])
        .unwrap_or_else(|e| panic!("{e}"));

    println!("cargo:rustc-link-search=native={}", build_dir.display());
    println!("cargo:rustc-link-lib=static=fe");
    if !env::var("CARGO_CFG_TARGET_OS").map(|o| o == "windows").unwrap_or(false) {
        println!("cargo:rustc-link-lib=m");
    }
}

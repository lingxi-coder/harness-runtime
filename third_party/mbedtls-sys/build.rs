//! cc-built seam for the vendored mbedTLS C sources (TLS + crypto).
//!
//! Compiles the three standard mbedTLS static libraries — `mbedcrypto`,
//! `mbedx509`, `mbedtls` — from `third_party/mbedtls/library/*.c` using the
//! upstream configuration with LingXi's platform threading wrapper, then exports
//! that same include tree as `DEP_MBEDTLS_INCLUDE` for the direct dependents
//! (libgit2-sys / libssh2-sys).
//!
//! Two things matter for this seam to actually link:
//!
//! `lib.rs` owns the `-bundle` link directives and a used link anchor. The
//! build script must not also emit cc's default bundled link directives.
//! Archive link order is tls, x509, crypto, as declared in `lib.rs`.

use std::env;
use std::fs;
use std::path::{Path, PathBuf};

fn track_cc_environment() {
    let target = env::var("TARGET").unwrap();
    let host = env::var("HOST").unwrap();
    let target_underscored = target.replace(['-', '.'], "_");
    let kind = if target == host { "HOST" } else { "TARGET" };

    // cc 1.2.x checks these four spellings, in precedence order. Watch even
    // unset variants so that adding a higher-priority override rebuilds the C.
    for variable in ["CC", "CFLAGS", "AR", "ARFLAGS", "RANLIB", "RANLIBFLAGS"] {
        println!("cargo:rerun-if-env-changed={variable}_{target}");
        println!("cargo:rerun-if-env-changed={variable}_{target_underscored}");
        println!("cargo:rerun-if-env-changed={kind}_{variable}");
        println!("cargo:rerun-if-env-changed={variable}");
    }

    // cc's other tool-selection and flag controls, Apple SDK selection, and
    // the environment inherited by the supported native toolchains.
    for variable in [
        "CC_FORCE_DISABLE",
        "CC_KNOWN_WRAPPER_CUSTOM",
        "CC_SHELL_ESCAPED_FLAGS",
        "CRATE_CC_NO_DEFAULTS",
        "CROSS_COMPILE",
        "RUSTC_WRAPPER",
        "RUSTC_LINKER",
        "ZERO_AR_DATE",
        "PATH",
        "CPATH",
        "C_INCLUDE_PATH",
        "WASI_SDK_PATH",
        "WASI_SYSROOT",
        "WASM_MUSL_SYSROOT",
        "SDKROOT",
        "DEVELOPER_DIR",
        "MACOSX_DEPLOYMENT_TARGET",
        "IPHONEOS_DEPLOYMENT_TARGET",
        "TVOS_DEPLOYMENT_TARGET",
        "WATCHOS_DEPLOYMENT_TARGET",
        "XROS_DEPLOYMENT_TARGET",
        "VSCMD_ARG_TGT_ARCH",
        "VSCMD_ARG_VCVARS_SPECTRE",
        "VCINSTALLDIR",
        "VSINSTALLDIR",
        "VSTEL_MSBuildProjectFullPath",
        "VCToolsVersion",
        "WindowsSdkDir",
        "WindowsSDKVersion",
        "ProgramFiles(x86)",
        "ProgramFiles",
        "INCLUDE",
        "LIB",
    ] {
        println!("cargo:rerun-if-env-changed={variable}");
    }
}

fn copy_include_tree(source: &Path, target: &Path) {
    assert!(
        fs::symlink_metadata(source).unwrap().file_type().is_dir(),
        "mbedTLS include source is not a directory: {}",
        source.display()
    );
    fs::create_dir_all(target).unwrap();
    for entry in fs::read_dir(source).unwrap() {
        let entry = entry.unwrap();
        let kind = entry.file_type().unwrap();
        let dest = target.join(entry.file_name());
        if kind.is_dir() {
            copy_include_tree(&entry.path(), &dest);
        } else if kind.is_file() {
            fs::copy(entry.path(), dest).unwrap();
        } else {
            panic!(
                "unexpected non-file in mbedTLS include tree: {}",
                entry.path().display()
            );
        }
    }
}

/// X.509 sources (CMake `src_x509`).
const SRC_X509: &[&str] = &[
    "pkcs7.c",
    "x509.c",
    "x509_create.c",
    "x509_crl.c",
    "x509_crt.c",
    "x509_csr.c",
    "x509write.c",
    "x509write_crt.c",
    "x509write_csr.c",
];

/// TLS sources (CMake `src_tls`).
const SRC_TLS: &[&str] = &[
    "debug.c",
    "mps_reader.c",
    "mps_trace.c",
    "net_sockets.c",
    "ssl_cache.c",
    "ssl_ciphersuites.c",
    "ssl_client.c",
    "ssl_cookie.c",
    "ssl_debug_helpers_generated.c",
    "ssl_msg.c",
    "ssl_ticket.c",
    "ssl_tls.c",
    "ssl_tls12_client.c",
    "ssl_tls12_server.c",
    "ssl_tls13_keys.c",
    "ssl_tls13_server.c",
    "ssl_tls13_client.c",
    "ssl_tls13_generic.c",
];

fn main() {
    let manifest = PathBuf::from(env::var_os("CARGO_MANIFEST_DIR").unwrap());
    // `cargo_metadata(false)` suppresses all cc-rs Cargo directives, including
    // environment invalidation. Restore the relevant hints without emitting
    // cc-rs' conflicting native link directives.
    track_cc_environment();
    // `third_party/mbedtls` lives next to `third_party/mbedtls-sys`.
    let src_root = manifest.parent().unwrap().join("mbedtls");
    let source_include = src_root.join("include");
    let library = src_root.join("library");
    let out = PathBuf::from(env::var_os("OUT_DIR").unwrap());
    let include = out.join("lingxi-mbedtls-include");
    let wrapper = manifest.join("lingxi_config.h");
    assert!(
        fs::symlink_metadata(&wrapper)
            .unwrap()
            .file_type()
            .is_file(),
        "mbedTLS configuration wrapper is not a regular file"
    );

    println!("cargo:rerun-if-changed={}", library.display());
    println!("cargo:rerun-if-changed={}", source_include.display());
    println!("cargo:rerun-if-changed={}", wrapper.display());

    // Every C consumer receives this same include tree through DEP_MBEDTLS_INCLUDE.
    // Retain the release config unchanged and layer the platform threading choice
    // on top, so public Mbed TLS struct layouts agree across all C objects.
    match fs::symlink_metadata(&include) {
        Ok(meta) => {
            assert!(
                meta.file_type().is_dir(),
                "mbedTLS generated include path is not a directory"
            );
            fs::remove_dir_all(&include).unwrap();
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => panic!("cannot inspect generated mbedTLS include directory: {error}"),
    }
    copy_include_tree(&source_include, &include);
    let config = include.join("mbedtls/mbedtls_config.h");
    fs::copy(&config, include.join("mbedtls/mbedtls_config_upstream.h")).unwrap();
    fs::copy(&wrapper, &config).unwrap();

    // Partition the library *.c into the three standard mbedTLS libs.
    let mut x509_files = Vec::new();
    let mut tls_files = Vec::new();
    let mut crypto_files = Vec::new();

    assert!(
        fs::symlink_metadata(&library).unwrap().file_type().is_dir(),
        "mbedTLS library source is not a directory"
    );
    let mut entries: Vec<PathBuf> = fs::read_dir(&library)
        .unwrap()
        .map(|entry| {
            let entry = entry.unwrap();
            assert!(
                entry.file_type().unwrap().is_file(),
                "unexpected non-file in mbedTLS library tree: {}",
                entry.path().display()
            );
            entry.path()
        })
        .filter(|p| p.extension().and_then(|s| s.to_str()) == Some("c"))
        .collect();
    entries.sort();

    for path in entries {
        let name = path.file_name().unwrap().to_str().unwrap().to_string();
        if SRC_X509.contains(&name.as_str()) {
            x509_files.push(path);
        } else if SRC_TLS.contains(&name.as_str()) {
            tls_files.push(path);
        } else {
            crypto_files.push(path);
        }
    }

    let base = |files: &[PathBuf]| {
        let mut cfg = cc::Build::new();
        cfg.include(&include)
            // The library *.c use `#include "common.h"` etc. from library/.
            .include(&library)
            .define("MBEDTLS_CONFIG_FILE", "\"mbedtls/mbedtls_config.h\"")
            .warnings(false)
            // lib.rs declares the -bundle native links and the link anchor.
            // cc's implicit bundled link directives must not conflict.
            .cargo_metadata(false);
        // NDK API floor: ensure --target uses API 29 (the P0a/G7 lesson).
        bump_android_api(&mut cfg);
        for f in files {
            cfg.file(f);
        }
        cfg
    };

    // Build all three archives with the same exported include tree.
    base(&tls_files).compile("mbedtls");
    base(&x509_files).compile("mbedx509");
    base(&crypto_files).compile("mbedcrypto");

    println!("cargo:include={}", include.display());
    // Only the search path is emitted here. The `name`/`kind`/`modifiers` of the
    // three archives are declared via `#[link(..)]` in `lib.rs` (with a
    // referenced anchor symbol) so rustc keeps this crate and its archives in
    // the final link instead of pruning the empty rlib. cc's own auto-emit is
    // suppressed above (`cargo_metadata(false)`) so it cannot fight that.
    println!("cargo:rustc-link-search=native={}", out.display());
}

/// When cross-compiling for Android, `cc` defaults the `--target=<triple>NN`
/// API level from the NDK toolchain; some host setups default below 29, which
/// breaks newer libc symbols. Force API 29 if the target is Android and the
/// caller hasn't already pinned a higher level. (P0a / G7 lesson.)
fn bump_android_api(cfg: &mut cc::Build) {
    let target = env::var("TARGET").unwrap_or_default();
    if !target.contains("android") {
        return;
    }
    // cc derives the clang `--target` triple+api itself; we only need to make
    // sure the minimum is 29. cc honors CLANG_TARGET via the per-file flag, but
    // the simplest robust knob is the `ANDROID_API`/`-D__ANDROID_API__` floor.
    cfg.define("__ANDROID_API__", "29");
}

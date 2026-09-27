//! Compiles the vendored libcap sources (`third_party/libcap/libcap`) into a
//! static `libcap.a` when targeting Android. The NDK ships no libcap, so the
//! minijail NDK build links against this archive. Host builds are a no-op so
//! the workspace stays checkable on macOS/Linux.

use std::env;
use std::path::PathBuf;

fn main() {
    // Only Android targets get a real build; host builds emit nothing.
    if env::var("CARGO_CFG_TARGET_OS").as_deref() != Ok("android") {
        return;
    }
    let manifest = PathBuf::from(env::var("CARGO_MANIFEST_DIR").unwrap());
    let libcap_src = manifest
        .ancestors()
        .nth(3) // platforms/android-libcap -> platforms -> lingxi-code -> repo root
        .unwrap()
        .join("third_party/libcap/libcap");
    assert!(
        libcap_src.join("cap_proc.c").exists(),
        "vendored libcap not found at {} — clone libcap-2.69 into third_party/libcap",
        libcap_src.display()
    );

    cc::Build::new()
        .files(
            [
                "cap_alloc.c",
                "cap_proc.c",
                "cap_extint.c",
                "cap_flag.c",
                "cap_text.c",
                "cap_file.c",
            ]
            .iter()
            .map(|f| libcap_src.join(f)),
        )
        .include(libcap_src.join("include"))
        .include(libcap_src.join("include/uapi"))
        .include(manifest.join("gen")) // pre-generated cap_names.h
        .define("_GNU_SOURCE", None)
        .warnings(false)
        .compile("cap");
    println!("cargo:rerun-if-changed={}", libcap_src.display());
    println!(
        "cargo:rerun-if-changed={}",
        manifest.join("gen/cap_names.h").display()
    );
}

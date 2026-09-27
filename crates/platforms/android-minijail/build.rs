//! Builds `libminijail.a` for Android targets straight from the vendored
//! upstream sources (`third_party/minijail`) with the NDK toolchain.
//!
//! Why not the upstream `minijail-sys` build script? Its static fallback
//! shells out to the `ChromiumOS` `Makefile`/`common.mk`, which is unbuildable
//! from a macOS host: it derives `CC` from a `CROSS_COMPILE` prefix that has
//! no NDK equivalent, post-processes `.d` files with GNU-sed-only
//! expressions (BSD sed errors out), and never links `sys/capability.h`
//! include paths (the NDK ships no libcap). Instead this script mirrors what
//! AOSP's own `Android.bp` does for `libminijail`: generate the per-arch
//! syscall/constant tables with the target clang, then `cc`-compile the core
//! sources plus the generated tables into one static archive.
//!
//! Host builds are a no-op so the workspace stays checkable on macOS/Linux.

use std::env;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

/// Minimum Android API level libminijail compiles against: `fexecve` and
/// `sigtimedwait` need bionic 28+, and upstream `Android.bp` declares
/// `min_sdk_version: "29"` — the same level the P0a plan targets
/// (`aarch64-linux-android29`). cargo-ndk defaults to platform 21, so
/// build.rs bumps any lower `--target=<triple><api>` itself.
const MIN_ANDROID_API: u32 = 29;

/// Core libminijail sources (the upstream Makefile's `CORE_OBJECT_FILES`,
/// which is `Android.bp`'s `libminijailSrcFiles` plus `config_parser.c`).
const CORE_SOURCES: [&str; 9] = [
    "bpf.c",
    "config_parser.c",
    "landlock_util.c",
    "libminijail.c",
    "signal_handler.c",
    "syscall_filter.c",
    "syscall_wrapper.c",
    "system.c",
    "util.c",
];

fn main() {
    println!("cargo:rerun-if-changed=build.rs");
    // Explicit rerun-if-changed disables cargo's whole-package default — the
    // security-relevant shim header must retrigger the C build when edited.
    println!("cargo:rerun-if-changed=shim-include");
    // Only Android targets get a real build; host builds emit nothing.
    if env::var("CARGO_CFG_TARGET_OS").as_deref() != Ok("android") {
        return;
    }
    let manifest = PathBuf::from(env::var("CARGO_MANIFEST_DIR").unwrap());
    let repo_root = manifest
        .ancestors()
        .nth(3) // platforms/android-minijail -> platforms -> lingxi-code -> repo root
        .unwrap()
        .to_path_buf();
    let minijail_src = repo_root.join("third_party/minijail");
    assert!(
        minijail_src.join("libminijail.c").exists(),
        "vendored minijail not found at {} — clone minijail into third_party/minijail",
        minijail_src.display()
    );
    let libcap_src = repo_root.join("third_party/libcap/libcap");
    assert!(
        libcap_src.join("include/sys/capability.h").exists(),
        "vendored libcap headers not found at {} (the NDK has no sys/capability.h)",
        libcap_src.display()
    );
    let out_dir = PathBuf::from(env::var("OUT_DIR").unwrap());

    // The versioned clang target for this arch, e.g. `aarch64-linux-android29`.
    let arch = env::var("CARGO_CFG_TARGET_ARCH").unwrap();
    let versioned_target = format!("{arch}-linux-android{MIN_ANDROID_API}");
    bump_android_api_in_cflags_env();

    // The exact target clang the `cc` crate will use (cargo-ndk exports
    // `CC_<target>`); the gen scripts need it as a single `CC` string so the
    // emitted syscall numbers are the *target* architecture's. The trailing
    // `--target` wins over any earlier one baked into the tool args.
    let compiler = cc::Build::new().get_compiler();
    let mut cc_cmd = compiler.path().display().to_string();
    for arg in compiler.args() {
        cc_cmd.push(' ');
        cc_cmd.push_str(arg.to_str().expect("non-UTF-8 compiler arg"));
    }
    cc_cmd.push_str(&format!(" --target={versioned_target}"));

    let syscalls_gen = run_gen_script(&minijail_src, "gen_syscalls.sh", &out_dir, &cc_cmd);
    let constants_gen = run_gen_script(&minijail_src, "gen_constants.sh", &out_dir, &cc_cmd);
    verify_syscall_table(&syscalls_gen);
    verify_table(&constants_gen, 100);

    let mut build = cc::Build::new();
    build
        .files(CORE_SOURCES.iter().map(|f| minijail_src.join(f)))
        .file(&syscalls_gen)
        .file(&constants_gen)
        // FIRST: the kernel-6.14 `linux/securebits.h` shim — system.c
        // static-asserts the 6.14 bit set on __ANDROID__, which neither the
        // NDK sysroot nor libcap's vendored uapi headers have yet.
        .include(manifest.join("shim-include"))
        .include(&minijail_src)
        .include(libcap_src.join("include"))
        .include(libcap_src.join("include/uapi"))
        // Belt-and-braces API floor when no CFLAGS env exists (a bare
        // `--target=aarch64-linux-android` would default below 28).
        .flag(format!("--target={versioned_target}"))
        // Mirror Android.bp's `libminijail_flags` (plus the LFS defines the
        // upstream Makefile always sets).
        .define("_FILE_OFFSET_BITS", "64")
        .define("_LARGEFILE_SOURCE", None)
        .define("_LARGEFILE64_SOURCE", None)
        .define("ALLOW_DEBUG_LOGGING", None)
        .define("ALLOW_DUPLICATE_SYSCALLS", None)
        .define("DEFAULT_PIVOT_ROOT", "\"/var/empty\"")
        .define("BINDMOUNT_ALLOWED_PREFIXES", "\"\"")
        // Unguarded use in libminijail.c; Android never installs the
        // preload library, so point it at an invalid path like AOSP does.
        .define("PRELOADPATH", "\"/invalidminijailpreload.so\"")
        .warnings(false);
    build.compile("minijail");

    println!("cargo:rerun-if-changed={}", minijail_src.display());
}

/// Rewrite `--target=<triple><api>` occurrences in the CFLAGS-style env vars
/// the `cc` crate appends LAST (so they beat any `.flag()` we set): cargo-ndk
/// exports them pinned to its `--platform` (default 21), which is below
/// libminijail's bionic floor. Bumping only the API digits keeps every other
/// cargo-ndk-provided flag intact.
fn bump_android_api_in_cflags_env() {
    let target = env::var("TARGET").unwrap();
    let names = [
        "CFLAGS".to_string(),
        "TARGET_CFLAGS".to_string(),
        format!("CFLAGS_{target}"),
        format!("CFLAGS_{}", target.replace('-', "_")),
    ];
    for name in names {
        let Ok(value) = env::var(&name) else { continue };
        let bumped: Vec<String> = value
            .split_whitespace()
            .map(|tok| match tok.strip_prefix("--target=") {
                Some(triple) => format!("--target={}", bump_android_api(triple)),
                None => tok.to_string(),
            })
            .collect();
        let bumped = bumped.join(" ");
        if bumped != value {
            // Affects only this build-script process; `cc` re-reads the env
            // when it assembles the compiler command line.
            env::set_var(&name, bumped);
        }
    }
}

/// Bump the trailing API digits of an `<arch>-linux-android[eabi]<api>`
/// clang triple to [`MIN_ANDROID_API`]; non-android or unversioned triples
/// pass through unchanged.
fn bump_android_api(triple: &str) -> String {
    let digits_at = triple
        .rfind(|c: char| !c.is_ascii_digit())
        .map_or(0, |i| i + 1);
    let (prefix, digits) = triple.split_at(digits_at);
    if !prefix.contains("-linux-android") {
        return triple.to_string();
    }
    match digits.parse::<u32>() {
        Ok(api) if api < MIN_ANDROID_API => format!("{prefix}{MIN_ANDROID_API}"),
        _ => triple.to_string(),
    }
}

/// Run an upstream table generator (`gen_syscalls.sh` / `gen_constants.sh`)
/// with the target clang and return the generated C file path.
fn run_gen_script(minijail_src: &Path, script: &str, out_dir: &Path, cc_cmd: &str) -> PathBuf {
    let out_file = out_dir.join(script.replace("gen_", "lib").replace(".sh", ".gen.c"));
    let status = Command::new("sh")
        .arg(minijail_src.join(script))
        .arg(&out_file)
        .env("CC", cc_cmd)
        .env("SRC", minijail_src)
        .current_dir(out_dir)
        .status()
        .unwrap_or_else(|e| panic!("failed to spawn {script}: {e}"));
    assert!(status.success(), "{script} failed with {status}");
    out_file
}

/// Generated-table sanity: a silently-empty table (the BSD-grep libcap trap)
/// must fail the build, not ship a libminijail that can't resolve syscalls.
fn verify_table(path: &Path, min_entries: usize) -> String {
    let text = fs::read_to_string(path)
        .unwrap_or_else(|e| panic!("cannot read generated {}: {e}", path.display()));
    let entries = text.matches("{ \"").count();
    assert!(
        entries >= min_entries,
        "generated table {} looks wrong: only {entries} entries (expected >= {min_entries}) — \
         check the gen script ran with the NDK target clang",
        path.display(),
    );
    text
}

/// Arch-correctness of the syscall table: every Linux arch has `openat`;
/// legacy `open` exists on `x86_64` but NOT on `aarch64`, so its presence is a
/// cheap proof the table was generated for the right architecture.
fn verify_syscall_table(path: &Path) {
    let text = verify_table(path, 200);
    assert!(
        text.contains("\"openat\""),
        "syscall table {} has no openat entry",
        path.display()
    );
    let has_legacy_open = text.contains("{ \"open\", __NR_open }");
    match env::var("CARGO_CFG_TARGET_ARCH").as_deref() {
        Ok("x86_64") => assert!(
            has_legacy_open,
            "x86_64 syscall table is missing __NR_open — wrong-arch headers were used"
        ),
        Ok("aarch64") => assert!(
            !has_legacy_open,
            "aarch64 syscall table contains __NR_open — host or x86 headers leaked in"
        ),
        _ => {}
    }
}

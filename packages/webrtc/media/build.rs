fn main() {
    println!("cargo:rerun-if-env-changed=PKG_CONFIG_PATH");
    if std::env::var_os("CARGO_FEATURE_STATIC_PLUGINS").is_some() {
        let ios = std::env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("ios");
        for name in [
            "gstapp",
            "gstaudioconvert",
            "gstaudioresample",
            "gstcoreelements",
            "gstopus",
            "gstrtp",
            "gstrtpmanager",
            "gstvpx",
            "gstvideoconvertscale",
        ] {
            let library = pkg_config::Config::new()
                .statik(true)
                .cargo_metadata(!ios)
                .probe(name)
                .expect("static GStreamer plugin SDK required");
            if ios {
                for path in library.link_paths {
                    println!("cargo:rustc-link-search=native={}", path.display());
                }
            }
        }
        if ios {
            println!("cargo:rustc-link-lib=static:-bundle=GStreamer");
        }
        if std::env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("android") {
            // ORC's instruction-cache flush uses the NDK compiler-rt helper.
            // Rust passes -nodefaultlibs, so Clang cannot add it implicitly.
            let output = cc::Build::new()
                .get_compiler()
                .to_command()
                .arg("--print-libgcc-file-name")
                .output()
                .expect("query NDK compiler runtime");
            assert!(output.status.success(), "NDK compiler runtime query failed");
            let path = std::path::PathBuf::from(String::from_utf8(output.stdout).unwrap().trim());
            assert!(path.is_file(), "NDK compiler runtime archive missing");
            let name = path
                .file_stem()
                .unwrap()
                .to_str()
                .unwrap()
                .strip_prefix("lib")
                .unwrap();
            println!(
                "cargo:rustc-link-search=native={}",
                path.parent().unwrap().display()
            );
            println!("cargo:rustc-link-lib=static={name}");
        }
    }
    if std::env::var_os("CARGO_FEATURE_GSTREAMER_MEDIA").is_some()
        && std::env::var("HOST").ok() == std::env::var("TARGET").ok()
        && std::env::var("PROFILE").as_deref() != Ok("release")
    {
        let output = std::process::Command::new("pkg-config")
            .args(["--variable=pluginsdir", "gstreamer-1.0"])
            .output()
            .expect("pkg-config required for native media development");
        assert!(
            output.status.success(),
            "GStreamer development SDK required"
        );
        println!(
            "cargo:rustc-env=LINGXI_DEV_GST_PLUGIN_DIR={}",
            String::from_utf8(output.stdout).unwrap().trim()
        );
    } else {
        println!("cargo:rustc-env=LINGXI_DEV_GST_PLUGIN_DIR=");
    }
}

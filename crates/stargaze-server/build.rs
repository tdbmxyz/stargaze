/// Build script for `stargaze-server`.
///
/// The `ffmpeg-sys-next` build script discovers `FFmpeg` libraries via `pkg-config` but
/// only links the top-level libraries (avcodec, avformat, etc.) without their transitive
/// dependencies.  When `FFmpeg` is installed as shared libraries (the typical system
/// package case), dynamic linking handles transitive deps automatically at runtime.
/// When `FFmpeg` is statically linked, the linker needs every transitive dependency
/// explicitly.
///
/// This build script re-queries `pkg-config` to obtain any additional linker flags
/// beyond the top-level `FFmpeg` libraries and emits them via `cargo:rustc-link-lib`.
/// It also queries `opus` for linker paths so `audiopus_sys` can find `libopus`.
/// It uses dynamic (non-`--static`) flags by default, which is correct for system
/// packages that provide shared libraries.
fn main() {
    // Inherit LD_LIBRARY_PATH so the child pkg-config process can find libpkgconf.
    let ld_path = std::env::var("LD_LIBRARY_PATH").unwrap_or_default();
    let pkg_config_path = std::env::var("PKG_CONFIG_PATH").unwrap_or_default();

    emit_ffmpeg_link_flags(&ld_path, &pkg_config_path);
    emit_opus_link_paths(&ld_path, &pkg_config_path);
    build_wayland_capture_bridge();
}

fn build_wayland_capture_bridge() {
    use std::path::{Path, PathBuf};

    println!("cargo:rerun-if-changed=src/capture/wayland_capture.c");
    println!("cargo:rerun-if-changed=src/capture/wayland_capture.h");
    let mut build = cc::Build::new();
    for library in ["wayland-client", "gbm", "libdrm"] {
        let library =
            pkg_config::probe_library(library).expect("Wayland capture development files");
        build.includes(library.include_paths);
    }
    pkg_config::Config::new()
        .atleast_version("1.49")
        .probe("wayland-protocols")
        .expect("wayland-protocols >= 1.49 development files");
    let protocols = pkg_config::get_variable("wayland-protocols", "pkgdatadir")
        .expect("wayland-protocols pkgdatadir");
    let out = PathBuf::from(std::env::var_os("OUT_DIR").expect("OUT_DIR"));
    for (name, xml) in [
        (
            "color-management-v1",
            Path::new(&protocols).join("staging/color-management/color-management-v1.xml"),
        ),
        (
            "ext-image-capture-source-v1",
            Path::new(&protocols)
                .join("staging/ext-image-capture-source/ext-image-capture-source-v1.xml"),
        ),
        (
            "ext-foreign-toplevel-list-v1",
            Path::new(&protocols)
                .join("staging/ext-foreign-toplevel-list/ext-foreign-toplevel-list-v1.xml"),
        ),
        (
            "ext-image-copy-capture-v1",
            Path::new(&protocols)
                .join("staging/ext-image-copy-capture/ext-image-copy-capture-v1.xml"),
        ),
        (
            "linux-dmabuf-v1",
            Path::new(&protocols).join("stable/linux-dmabuf/linux-dmabuf-v1.xml"),
        ),
        (
            "xx-image-capture-color-management-v1",
            PathBuf::from("protocols/xx-image-capture-color-management-v1.xml"),
        ),
    ] {
        println!("cargo:rerun-if-changed={}", xml.display());
        for (mode, suffix) in [
            ("client-header", "client-protocol.h"),
            ("private-code", "protocol.c"),
        ] {
            let status = std::process::Command::new("wayland-scanner")
                .arg(mode)
                .arg(&xml)
                .arg(out.join(format!("{name}-{suffix}")))
                .status()
                .expect("wayland-scanner");
            assert!(status.success(), "wayland-scanner failed for {name}");
        }
        build.file(out.join(format!("{name}-protocol.c")));
    }
    build
        .file("src/capture/wayland_capture.c")
        .include(&out)
        .flag_if_supported("-std=c11")
        .opt_level(2)
        .warnings(true)
        .compile("stargaze_wayland_capture");
}

/// Queries `pkg-config` for `FFmpeg` transitive link flags and emits them.
fn emit_ffmpeg_link_flags(ld_path: &str, pkg_config_path: &str) {
    // Only run the extra link fixup when pkg-config is available.
    let Ok(output) = std::process::Command::new("pkg-config")
        .args(["--libs", "libavformat", "libavcodec", "libavutil"])
        .env("PKG_CONFIG_PATH", pkg_config_path)
        .env("LD_LIBRARY_PATH", ld_path)
        .output()
    else {
        return;
    };

    if !output.status.success() {
        return;
    }

    let stdout = String::from_utf8_lossy(&output.stdout);
    for token in stdout.split_whitespace() {
        if let Some(lib) = token.strip_prefix("-l") {
            // Skip the top-level FFmpeg libs — ffmpeg-sys-next already links them.
            if !matches!(
                lib,
                "avcodec"
                    | "avformat"
                    | "avutil"
                    | "avfilter"
                    | "avdevice"
                    | "swscale"
                    | "swresample"
                    | "postproc"
            ) {
                println!("cargo:rustc-link-lib={lib}");
            }
        } else if let Some(path) = token.strip_prefix("-L") {
            println!("cargo:rustc-link-search=native={path}");
        }
    }
}

/// Queries `pkg-config` for `opus` library paths so `audiopus_sys` can find `libopus`.
fn emit_opus_link_paths(ld_path: &str, pkg_config_path: &str) {
    let Ok(output) = std::process::Command::new("pkg-config")
        .args(["--libs", "opus"])
        .env("PKG_CONFIG_PATH", pkg_config_path)
        .env("LD_LIBRARY_PATH", ld_path)
        .output()
    else {
        return;
    };

    if !output.status.success() {
        return;
    }

    let stdout = String::from_utf8_lossy(&output.stdout);
    for token in stdout.split_whitespace() {
        if let Some(path) = token.strip_prefix("-L") {
            println!("cargo:rustc-link-search=native={path}");
        }
    }
}

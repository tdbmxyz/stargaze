/// Build script for `stargaze-client`.
///
/// Re-queries `pkg-config` to obtain transitive linker flags for
/// `FFmpeg` shared libraries, SDL2 and opus library search paths.
fn main() {
    let ld_path = std::env::var("LD_LIBRARY_PATH").unwrap_or_default();
    let pkg_config_path = std::env::var("PKG_CONFIG_PATH").unwrap_or_default();

    query_ffmpeg_libs(&pkg_config_path, &ld_path);
    query_sdl2_paths(&pkg_config_path, &ld_path);
    query_opus_paths(&pkg_config_path, &ld_path);
    build_wayland_color_bridge();
}

fn build_wayland_color_bridge() {
    println!("cargo:rerun-if-changed=src/render/wayland_hdr.c");
    let wayland =
        pkg_config::probe_library("wayland-client").expect("wayland-client development files");
    let protocols = pkg_config::get_variable("wayland-protocols", "pkgdatadir")
        .expect("wayland-protocols >= 1.41 development files");
    let xml =
        std::path::Path::new(&protocols).join("staging/color-management/color-management-v1.xml");
    let out = std::path::PathBuf::from(std::env::var_os("OUT_DIR").expect("OUT_DIR"));
    for (mode, file) in [
        ("client-header", "color-management-v1-client-protocol.h"),
        ("private-code", "color-management-v1-protocol.c"),
    ] {
        let status = std::process::Command::new("wayland-scanner")
            .arg(mode)
            .arg(&xml)
            .arg(out.join(file))
            .status()
            .expect("wayland-scanner");
        assert!(status.success(), "wayland-scanner failed");
    }
    cc::Build::new()
        .file("src/render/wayland_hdr.c")
        .file(out.join("color-management-v1-protocol.c"))
        .include(&out)
        .includes(wayland.include_paths)
        .flag_if_supported("-std=c11")
        .opt_level(2)
        .warnings(true)
        .compile("stargaze_wayland_hdr");
}

fn query_ffmpeg_libs(pkg_config_path: &str, ld_path: &str) {
    let Ok(output) = std::process::Command::new("pkg-config")
        .args(["--libs", "libavcodec", "libavutil", "libswscale"])
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

fn query_sdl2_paths(pkg_config_path: &str, ld_path: &str) {
    let Ok(output) = std::process::Command::new("pkg-config")
        .args(["--libs", "sdl2"])
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

fn query_opus_paths(pkg_config_path: &str, ld_path: &str) {
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

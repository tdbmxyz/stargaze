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

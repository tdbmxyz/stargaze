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
/// It uses dynamic (non-`--static`) flags by default, which is correct for system
/// packages that provide shared libraries.
fn main() {
    // Inherit LD_LIBRARY_PATH so the child pkg-config process can find libpkgconf.
    let ld_path = std::env::var("LD_LIBRARY_PATH").unwrap_or_default();

    // Only run the extra link fixup when pkg-config is available.
    let Ok(output) = std::process::Command::new("pkg-config")
        .args(["--libs", "libavformat", "libavcodec", "libavutil"])
        .env(
            "PKG_CONFIG_PATH",
            std::env::var("PKG_CONFIG_PATH").unwrap_or_default(),
        )
        .env("LD_LIBRARY_PATH", &ld_path)
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

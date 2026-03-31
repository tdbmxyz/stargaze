/// Build script for `stargaze-server`.
///
/// The `ffmpeg-sys-next` build script discovers `FFmpeg` libraries via `pkg-config` but
/// only links the top-level libraries (avcodec, avformat, etc.) without their transitive
/// static dependencies.  When `FFmpeg` is built as static archives (`.a` files), the
/// linker also needs every library that `FFmpeg` itself depends on (libxml2, gnutls,
/// x264, x265, etc.).
///
/// This build script re-queries `pkg-config` with the `--static` flag to obtain the
/// complete set of linker flags and emits them via `cargo:rustc-link-lib`.
fn main() {
    // Add the ffmpeg-deps directory which contains unversioned .so symlinks
    // for system libraries that FFmpeg depends on transitively.
    let home = std::env::var("HOME").unwrap_or_else(|_| "/home/vscode".to_string());
    let deps_dir = format!("{home}/.local/usr/lib/x86_64-linux-gnu/ffmpeg-deps");
    if std::path::Path::new(&deps_dir).is_dir() {
        println!("cargo:rustc-link-search=native={deps_dir}");
    }

    // Inherit LD_LIBRARY_PATH so the child pkg-config process can find libpkgconf.
    let ld_path = std::env::var("LD_LIBRARY_PATH").unwrap_or_default();

    // Only run the extra static-link fixup when pkg-config is available.
    let Ok(output) = std::process::Command::new("pkg-config")
        .args([
            "--libs",
            "--static",
            "libavformat",
            "libavcodec",
            "libavutil",
        ])
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

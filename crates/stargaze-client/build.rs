/// Build script for `stargaze-client`.
///
/// Same purpose as stargaze-server's build.rs: re-queries `pkg-config`
/// to obtain transitive linker flags for FFmpeg shared libraries.
fn main() {
    let ld_path = std::env::var("LD_LIBRARY_PATH").unwrap_or_default();

    let Ok(output) = std::process::Command::new("pkg-config")
        .args(["--libs", "libavcodec", "libavutil", "libswscale"])
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

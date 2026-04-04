{
  description = "Stargaze — Rust-native low-latency desktop/game streaming";

  inputs = {
    nixpkgs.url = "github:NixOS/nixpkgs/nixos-unstable";
    fenix = {
      url = "github:nix-community/fenix";
      inputs.nixpkgs.follows = "nixpkgs";
    };
  };

  outputs = { self, nixpkgs, fenix }:
    let
      system = "x86_64-linux";
      pkgs = import nixpkgs { inherit system; };

      # Rust nightly toolchain via fenix — single source of truth for the
      # Rust version.  Do NOT add a rust-toolchain.toml; this flake manages it.
      toolchain = fenix.packages.${system}.complete.withComponents [
        "cargo"
        "clippy"
        "rust-src"
        "rustc"
        "rustfmt"
      ];

      # FFmpeg 7.x — matches ffmpeg-next = "7" / ffmpeg-sys-next = "7" in Cargo.toml.
      ffmpeg = pkgs.ffmpeg_7-full;
    in
    {
      devShells.${system}.default = pkgs.mkShell {
        nativeBuildInputs = [
          toolchain
          pkgs.pkg-config
          fenix.packages.${system}.rust-analyzer
        ];

        buildInputs = [
          # Video: FFmpeg 7 (NVENC, VAAPI, SW decode — all included in -full)
          ffmpeg

          # Capture: PipeWire + portal
          pkgs.pipewire
          pkgs.dbus

          # Render / audio playback
          pkgs.SDL2

          # Audio codec
          pkgs.libopus

          # Bindgen (ffmpeg-sys-next, pipewire-sys)
          pkgs.libclang
          pkgs.llvmPackages.libclang
        ];

        # pkg-config needs to find headers + libs for all -sys crates.
        # Nix sets PKG_CONFIG_PATH automatically via buildInputs, but
        # LIBCLANG_PATH and bindgen flags need explicit help.
        env = {
          LIBCLANG_PATH = "${pkgs.llvmPackages.libclang.lib}/lib";
        };

        shellHook = ''
          # Let ffmpeg-sys-next find FFmpeg headers via bindgen.
          export BINDGEN_EXTRA_CLANG_ARGS="$(< ${pkgs.stdenv.cc}/nix-support/libc-cflags) -isystem ${pkgs.llvmPackages.libclang.lib}/lib/clang/${pkgs.llvmPackages.libclang.version}/include"

          # Runtime library path so test/debug binaries can find .so files
          # (Nix sets PKG_CONFIG_PATH for compilation but not LD_LIBRARY_PATH).
          export LD_LIBRARY_PATH="${pkgs.lib.makeLibraryPath [
            ffmpeg
            pkgs.pipewire
            pkgs.dbus
            pkgs.SDL2
            pkgs.libopus
          ]}''${LD_LIBRARY_PATH:+:$LD_LIBRARY_PATH}"
        '';
      };
    };
}

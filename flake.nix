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

      # Separate nixpkgs instantiation with unfree + CUDA support enabled.
      # Only used by the CUDA devShell — keeps the default shell free-only.
      pkgsCuda = import nixpkgs {
        inherit system;
        config = {
          allowUnfree = true;
          cudaSupport = true;
        };
      };

      # ── Rust nightly toolchain (fenix) ──────────────────────────────
      # Single source of truth for the Rust version.
      # Do NOT add a rust-toolchain.toml; this flake manages it.
      toolchain = fenix.packages.${system}.complete.withComponents [
        "cargo"
        "clippy"
        "rust-src"
        "rustc"
        "rustfmt"
      ];

      # ── FFmpeg 7.x ─────────────────────────────────────────────────
      # Matches ffmpeg-next = "7" / ffmpeg-sys-next = "7" in Cargo.toml.
      ffmpeg = pkgs.ffmpeg_7-full;
      ffmpegCuda = pkgsCuda.ffmpeg_7-full; # built with CUDA / NVENC support

      # ── Shared native dependencies ─────────────────────────────────
      # Common to both server and client (compile-time).
      commonBuildInputs = [
        ffmpeg
        pkgs.pipewire
        pkgs.dbus
        pkgs.SDL2
        pkgs.libopus
        pkgs.libclang
        pkgs.llvmPackages.libclang
      ];

      commonNativeBuildInputs = [
        toolchain
        pkgs.pkg-config
      ];

      # ── Bindgen / LIBCLANG environment ─────────────────────────────
      bindgenEnv = {
        LIBCLANG_PATH = "${pkgs.llvmPackages.libclang.lib}/lib";
      };

      bindgenShellHook = ''
        export BINDGEN_EXTRA_CLANG_ARGS="$(< ${pkgs.stdenv.cc}/nix-support/libc-cflags) -isystem ${pkgs.llvmPackages.libclang.lib}/lib/clang/${pkgs.llvmPackages.libclang.version}/include"
      '';

      # Runtime library path for test/debug binaries inside devShells.
      runtimeLibPath = pkgs.lib.makeLibraryPath [
        ffmpeg
        pkgs.pipewire
        pkgs.dbus
        pkgs.SDL2
        pkgs.libopus
      ];

      # ── Rust platform (for Nix package builds) ─────────────────────
      rustPlatform = pkgs.makeRustPlatform {
        cargo = toolchain;
        rustc = toolchain;
      };

      # Shared attrs for buildRustPackage — avoids repeating Cargo
      # metadata and build environment across the two package derivations.
      commonPackageAttrs = {
        pname = "stargaze";
        version = "0.1.0";
        src = self;

        cargoLock.lockFile = ./Cargo.lock;

        nativeBuildInputs = [
          pkgs.pkg-config
          pkgs.makeWrapper
        ];

        buildInputs = commonBuildInputs;

        env = bindgenEnv;

        # Tests require runtime resources unavailable in the Nix sandbox
        # (PipeWire, display server, NVIDIA GPU, /dev/uinput).  Run tests
        # via `nix develop -c cargo test` instead.
        doCheck = false;

        # build.rs in each crate calls pkg-config at build time.
        preBuild = bindgenShellHook;
      };

      # Server-specific native deps (PipeWire, dbus for portals, evdev).
      serverBuildInputs = [
        pkgs.pipewire
        pkgs.dbus
      ];

      # Client-specific native deps (SDL2 for rendering + audio playback).
      clientBuildInputs = [
        pkgs.SDL2
      ];

      # Helper: wrap a binary so it finds .so files at runtime.
      wrapBin = { drv, binName, extraLibs ? [] }:
        let
          libPath = pkgs.lib.makeLibraryPath (commonBuildInputs ++ extraLibs);
        in
        drv.overrideAttrs (old: {
          postFixup = (old.postFixup or "") + ''
            wrapProgram $out/bin/${binName} \
              --prefix LD_LIBRARY_PATH : "${libPath}"
          '';
        });

    in
    {
      # ── Dev shells ─────────────────────────────────────────────────

      devShells.${system} = {
        # Default: no CUDA, no unfree packages.
        default = pkgs.mkShell {
          nativeBuildInputs = commonNativeBuildInputs ++ [
            fenix.packages.${system}.rust-analyzer
          ];

          buildInputs = commonBuildInputs;

          env = bindgenEnv;

          shellHook = ''
            ${bindgenShellHook}
            export LD_LIBRARY_PATH="${runtimeLibPath}''${LD_LIBRARY_PATH:+:$LD_LIBRARY_PATH}"
          '';
        };

        # CUDA: extends default with NVIDIA/CUDA packages for NVENC tests.
        # Usage: nix develop .#cuda
        cuda = pkgsCuda.mkShell {
          nativeBuildInputs = commonNativeBuildInputs ++ [
            fenix.packages.${system}.rust-analyzer
            pkgsCuda.cudaPackages.cuda_nvcc
          ];

          buildInputs = commonBuildInputs ++ [
            # Replace ffmpeg with the CUDA-enabled build.
            ffmpegCuda

            # CUDA runtime + toolkit
            pkgsCuda.cudaPackages.cuda_cudart
            pkgsCuda.cudaPackages.cuda_nvml_dev
          ];

          env = bindgenEnv // {
            CUDA_PATH = "${pkgsCuda.cudaPackages.cuda_cudart}";
          };

          shellHook = ''
            ${bindgenShellHook}

            # Runtime paths: include CUDA libs + driver libs alongside the
            # normal project dependencies.
            export LD_LIBRARY_PATH="${
              pkgsCuda.lib.makeLibraryPath [
                ffmpegCuda
                pkgsCuda.pipewire
                pkgsCuda.dbus
                pkgsCuda.SDL2
                pkgsCuda.libopus
                pkgsCuda.cudaPackages.cuda_cudart
                pkgsCuda.cudaPackages.cuda_nvml_dev
              ]
            }''${LD_LIBRARY_PATH:+:$LD_LIBRARY_PATH}"

            # If an NVIDIA driver is installed on the host, add its libs.
            if [ -d /run/opengl-driver/lib ]; then
              export LD_LIBRARY_PATH="/run/opengl-driver/lib:$LD_LIBRARY_PATH"
            fi
          '';
        };
      };

      # ── Packages ───────────────────────────────────────────────────

      packages.${system} = {
        stargaze-server = wrapBin {
          binName = "stargaze-server";
          extraLibs = serverBuildInputs;
          drv = rustPlatform.buildRustPackage (commonPackageAttrs // {
            pname = "stargaze-server";
            cargoBuildFlags = [ "--bin" "stargaze-server" ];

            buildInputs = commonPackageAttrs.buildInputs ++ serverBuildInputs;

            meta = {
              description = "Stargaze streaming server — capture, encode, transport";
              mainProgram = "stargaze-server";
            };
          });
        };

        stargaze-client = wrapBin {
          binName = "stargaze-client";
          extraLibs = clientBuildInputs;
          drv = rustPlatform.buildRustPackage (commonPackageAttrs // {
            pname = "stargaze-client";
            cargoBuildFlags = [ "--bin" "stargaze-client" ];

            buildInputs = commonPackageAttrs.buildInputs ++ clientBuildInputs;

            meta = {
              description = "Stargaze streaming client — decode, render, input forwarding";
              mainProgram = "stargaze-client";
            };
          });
        };

        default = self.packages.${system}.stargaze-server;
      };
    };
}

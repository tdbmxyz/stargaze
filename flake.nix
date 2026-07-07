{
  description = "Stargaze — Rust-native low-latency desktop/game streaming";

  inputs = {
    nixpkgs.url = "github:NixOS/nixpkgs/nixos-unstable";
    fenix = {
      url = "github:nix-community/fenix";
      inputs.nixpkgs.follows = "nixpkgs";
    };
  };

  outputs = {
    self,
    nixpkgs,
    fenix,
  }: let
    system = "x86_64-linux";
    pkgs = import nixpkgs {inherit system;};

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
    # Slim variant for the portable client bundle: decoding needs only
    # libavcodec/libavutil/libswscale (+ VAAPI), and ffmpeg-full drags a
    # ~4 GiB closure (every codec, pango, libcaca, ...) into the AppImage.
    ffmpegHeadless = pkgs.ffmpeg_7-headless;

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
      pkgs.libglvnd
      pkgs.mesa
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
      pkgs.libglvnd
      pkgs.mesa
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
      version = "1.2.7";
      src = self;

      cargoLock.lockFile = ./Cargo.lock;

      nativeBuildInputs = [
        pkgs.pkg-config
        pkgs.makeWrapper
        pkgs.removeReferencesTo
      ];

      buildInputs = commonBuildInputs;

      env = bindgenEnv;

      # Tests require runtime resources unavailable in the Nix sandbox
      # (PipeWire, display server, NVIDIA GPU, /dev/uinput).  Run tests
      # via `nix develop -c cargo test` instead.
      doCheck = false;

      # build.rs in each crate calls pkg-config at build time.
      preBuild = bindgenShellHook;

      # Panic-location strings in the binary embed rust-src store paths,
      # dragging the whole nightly toolchain (~300 MiB) into the runtime
      # closure.  Strip the references; they are display-only.
      postFixup = ''
        find $out/bin -type f -exec remove-references-to -t ${toolchain} {} +
      '';
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

    # SDL2 without the heavy optional runtime deps, for the portable
    # bundle: no PipeWire (audio goes through the PulseAudio API, which
    # PipeWire serves on modern systems including the Steam Deck), no
    # JACK/sndio, and a stub zenity (only used for error dialogs; the
    # real one drags in GTK4 + GStreamer).
    sdl2Slim = pkgs.sdl2-compat.override {
      sdl3 = pkgs.sdl3.override {
        pipewireSupport = false;
        jackSupport = false;
        sndioSupport = false;
        zenity = pkgs.writeShellScriptBin "zenity" "exit 1";
      };
    };

    # Client package, parameterized over the FFmpeg and SDL2 builds: the
    # full ones for local/Nix use, slim ones for the portable AppImage.
    # The client links only FFmpeg, SDL2, opus, and GL — no PipeWire or
    # dbus — so its build inputs and wrapper library path stay minimal.
    mkStargazeClient = {
      ffmpegPkg,
      sdl2Pkg,
      extraWrapFlags ? [],
    }: let
      clientLibs = [
        ffmpegPkg
        sdl2Pkg
        pkgs.libopus
        pkgs.libglvnd
        pkgs.mesa
      ];
    in
      wrapBin {
        binName = "stargaze-client";
        libs = clientLibs;
        inherit extraWrapFlags;
        steamEnvGuard = true;
        drv = rustPlatform.buildRustPackage (commonPackageAttrs
          // {
            pname = "stargaze-client";
            cargoBuildFlags = ["--bin" "stargaze-client"];

            buildInputs =
              clientLibs
              ++ [
                pkgs.libclang
                pkgs.llvmPackages.libclang
              ];

            # Desktop entry + icons so the client shows up in XDG menus;
            # the AppImage bundler also picks these up for its root
            # .desktop/.DirIcon instead of synthesizing a stub.
            postInstall = ''
              install -Dm644 $src/assets/stargaze.desktop \
                $out/share/applications/stargaze.desktop
              install -Dm644 $src/assets/icons/stargaze.svg \
                $out/share/icons/hicolor/scalable/apps/stargaze.svg
              install -Dm644 $src/assets/icons/stargaze-256.png \
                $out/share/icons/hicolor/256x256/apps/stargaze.png
            '';

            meta = {
              description = "Stargaze streaming client — decode, render, input forwarding";
              mainProgram = "stargaze-client";
            };
          });
      };

    # Helper: wrap a binary so it finds .so files at runtime.
    #
    # steamEnvGuard additionally puts a stage-0 `#!/bin/sh` script in
    # front of the (Nix-bash) wrapper. Steam launches non-Steam games
    # with its scout runtime in LD_LIBRARY_PATH and the overlay in
    # LD_PRELOAD; LD_LIBRARY_PATH outranks the DT_RUNPATH Nix binaries
    # resolve their libraries with, so Steam's ancient libs shadow the
    # bundled ones — and that kills the wrapper's OWN bash before any
    # in-script stripping could run ("bash: error while loading shared
    # libraries: libGL.so.1", observed on the Deck in gaming mode). The
    # stage-0 script runs under the HOST /bin/sh (host-linked, immune to
    # the poison — Steam itself runs /bin/sh in this exact environment
    # for launch options), strips exactly the Steam entries (keeping
    # e.g. nixGL paths), and only then execs the Nix wrapper. The host
    # /bin/sh is visible even inside the AppImage: its AppRun overlays
    # only /nix.
    wrapBin = {
      drv,
      binName,
      libs,
      extraWrapFlags ? [],
      steamEnvGuard ? false,
    }: let
      libPath = pkgs.lib.makeLibraryPath libs;
      stage0Script = pkgs.writeScript "stage0-steam-env-guard" ''
        #!/bin/sh
        # Strip Steam's runtime entries from the dynamic-loader
        # environment; everything Nix-linked (including the stage-2
        # wrapper's bash) breaks under them. POSIX sh only.
        sanitize() {
          _acc=""
          IFS=': '
          for _p in $1; do
            case "$_p" in
              *steam-runtime* | *gameoverlay* | */Steam/ubuntu12_32* | */Steam/ubuntu12_64*) ;;
              *) _acc="''${_acc:+$_acc:}$_p" ;;
            esac
          done
          unset IFS
          printf '%s' "$_acc"
        }
        if [ -n "''${LD_LIBRARY_PATH-}" ]; then
          LD_LIBRARY_PATH=$(sanitize "$LD_LIBRARY_PATH")
          export LD_LIBRARY_PATH
        fi
        if [ -n "''${LD_PRELOAD-}" ]; then
          LD_PRELOAD=$(sanitize "$LD_PRELOAD")
          export LD_PRELOAD
        fi
        exec "@stage2@" "$@"
      '';
      stage0 = ''
        mv $out/bin/${binName} $out/bin/.${binName}-stage2
        cp ${stage0Script} $out/bin/${binName}
        sed -i "s|@stage2@|$out/bin/.${binName}-stage2|" $out/bin/${binName}
        chmod 555 $out/bin/${binName}
      '';
    in
      drv.overrideAttrs (old: {
        postFixup =
          (old.postFixup or "")
          + ''
            wrapProgram $out/bin/${binName} \
              --prefix LD_LIBRARY_PATH : "${libPath}" ${pkgs.lib.concatStringsSep " " extraWrapFlags}
          ''
          + pkgs.lib.optionalString steamEnvGuard stage0;
      });
  in {
    # ── Dev shells ─────────────────────────────────────────────────

    devShells.${system} = {
      # Default: no CUDA, no unfree packages.
      default = pkgs.mkShell {
        nativeBuildInputs =
          commonNativeBuildInputs
          ++ [
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
        nativeBuildInputs =
          commonNativeBuildInputs
          ++ [
            fenix.packages.${system}.rust-analyzer
            pkgsCuda.cudaPackages.cuda_nvcc
          ];

        buildInputs =
          commonBuildInputs
          ++ [
            # Replace ffmpeg with the CUDA-enabled build.
            ffmpegCuda

            # CUDA runtime + toolkit
            pkgsCuda.cudaPackages.cuda_cudart
            pkgsCuda.cudaPackages.cuda_nvml_dev
            # Runtime kernel compilation (GPU NV12 converter dlopens libnvrtc).
            # Pinned to CUDA 13 to match cudarc's "cuda-13020" feature.
            pkgsCuda.cudaPackages_13.cuda_nvrtc
          ];

        env =
          bindgenEnv
          // {
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
              pkgsCuda.cudaPackages_13.cuda_nvrtc
              pkgsCuda.libglvnd
              pkgsCuda.mesa
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
      # The server hard-requires CUDA for NVENC encoding.  Use the
      # CUDA-enabled FFmpeg variant so hevc_nvenc is available, pull in
      # the CUDA runtime packages, and add /run/opengl-driver/lib (the
      # NixOS conventional path for the NVIDIA driver's libcuda.so) to
      # the wrapper's LD_LIBRARY_PATH.
      stargaze-server = let
        serverBuildInputs' =
          serverBuildInputs
          ++ [
            ffmpegCuda # CUDA-enabled; provides hevc_nvenc
            pkgs.libopus
            pkgs.libclang
            pkgs.llvmPackages.libclang
            pkgs.libglvnd
            pkgs.mesa
            pkgsCuda.cudaPackages.cuda_cudart # libcuda_cudart.so
            pkgsCuda.cudaPackages.cuda_nvml_dev # libnvidia-ml.so
            pkgsCuda.cudaPackages_13.cuda_nvrtc # libnvrtc.so (GPU NV12 converter)
          ];
        libPath = pkgs.lib.makeLibraryPath serverBuildInputs';
        drv = rustPlatform.buildRustPackage (commonPackageAttrs
          // {
            pname = "stargaze-server";
            cargoBuildFlags = ["--bin" "stargaze-server"];
            buildInputs = serverBuildInputs';
            meta = {
              description = "Stargaze streaming server — capture, encode, transport";
              mainProgram = "stargaze-server";
            };
          });
      in
        drv.overrideAttrs (old: {
          postFixup =
            (old.postFixup or "")
            + ''
              wrapProgram $out/bin/stargaze-server \
                --prefix LD_LIBRARY_PATH : "${libPath}" \
                --suffix LD_LIBRARY_PATH : "/run/opengl-driver/lib"
            '';
        });

      stargaze-client = mkStargazeClient {
        ffmpegPkg = ffmpeg;
        sdl2Pkg = pkgs.SDL2;
      };

      # Same client built against headless FFmpeg and slim SDL2 with a
      # minimal runtime library set.  Used by the release workflow to
      # produce a reasonably sized self-contained AppImage (`nix bundle`)
      # for non-Nix machines (e.g. a Steam Deck).  The --set-default
      # wrapper flags point Mesa/libva at the bundled drivers on systems
      # without /run/opengl-driver (any non-NixOS host); on NixOS or when
      # the user already set them, they are left untouched.
      stargaze-client-portable = mkStargazeClient {
        ffmpegPkg = ffmpegHeadless;
        sdl2Pkg = sdl2Slim;
        extraWrapFlags = [
          "--set-default LIBVA_DRIVERS_PATH ${pkgs.mesa}/lib/dri"
          "--set-default LIBGL_DRIVERS_PATH ${pkgs.mesa}/lib/dri"
          "--set-default __EGL_VENDOR_LIBRARY_DIRS ${pkgs.mesa}/share/glvnd/egl_vendor.d"
        ];
      };

      default = self.packages.${system}.stargaze-server;
    };

    # Permission setup for the built-in USB forwarding (Steam Controller
    # hardware tunneled over the session connection). Import the client
    # module on the machine running stargaze-client, the server module on
    # the host, and set services.stargaze.usbClient/usbServer.{enable,users}.
    nixosModules = {
      usb-client = import ./nix/usb-client.nix;
      usb-server = import ./nix/usb-server.nix;
    };
  };
}

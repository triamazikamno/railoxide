{
  description = "RailOxide — Desktop wallet for RAILGUN private transactions";

  inputs = {
    nixpkgs.url = "github:NixOS/nixpkgs/nixpkgs-unstable";
    flake-utils.url = "github:numtide/flake-utils";
    rust-overlay = {
      url = "github:oxalica/rust-overlay";
      inputs.nixpkgs.follows = "nixpkgs";
    };
  };

  outputs = { self, nixpkgs, flake-utils, rust-overlay }:
    flake-utils.lib.eachDefaultSystem (system:
      let
        overlays = [ (import rust-overlay) ];
        pkgs = import nixpkgs { inherit system overlays; };

        rustToolchain = pkgs.rust-bin.stable.latest.default.override {
          extensions = [ "rust-src" "rust-analyzer" ];
          targets = [ "wasm32-unknown-unknown" ];
        };

        rustPlatform = pkgs.makeRustPlatform {
          cargo = rustToolchain;
          rustc = rustToolchain;
        };

        isLinux = pkgs.stdenv.isLinux;
        isDarwin = pkgs.stdenv.isDarwin;

        extensionTools = builtins.fromJSON (builtins.readFile ./scripts/browser-extension-tools.json);
        bindgenTarget = {
          x86_64-linux = "x86_64-unknown-linux-musl";
          aarch64-linux = "aarch64-unknown-linux-musl";
          x86_64-darwin = "x86_64-apple-darwin";
          aarch64-darwin = "aarch64-apple-darwin";
        }.${system};
        wasmBindgen = pkgs.stdenvNoCC.mkDerivation {
          pname = "wasm-bindgen-cli";
          version = extensionTools.version;
          src = pkgs.fetchurl {
            url = "https://github.com/wasm-bindgen/wasm-bindgen/releases/download/${extensionTools.version}/wasm-bindgen-${extensionTools.version}-${bindgenTarget}.tar.gz";
            sha256 = extensionTools.targets.${bindgenTarget};
          };
          dontStrip = true;
          installPhase = ''
            install -Dm755 wasm-bindgen $out/bin/wasm-bindgen
            install -Dm644 LICENSE-MIT $out/share/licenses/wasm-bindgen/LICENSE-MIT
            install -Dm644 LICENSE-APACHE $out/share/licenses/wasm-bindgen/LICENSE-APACHE
          '';
        };

        walletCargoToml = builtins.fromTOML (builtins.readFile ./bins/wallet/Cargo.toml);

        linuxBuildInputs = with pkgs; [
          openssl
          sqlite
          fontconfig
          libxkbcommon
          libx11
          libxcursor
          libxi
          libxrandr
          libxcb
          wayland
          vulkan-loader
          alsa-lib
          libGL
          zstd
          libusb1
          eudev
          hidapi
        ];

        darwinBuildInputs = with pkgs; [
          apple-sdk
        ];

        nativeBuildInputs = with pkgs; [
          pkg-config
          cmake
          clang
          libclang.lib
          rustPlatform.bindgenHook
          makeWrapper
          python3
        ] ++ [ wasmBindgen ] ++ (if isLinux then [
          wayland-protocols
          libxkbcommon
        ] else []);

      in
      {
        packages.default = rustPlatform.buildRustPackage {
          pname = "railoxide";
          version = walletCargoToml.package.version;

          src = ./.;

          cargoLock = {
            lockFile = ./Cargo.lock;
            allowBuiltinFetchGit = true;
          };

          cargoBuildFlags = [ "-p" "wallet" ];

          buildFeatures = [ "hardware" ];

          inherit nativeBuildInputs;

          buildInputs =
            (if isLinux then linuxBuildInputs else []) ++
            (if isDarwin then darwinBuildInputs else []);

          LIBCLANG_PATH = "${pkgs.libclang.lib}/lib";

          preBuild = ''
            CARGO_TOOLCHAIN= CARGO_NET_OFFLINE=true scripts/build-browser-extension
            export RAILOXIDE_EXTENSION_BUNDLE="$PWD/target/browser-extension.zip"
          '';

          postInstall = ''
            mv $out/bin/wallet $out/bin/railoxide
            python3 scripts/verify-browser-extension.py $out/bin/railoxide "$RAILOXIDE_EXTENSION_BUNDLE"
          '';

          postFixup = pkgs.lib.optionalString isLinux ''
            wrapProgram $out/bin/railoxide \
              --prefix LD_LIBRARY_PATH : "${pkgs.lib.makeLibraryPath (linuxBuildInputs ++ [ pkgs.vulkan-loader ])}"
          '';

          doCheck = false;

          meta = with pkgs.lib; {
            description = "Desktop wallet for RAILGUN private transactions";
            homepage = "https://github.com/triamazikamno/railoxide";
            license = licenses.mit;
            mainProgram = "railoxide";
            platforms = [ "x86_64-linux" "aarch64-linux" "x86_64-darwin" "aarch64-darwin" ];
          };
        };

        devShells.default = pkgs.mkShell {
          packages = [
            rustToolchain
            pkgs.pkg-config
            pkgs.cmake
            pkgs.clang
            pkgs.libclang.lib
            pkgs.python3
            wasmBindgen
          ] ++ (if isLinux then (with pkgs; [
            openssl
            sqlite
            fontconfig
            libxkbcommon
            libx11
            libxcursor
            libxi
            libxrandr
            libxcb
            wayland
            wayland-protocols
            vulkan-loader
            alsa-lib
            libGL
            zstd
            libusb1
            eudev
            hidapi
          ]) else if isDarwin then darwinBuildInputs else []);

          LIBCLANG_PATH = "${pkgs.libclang.lib}/lib";

          CARGO_TOOLCHAIN = "";

          shellHook = ''
            echo "RailOxide dev shell"
            echo "Rust: $(rustc --version)"
            echo "Cargo: $(cargo --version)"
          '';
        };
      }
    );
}

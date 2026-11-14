{
  description = "Koma - agent-native Wayland tiling compositor workspace";

  inputs = {
    nixpkgs.url = "github:NixOS/nixpkgs/nixpkgs-unstable";
    flake-utils.url = "github:numtide/flake-utils";
    rust-overlay = {
      url = "github:oxalica/rust-overlay";
      inputs.nixpkgs.follows = "nixpkgs";
    };
    crane.url = "github:ipetkov/crane";
  };

  outputs = { self, nixpkgs, flake-utils, rust-overlay, crane }:
    let
      supportedSystems = [
        "x86_64-linux"
        "aarch64-linux"
      ];

      perSystem = flake-utils.lib.eachSystem supportedSystems (system:
        let
          pkgs = import nixpkgs {
            inherit system;
            overlays = [ (import rust-overlay) ];
          };

          rustToolchain = pkgs.rust-bin.fromRustupToolchainFile ./rust-toolchain.toml;
          craneLib = (crane.mkLib pkgs).overrideToolchain rustToolchain;

          # Keep `examples/init.lua` (embedded by `koma-lua`), `resources/`
          # (session wrapper, desktop entry, systemd units, portal routing),
          # and `LICENSE` alongside Cargo sources.
          src = pkgs.lib.fileset.toSource {
            root = ./.;
            fileset = pkgs.lib.fileset.unions [
              (craneLib.fileset.commonCargoSources ./.)
              ./examples/init.lua
              ./resources
              ./LICENSE
            ];
          };

          cargoToml = builtins.fromTOML (builtins.readFile ./Cargo.toml);
          version = cargoToml.workspace.package.version or "0.0.1";

          nativeBuildInputs = with pkgs; [
            pkg-config
            makeWrapper
          ];

          runtimeLibs = with pkgs; [
            seatd
            libinput
            udev
            libxkbcommon
            libdrm
            mesa
            libglvnd
            wayland
            pixman
            dbus
            fontconfig
          ] ++ pkgs.lib.optional (pkgs ? libgbm) pkgs.libgbm;

          commonArgs = {
            inherit src version nativeBuildInputs;
            pname = "koma";
            strictDeps = true;
            buildInputs = runtimeLibs;
            cargoExtraArgs = "--locked -p koma -p komactl -p koma-mcp -p koma-overview";
          };

          cargoArtifacts = craneLib.buildDepsOnly commonArgs;

          koma = craneLib.buildPackage (commonArgs // {
            inherit cargoArtifacts;
            doCheck = false;

            postInstall = ''
              install -Dm755 resources/koma-session "$out/bin/koma-session"
              wrapProgram "$out/bin/koma-session" \
                --prefix PATH : "$out/bin"

              install -Dm644 resources/koma.desktop "$out/share/wayland-sessions/koma.desktop"
              install -Dm644 resources/koma.service "$out/lib/systemd/user/koma.service"
              substituteInPlace "$out/lib/systemd/user/koma.service" \
                --replace-fail "/usr/bin/koma" "$out/bin/koma"
              install -Dm644 resources/koma-shutdown.target "$out/lib/systemd/user/koma-shutdown.target"
              install -Dm644 resources/koma-portals.conf "$out/share/xdg-desktop-portal/koma-portals.conf"

              install -Dm644 examples/init.lua "$out/share/doc/koma/examples/init.lua"
              install -Dm644 resources/greetd-config.toml "$out/share/doc/koma/examples/greetd-config.toml"
              install -Dm644 LICENSE "$out/share/licenses/koma/LICENSE"
            '';

            postFixup = ''
              rpath="${pkgs.lib.makeLibraryPath runtimeLibs}"
              for bin in "$out/bin/koma" "$out/bin/komactl" "$out/bin/koma-mcp" "$out/bin/koma-overview"; do
                if [ -x "$bin" ]; then
                  existing="$(patchelf --print-rpath "$bin" || true)"
                  if [ -n "$existing" ]; then
                    patchelf --set-rpath "$existing:$rpath" "$bin"
                  else
                    patchelf --set-rpath "$rpath" "$bin"
                  fi
                fi
              done
            '';

            passthru.providedSessions = [ "koma" ];

            meta = with pkgs.lib; {
              description = "Agent-native Wayland tiling compositor";
              homepage = "https://github.com/komawm/koma";
              license = licenses.gpl3Plus;
              mainProgram = "koma";
              platforms = platforms.linux;
            };
          });
        in
        {
          packages = {
            default = koma;
            koma = koma;
          };

          apps = {
            default = flake-utils.lib.mkApp {
              drv = koma;
            };
            koma = flake-utils.lib.mkApp {
              drv = koma;
            };
          };

          checks = {
            inherit koma;
          };

          devShells.default = craneLib.devShell {
            checks = self.checks.${system};
            packages = with pkgs; [
              pkg-config
              just
              cargo-deny
              actionlint
              shellcheck
              python3
              wayland-scanner
            ] ++ runtimeLibs;

            LD_LIBRARY_PATH = pkgs.lib.makeLibraryPath runtimeLibs;
          };

          formatter = pkgs.nixpkgs-fmt;
        });

      komaModule = { config, lib, pkgs, ... }:
        let
          cfg = config.programs.koma;
        in
        {
          options.programs.koma = {
            enable = lib.mkEnableOption "Koma, an agent-native Wayland tiling compositor";

            package = lib.mkOption {
              type = lib.types.package;
              default = self.packages.${pkgs.stdenv.hostPlatform.system}.koma;
              defaultText = lib.literalExpression "inputs.koma.packages.\${pkgs.stdenv.hostPlatform.system}.koma";
              description = "The Koma package to install.";
            };

            xwayland.enable = lib.mkOption {
              type = lib.types.bool;
              default = true;
              description = "Whether to provide xwayland-satellite for X11 client support.";
            };
          };

          config = lib.mkIf cfg.enable {
            environment.systemPackages = [ cfg.package ]
              ++ lib.optional (cfg.xwayland.enable && (pkgs ? xwayland-satellite)) pkgs.xwayland-satellite;

            services.displayManager.sessionPackages = [ cfg.package ];
            systemd.packages = [ cfg.package ];

            hardware.graphics.enable = lib.mkDefault true;
            security.polkit.enable = lib.mkDefault true;

            xdg.portal = {
              enable = lib.mkDefault true;
              extraPortals = lib.mkDefault [ pkgs.xdg-desktop-portal-gtk ];
              configPackages = lib.mkDefault [ cfg.package ];
            };
          };
        };
    in
    perSystem // {
      nixosModules = {
        default = komaModule;
        koma = komaModule;
      };
    };
}

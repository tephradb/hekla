{
  description = "hekla: a single-app event-sourcing runtime over the Dynamic Consistency Boundary";

  inputs.nixpkgs.url = "github:NixOS/nixpkgs/nixos-unstable";

  outputs =
    { self, nixpkgs }:
    let
      systems = [
        "x86_64-linux"
        "aarch64-linux"
        "aarch64-darwin"
        "x86_64-darwin"
      ];
      forEachSystem = f: nixpkgs.lib.genAttrs systems (system: f nixpkgs.legacyPackages.${system});
      version = (builtins.fromTOML (builtins.readFile ./Cargo.toml)).package.version;
    in
    {
      packages = forEachSystem (pkgs: rec {
        default = hekla;

        # The runtime and its CLI. `AUTHORING.md` is the contract for what it runs.
        #
        # `src = self` is the whole repository as git has it, which is the point: the working
        # tree carries a multi-gigabyte `target/`, and a flake's source is the tracked files
        # alone. The admin console under `ui/` reaches the binary through `include_bytes!`,
        # so it has to be tracked to be built, which it is.
        #
        # rusqlite is `bundled`, so this compiles SQLite from C and needs a compiler rather
        # than a system sqlite; stdenv already carries one, and nothing here needs
        # pkg-config or openssl (ureq speaks rustls).
        hekla = pkgs.rustPlatform.buildRustPackage {
          pname = "hekla";
          inherit version;

          src = self;
          cargoLock.lockFile = ./Cargo.lock;

          # The suite is `nix flake check` below, and in the repository it is a development
          # loop that runs on every change. Building the binary should not pay for it twice.
          doCheck = false;

          meta = {
            description = "A single-app event-sourcing runtime you write in heklang";
            mainProgram = "hekla";
            license = with pkgs.lib.licenses; [
              mit
              asl20
            ];
            platforms = systems;
          };
        };
      });

      # `nix flake check` runs the whole suite, which the package deliberately does not. The
      # integration tests read `examples/` and `tests/fixtures/` from the source tree through
      # `CARGO_MANIFEST_DIR`, so they need `src` to be the repository, which it is.
      checks = forEachSystem (pkgs: {
        tests = self.packages.${pkgs.stdenv.hostPlatform.system}.hekla.overrideAttrs (_: {
          pname = "hekla-tests";
          doCheck = true;
        });
      });
    };
}

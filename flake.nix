{
  description = "Tiny native coding agent for the terminal, powered by Kimi or Gemini";

  inputs.nixpkgs.url = "github:NixOS/nixpkgs/nixos-unstable";

  outputs =
    { self, nixpkgs }:
    let
      systems = [
        "x86_64-linux"
        "aarch64-linux"
        "x86_64-darwin"
        "aarch64-darwin"
      ];
      forAllSystems = f: nixpkgs.lib.genAttrs systems (system: f nixpkgs.legacyPackages.${system});
      manifest = (builtins.fromTOML (builtins.readFile ./Cargo.toml)).package;
    in
    {
      packages = forAllSystems (pkgs: {
        default = pkgs.rustPlatform.buildRustPackage {
          pname = "odei";
          version = manifest.version;
          src = self;
          cargoLock.lockFile = ./Cargo.lock;
          preCheck = ''
            export HOME=$(mktemp -d)
          '';
          meta = {
            description = manifest.description;
            homepage = "https://github.com/enekos/odei";
            license = pkgs.lib.licenses.mit;
            mainProgram = "odei";
          };
        };
      });
    };
}

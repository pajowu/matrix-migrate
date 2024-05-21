{
  description = "Tool to migrate matrix accounts";

  inputs.nixpkgs.url = "github:NixOS/nixpkgs/nixos-unstable";
  inputs.flake-utils.url = "github:numtide/flake-utils";

  outputs = { self, nixpkgs, flake-utils }: {
    overlays.default = final: prev: rec {
      matrix-migrate = final.callPackage (
        { rustPlatform, pkg-config, openssl, sqlite }:

        rustPlatform.buildRustPackage {
          pname = "matrix-migrate";
          version = self.shortRev or "dirty-${toString self.lastModifiedDate}";

          src = self;

          nativeBuildInputs = [ pkg-config ];
          buildInputs = [ openssl sqlite ];

          cargoLock.lockFile = ./Cargo.lock;
        }
      ) {};
      default = matrix-migrate;
    };
  } //
    flake-utils.lib.eachDefaultSystem (system:
    let
      pkgs = import nixpkgs {
        inherit system;
        overlays = builtins.attrValues self.overlays;
      };
    in {
      packages = {
        inherit (pkgs) matrix-migrate default;
      };
    });
}

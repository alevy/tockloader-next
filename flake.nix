{
  description = "Install and inspect Tock applications on a board";

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
      # No flake-utils: this is three lines and one fewer input to keep
      # current.
      forAllSystems = f: nixpkgs.lib.genAttrs systems (system: f system nixpkgs.legacyPackages.${system});
    in
    {
      packages = forAllSystems (
        system: pkgs: rec {
          default = tockloader-next;
          tockloader-next = pkgs.callPackage ./package.nix { };
          # The same thing with a known-good openocd on the transport's
          # PATH, for a board reached over a debug probe.
          with-openocd = pkgs.callPackage ./package.nix { withOpenocd = true; };
        }
      );

      devShells = forAllSystems (
        system: pkgs: {
          default = pkgs.mkShell {
            # rustc and cargo come from the package itself, so the shell
            # and the build can never disagree about the toolchain.
            inputsFrom = [ self.packages.${system}.default ];
            packages = with pkgs; [
              clippy
              rustfmt
              rust-analyzer
              # For a board on the desk, and for the openocd transport's
              # own tests.
              openocd
            ];
          };
        }
      );

      # `nix flake check` builds the package, which runs the whole
      # workspace test suite in its check phase.
      checks = forAllSystems (
        system: _: {
          inherit (self.packages.${system}) tockloader-next;
        }
      );

      formatter = forAllSystems (_: pkgs: pkgs.nixfmt);
    };
}

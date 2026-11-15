# `nix-build`, `nix-env -i -f .`, and anything pinning this repo with niv
# or npins. The flake is the other door onto the same `package.nix`.
#
# This one deliberately takes nixpkgs from the caller's channel rather
# than pinning: it is the entry point for people who have not turned
# flakes on, and a pin here would be a second thing to keep current
# alongside `flake.lock`. If you want the exact nixpkgs this is tested
# against, use the flake.
{
  pkgs ? import <nixpkgs> { },
  withOpenocd ? false,
}:

pkgs.callPackage ./package.nix { inherit withOpenocd; }

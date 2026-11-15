# What the package *is*, once.
#
# `default.nix` and `flake.nix` are both thin doors onto this file, so
# there is one definition of the build and two ways to ask for it rather
# than two definitions that drift.
{
  lib,
  rustPlatform,
  makeWrapper,
  openocd,
  python3,
  # Put `openocd` on the openocd transport's own PATH.
  #
  # Off by default: reaching a board over the file or serial transport
  # should not drag a debug server into the closure, and anyone doing JTAG
  # work already has an openocd -- often a vendor fork that a pinned one
  # would quietly shadow. `--opt openocd.command=<path>` names a specific
  # one whichever way this is built.
  withOpenocd ? false,
}:

rustPlatform.buildRustPackage {
  pname = "tockloader-next";
  inherit ((lib.importTOML ./Cargo.toml).workspace.package) version;

  # Only what the build actually reads. `boards/` is in here because the
  # bundled board files are `include_str!`d into tock-board at compile
  # time -- they are a build input, not something to install.
  src = lib.fileset.toSource {
    root = ./.;
    fileset = lib.fileset.unions [
      ./Cargo.toml
      ./Cargo.lock
      ./crates
      ./boards
    ];
  };

  # The lockfile is committed, so there is no vendor hash to regenerate
  # every time a dependency moves.
  cargoLock.lockFile = ./Cargo.lock;

  nativeBuildInputs = lib.optional withOpenocd makeWrapper;

  # One test stands in a subcommand written in Python, to prove a
  # subcommand needs nothing of ours but the fd and the frame layout. It
  # is the whole point of that test that the interpreter is not Rust.
  nativeCheckInputs = [ python3 ];

  # Wrapping the transport rather than `tock` is deliberate. `tock` finds
  # its subcommands beside its own executable (see `external.rs`), and
  # `wrapProgram` renames the real binary out of the way -- doing that to
  # `tock` itself would be asking for trouble, while the hidden
  # `.tock-transport-openocd-wrapped` is invisible to that search because
  # it no longer starts with `tock-`.
  postInstall = lib.optionalString withOpenocd ''
    wrapProgram $out/bin/tock-transport-openocd \
      --prefix PATH : ${lib.makeBinPath [ openocd ]}
  '';

  meta = {
    description = "Install and inspect Tock applications on a board";
    longDescription = ''
      A rewrite of Tock's tockloader. `tock` resolves which board, which
      transport and where apps go, then hands off to a `tock-<name>`
      subcommand over an open connection. Transports are separate
      executables speaking a framed protocol over stdio, so a new one need
      not be written in Rust.
    '';
    homepage = "https://github.com/alevy/tockloader-next";
    license = with lib.licenses; [
      asl20
      mit
    ];
    mainProgram = "tock";
    # fd 3 handoff, unix sockets and termios throughout.
    platforms = lib.platforms.unix;
  };
}

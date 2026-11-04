{
  lib,
  rustPlatform,
  tmux,
  ps,
}:
let
  cargo = lib.importTOML ../Cargo.toml;
in
rustPlatform.buildRustPackage {
  pname = "tmux-agent";
  inherit (cargo.package) version;

  src = lib.fileset.toSource {
    root = ../.;
    fileset = lib.fileset.unions [
      ../Cargo.toml
      ../Cargo.lock
      ../src
      (lib.fileset.maybeMissing ../tests)
    ];
  };
  cargoLock.lockFile = ../Cargo.lock;

  # Integration tests run private tmux servers; the background-session tests
  # read the process table, which the Linux build sandbox has no ps for.
  nativeCheckInputs = [
    tmux
    ps
  ];
  __darwinAllowLocalNetworking = true;

  meta = {
    description = cargo.package.description;
    license = with lib.licenses; [ mit asl20 ];
    mainProgram = "tmux-agent";
    platforms = lib.platforms.unix;
  };
}

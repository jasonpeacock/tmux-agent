{
  lib,
  rustPlatform,
  tmux,
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

  # Integration tests run private tmux servers.
  nativeCheckInputs = [ tmux ];
  __darwinAllowLocalNetworking = true;

  meta = {
    description = cargo.package.description;
    license = with lib.licenses; [ mit asl20 ];
    mainProgram = "tmux-agent";
    platforms = lib.platforms.unix;
  };
}

{
  lib,
  stdenv,
  rustPlatform,
  makeWrapper,
  nix,
  libnotify,
  terminal-notifier,
}:

# Threshold-triggered cleanup for safe Nix and Rust artifacts. Packaged with
# its own runtime PATH (host nix + platform notifier) so systemd timers,
# launchd daemons and interactive shells invoke the exact same binary.
let
  runtimeBinPaths = [
    nix
  ]
  ++ lib.optional stdenv.hostPlatform.isLinux libnotify
  ++ lib.optional stdenv.hostPlatform.isDarwin terminal-notifier;
in
rustPlatform.buildRustPackage {
  pname = "disk-cleanup";
  version = "0.1.0";

  src = lib.fileset.toSource {
    root = ./.;
    fileset = lib.fileset.unions [
      ./Cargo.toml
      ./Cargo.lock
      ./src
      ./tests
    ];
  };

  cargoLock.lockFile = ./Cargo.lock;

  nativeBuildInputs = [
    makeWrapper
  ];

  postInstall = ''
    wrapProgram $out/bin/disk-cleanup \
      --prefix PATH : ${lib.makeBinPath runtimeBinPaths}
  '';

  meta = {
    description = "Threshold-triggered cleanup for safe Nix and Rust build artifacts";
    platforms = lib.platforms.unix;
    mainProgram = "disk-cleanup";
  };
}

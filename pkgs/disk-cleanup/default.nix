{
  lib,
  stdenvNoCC,
  python3,
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
  ++ lib.optional stdenvNoCC.hostPlatform.isLinux libnotify
  ++ lib.optional stdenvNoCC.hostPlatform.isDarwin terminal-notifier;
in
stdenvNoCC.mkDerivation {
  pname = "disk-cleanup";
  version = "0.1.0";

  src = ./.;

  nativeBuildInputs = [
    makeWrapper
    python3
  ];

  doCheck = true;
  checkPhase = ''
    runHook preCheck
    python3 -m unittest discover -s . -v
    runHook postCheck
  '';

  installPhase = ''
    runHook preInstall
    install -Dm755 disk_cleanup.py $out/libexec/disk-cleanup/disk_cleanup.py
    mkdir -p $out/bin
    makeWrapper ${lib.getExe python3} $out/bin/disk-cleanup \
      --add-flags "$out/libexec/disk-cleanup/disk_cleanup.py" \
      --prefix PATH : ${lib.makeBinPath runtimeBinPaths}
    runHook postInstall
  '';

  meta = {
    description = "Threshold-triggered cleanup for safe Nix and Rust build artifacts";
    platforms = lib.platforms.unix;
    mainProgram = "disk-cleanup";
  };
}

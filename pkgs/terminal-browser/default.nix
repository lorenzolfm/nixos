{
  lib,
  stdenv,
  fetchurl,
  autoPatchelfHook,
  alsa-lib,
  at-spi2-atk,
  at-spi2-core,
  atk,
  cairo,
  cups,
  dbus,
  expat,
  glib,
  gtk3,
  libgbm,
  libGL,
  libx11,
  libxcb,
  libxcomposite,
  libxdamage,
  libxext,
  libxfixes,
  libxkbcommon,
  libxrandr,
  nspr,
  nss,
  pango,
  systemd,
}:

# zenbu-labs/terminal-browser: Chromium (through Electron) drawn into the
# terminal over the kitty graphics protocol. Not in nixpkgs; the Macs could
# take the Homebrew cask, but the desktop cannot, so all three use the
# upstream release tarball. `terminal-browser upgrade` cannot write to the
# store: bump version and hashes here instead.
let
  version = "0.16.3";
  sources = {
    aarch64-darwin = {
      asset = "darwin-arm64";
      hash = "sha256-QuiUpBH1TKmlUjf2xlTJx6kPRZZv901NmzauDWCNvoc=";
    };
    x86_64-linux = {
      asset = "linux-x64";
      hash = "sha256-tmGZ9H7BiwCkdCPMgJXcBomOrtDE8keiUs/OdjiOoVE=";
    };
  };
  source =
    sources.${stdenv.hostPlatform.system}
      or (throw "terminal-browser: no release for ${stdenv.hostPlatform.system}");
in
stdenv.mkDerivation {
  pname = "terminal-browser";
  inherit version;

  src = fetchurl {
    url = "https://github.com/zenbu-labs/terminal-browser/releases/download/v${version}/terminal-browser-${source.asset}.tar.gz";
    inherit (source) hash;
  };

  sourceRoot = "terminal-browser";

  nativeBuildInputs = lib.optionals stdenv.hostPlatform.isLinux [ autoPatchelfHook ];

  buildInputs = lib.optionals stdenv.hostPlatform.isLinux [
    alsa-lib
    at-spi2-atk
    at-spi2-core
    atk
    cairo
    cups
    dbus
    expat
    glib
    gtk3
    libgbm
    libx11
    libxcb
    libxcomposite
    libxdamage
    libxext
    libxfixes
    libxkbcommon
    libxrandr
    nspr
    nss
    pango
    stdenv.cc.cc.lib
    systemd
  ];

  # Chromium dlopens these rather than linking them.
  runtimeDependencies = lib.optionals stdenv.hostPlatform.isLinux [
    libGL
    systemd
  ];

  dontConfigure = true;
  dontBuild = true;
  # The Mac binaries carry Developer ID signatures, which stripping or any
  # other fixup would break. macOS keys their Accessibility grant to that
  # signature, so it survives the store path changing on each bump.
  dontFixup = stdenv.hostPlatform.isDarwin;

  # bin/terminal-browser is a shell script that resolves its own symlinks and
  # finds the Electron runtime relative to the tree, so the tree stays whole.
  # The tarballs are made on a Mac and carry AppleDouble ._* entries. macOS
  # tar folds those back into extended attributes; GNU tar writes them out as
  # files, and those extra files break the app bundle's sealed signature.
  installPhase = ''
    runHook preInstall
    find . -name '._*' -delete
    mkdir -p $out/libexec $out/bin
    cp -r . $out/libexec/terminal-browser
    ln -s ../libexec/terminal-browser/bin/terminal-browser $out/bin/terminal-browser
    runHook postInstall
  '';

  meta = {
    description = "A real browser inside your terminal";
    homepage = "https://github.com/zenbu-labs/terminal-browser";
    license = lib.licenses.mit;
    sourceProvenance = [ lib.sourceTypes.binaryNativeCode ];
    platforms = builtins.attrNames sources;
    mainProgram = "terminal-browser";
  };
}

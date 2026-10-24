{
  lib,
  stdenvNoCC,
  fetchurl,
  autoPatchelfHook,
  makeWrapper,
  versionCheckHook,
}:

# Replaces `curl -fsSL https://numes.ai/install | sh`. The installer fetches
# this same tarball, checks it against the release SHA256SUMS (the hash below
# is that entry), then runs `numes install` to copy itself into a
# self-updating layout under ~/.local. Here the store path is the install,
# and a version bump is a new `version` and `hash`.
stdenvNoCC.mkDerivation (finalAttrs: {
  pname = "numes";
  version = "0.0.34";

  src = fetchurl {
    url = "https://releases.numes.ai/numes/${finalAttrs.version}/numes-${finalAttrs.version}-linux-x64.tar.gz";
    hash = "sha256-GyrhcAsNvHWGhjyLOihDTcQbMAUQB2GHKV91RxkL7r0=";
  };

  sourceRoot = ".";

  nativeBuildInputs = [
    autoPatchelfHook
    makeWrapper
  ];

  # A Bun single-file executable: the JS bundle lives in an ELF `.bun`
  # section, which patchelf keeps but strip would not be trusted with.
  dontStrip = true;

  # Background services otherwise replace the binary with a newer release
  # on their own. With auto-apply off they only report it.
  installPhase = ''
    runHook preInstall
    install -Dm755 numes $out/libexec/numes
    makeWrapper $out/libexec/numes $out/bin/numes \
      --set-default NUMES_AUTO_UPDATE 0
    runHook postInstall
  '';

  doInstallCheck = true;
  nativeInstallCheckInputs = [ versionCheckHook ];
  versionCheckProgramArg = "version";

  meta = {
    description = "Numes command line: coding agents on Numes models and inference providers";
    homepage = "https://numes.ai";
    license = lib.licenses.unfree;
    sourceProvenance = [ lib.sourceTypes.binaryNativeCode ];
    platforms = [ "x86_64-linux" ];
    mainProgram = "numes";
  };
})

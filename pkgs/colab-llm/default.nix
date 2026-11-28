{
  lib,
  python3Packages,
  google-colab-cli,
  openssh,
  mypy,
  ruff,
  shellcheck,
}:
python3Packages.buildPythonApplication {
  pname = "colab-llm";
  version = "0.1.0";
  pyproject = true;

  src = lib.cleanSource ./.;

  build-system = with python3Packages; [ hatchling ];

  postInstall = ''
    mkdir -p "$out/share/colab-llm/engines"
    cp -r ${./engines}/* "$out/share/colab-llm/engines/"
    # Keep remote VM bootstrap scripts non-executable in the Nix store so
    # patchShebangs never rewrites their #!/usr/bin/env headers to /nix/store/...
    # (they are uploaded to Ubuntu Colab VMs and chmod 700'd remotely).
    chmod 0644 "$out/share/colab-llm/engines"/*.sh "$out/share/colab-llm/engines/deadman.py"
  '';

  makeWrapperArgs = [
    "--prefix"
    "PATH"
    ":"
    (lib.makeBinPath [
      google-colab-cli
      openssh
    ])
    "--set"
    "COLAB_LLM_COLAB_BIN"
    "${google-colab-cli}/bin/colab"
    "--set"
    "COLAB_LLM_SSH_BIN"
    "${openssh}/bin/ssh"
    "--set"
    "COLAB_LLM_ENGINES_DIR"
    "${placeholder "out"}/share/colab-llm/engines"
  ];

  nativeCheckInputs = [
    python3Packages.pytestCheckHook
    mypy
    ruff
    shellcheck
  ];
  enabledTestPaths = [ "tests" ];

  postCheck = ''
    shellcheck engines/*.sh
    ruff check colab_llm engines/deadman.py tests
    # Without a checker the sum types and `assert_never` branches are comments.
    mypy --strict colab_llm
    HOME="$TMPDIR" "$out/bin/colab-llm" --help > /dev/null
    HOME="$TMPDIR" "$out/bin/colab-llm" engines > /dev/null
  '';

  pythonImportsCheck = [
    "colab_llm"
    "colab_llm.cli"
    "colab_llm.colab"
    "colab_llm.config"
    "colab_llm.engines"
    "colab_llm.proxy"
    "colab_llm.state"
  ];

  meta = {
    description = "On-demand LLM inference launcher for Google Colab G4 runtimes";
    mainProgram = "colab-llm";
    platforms = lib.platforms.unix;
  };
}

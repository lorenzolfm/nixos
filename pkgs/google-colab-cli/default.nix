{
  lib,
  stdenv,
  python3Packages,
  fetchFromGitHub,
  fetchurl,
  openssh,
}:
let
  jupyter-mimetypes = python3Packages.buildPythonPackage {
    pname = "jupyter-mimetypes";
    version = "0.2.0";
    format = "wheel";

    src = fetchurl {
      url = "https://files.pythonhosted.org/packages/72/45/cb4671e13fed39f721066ad1a00714d4b607982b8d3e97a25f836198d1df/jupyter_mimetypes-0.2.0-py3-none-any.whl";
      hash = "sha256-5tzZiSWOP8lENltlbZFzGRUX4OOTvYeOl85QDls4hSc=";
    };

    dependencies = with python3Packages; [
      pyarrow
      typing-extensions
    ];

    pythonImportsCheck = [ "jupyter_mimetypes" ];
  };

  # Fork pinned in google-colab-cli's uv.lock.
  jupyter-kernel-client = python3Packages.buildPythonPackage {
    pname = "jupyter-kernel-client";
    version = "0.8.0";
    pyproject = true;

    src = fetchFromGitHub {
      owner = "googlecolab";
      repo = "jupyter-kernel-client";
      rev = "f18e982c3265df5e923aa9def101ab3fd737e139";
      hash = "sha256-A2c78qPdY5HIdAmcr1PP3UbtMty84zrNnvZItu7Rk+E=";
    };

    build-system = with python3Packages; [ hatchling ];

    dependencies = with python3Packages; [
      jupyter-client
      jupyter-core
      jupyter-mimetypes
      requests
      traitlets
      typing-extensions
      websocket-client
    ];

    # Upstream tests require a live jupyter-server; verify import integrity here.
    doCheck = false;
    pythonImportsCheck = [ "jupyter_kernel_client" ];
  };
in
# Official Google Colab CLI (post-v0.8.0 main with `colab stop --all`,
# `colab ssh --force`, `colab logout`, and Contents API timeouts).
python3Packages.buildPythonApplication {
  pname = "google-colab-cli";
  version = "0.8.1.dev16";
  pyproject = true;

  src = fetchFromGitHub {
    owner = "googlecolab";
    repo = "google-colab-cli";
    rev = "7370b25f953a3f9a29f925a85607359f2f05d749";
    hash = "sha256-URGnPKcqiVPyai1F4BIoJnBkiu2XEJ6iCYGEIE9GTPA=";
  };

  # hatch-vcs reads version metadata via setuptools-scm; fetchFromGitHub strips .git.
  # Use 0.8.1.dev16 (post-0.8.0) so PEP 440 does not treat it as older than 0.8.0.
  env = {
    SETUPTOOLS_SCM_PRETEND_VERSION = "0.8.1.dev16";
    SETUPTOOLS_SCM_PRETEND_VERSION_FOR_GOOGLE_COLAB_CLI = "0.8.1.dev16";
  };

  build-system = with python3Packages; [
    hatchling
    hatch-vcs
  ];

  # The pinned googlecolab fork of jupyter-kernel-client reports 0.8.0 while
  # pyproject.toml pins ==0.9.0.
  pythonRelaxDeps = [ "jupyter-kernel-client" ];

  dependencies =
    (with python3Packages; [
      click
      filelock
      google-auth
      google-auth-oauthlib
      html2text
      jupyter-kernel-client
      keyring
      nbformat
      packaging
      prompt-toolkit
      pydantic
      pygments
      requests
      rich
      typer
      typing-extensions
      websocket-client
    ])
    ++ lib.optionals stdenv.hostPlatform.isLinux [
      # Enables D-Bus SecretService keyring backend (GNOME Keyring / KWallet)
      # on Linux desktops so OAuth tokens stay in the OS keyring.
      python3Packages.secretstorage
    ];

  makeWrapperArgs = [
    "--prefix"
    "PATH"
    ":"
    (lib.makeBinPath [ openssh ])
  ];

  doCheck = false;
  pythonImportsCheck = [ "colab_cli" ];

  postInstallCheck = ''
    HOME="$TMPDIR" "$out/bin/colab" --help > /dev/null
    HOME="$TMPDIR" "$out/bin/colab" ssh --help > /dev/null
    HOME="$TMPDIR" "$out/bin/colab" usage --help > /dev/null
  '';

  meta = {
    description = "Official Google Colab command-line interface";
    homepage = "https://github.com/googlecolab/google-colab-cli";
    license = lib.licenses.asl20;
    mainProgram = "colab";
    platforms = lib.platforms.unix;
  };
}

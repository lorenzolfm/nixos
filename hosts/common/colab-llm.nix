# Official Google Colab CLI (`colab`) and on-demand G4 LLM launcher (`colab-llm`).
# Imported on personal machines (desktop + personal macbook), not work-macbook.
{ pkgs, ... }:
let
  google-colab-cli = pkgs.callPackage ../../pkgs/google-colab-cli { };
  colab-llm = pkgs.callPackage ../../pkgs/colab-llm { inherit google-colab-cli; };
in
{
  environment.systemPackages = [
    google-colab-cli
    colab-llm
  ];
}

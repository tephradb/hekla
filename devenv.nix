{
  inputs,
  pkgs,
  ...
}:

{
  languages.rust = {
    enable = true;
    channel = "nightly";
  };

  packages = [
    inputs.tephra.packages.${pkgs.system}.tephra-server
  ];
}

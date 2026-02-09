{
  pkgs,
}:
pkgs.stdenv.mkDerivation {
  pname = "windows-11-CHV-image-raw";
  version = "0.1.0";

  src = pkgs.copyPathToStore "/tmp/windows-disk.raw";

  unpackPhase = ":";
  buildPhase = ''
    mkdir -p $out
    cp $src $out/image.raw
  '';
}

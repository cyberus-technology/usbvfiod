{
  lib,
  pkgs,
  usbvfiod,
}:
let
  testutils = import ./testutils.nix { inherit lib pkgs; };
  mkNixosIntegrationTest =
    file:
    pkgs.callPackage file {
      inherit (pkgs) cloud-hypervisor;
      inherit usbvfiod testutils;
    };
in
{
  multiple-blockdevices = mkNixosIntegrationTest ./multiple-blockdevices.nix;
  forceful-removal = mkNixosIntegrationTest ./forceful-removal.nix;
}
// import ./blockdevice.nix {
  inherit (pkgs)
    cloud-hypervisor
    ;
  inherit
    lib
    usbvfiod
    testutils
    ;
}
// import ./attach-detach.nix {
  inherit (pkgs)
    cloud-hypervisor
    ;
  inherit
    lib
    usbvfiod
    testutils
    ;
}
// import ./interrupt.nix {
  inherit (pkgs)
    cloud-hypervisor
    ;
  inherit
    lib
    usbvfiod
    testutils
    ;
}

{
  lib,
  pkgs,
  usbvfiod,
}:
let
  testutils = import ./testutils.nix {
    inherit lib pkgs usbvfiod;
    inherit (pkgs) cloud-hypervisor;
  };
  mkNixosIntegrationTest =
    file:
    pkgs.callPackage file {
      inherit testutils;
    };
in
{
  multiple-blockdevices = mkNixosIntegrationTest ./multiple-blockdevices.nix;
  forceful-removal = mkNixosIntegrationTest ./forceful-removal.nix;
}
// import ./controller-reset.nix {
  inherit testutils;
}
// import ./blockdevice.nix {
  inherit testutils;
}
// import ./attach-detach.nix {
  inherit usbvfiod testutils;
}
// import ./reattach-attached.nix {
  inherit usbvfiod testutils;
}
// import ./interrupt.nix {
  inherit testutils;
}

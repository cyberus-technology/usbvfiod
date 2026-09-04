{
  craneLib,
  pkgs,
  shellHook,
}:
let
  sshhost = pkgs.writeShellScriptBin "sshhost" ''
    ssh -p 2000 root@localhost -o UserKnownHostsFile=/dev/null -o StrictHostKeyChecking=no
  '';

  sshguest = pkgs.writeShellScriptBin "sshguest" ''
    ssh -p 2000 root@localhost -o ProxyCommand="ssh -W %h:%p -p 2000 root@localhost -o UserKnownHostsFile=/dev/null -o StrictHostKeyChecking=no" -o UserKnownHostsFile=/dev/null -o StrictHostKeyChecking=no
  '';
in
{
  default = craneLib.devShell {
    inherit shellHook;
    packages = [
      sshhost
      sshguest
    ];
  };
}

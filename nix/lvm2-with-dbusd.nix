{
  lib,
  lvm2,
  python3,
  makeWrapper,
  systemd,
}:

let
  python = python3.withPackages (ps: [
    ps.pyudev
    ps.dbus-python
    ps.pygobject3
  ]);
in
(lvm2.override {
  enableCmdlib = true;
}).overrideAttrs (old: {
  pname = "lvm2-with-dbusd";

  nativeBuildInputs = (old.nativeBuildInputs or [ ]) ++ [
    makeWrapper
    python
  ];

  configureFlags = (old.configureFlags or [ ]) ++ [
    "--enable-dbus-service"
    "--enable-notify-dbus"
  ];

  postFixup = (old.postFixup or "") + ''
    wrapProgram $out/bin/lvmdbusd \
      --prefix PYTHONPATH : "$out/${python.sitePackages}" \
      --prefix PATH : "${lib.makeBinPath [ lvm2 systemd ]}"

    # Keep only the lvmdbusd unit so systemd.packages doesn't collide
    # with the system lvm2 units.
    find $out/lib/systemd/system -mindepth 1 -maxdepth 1 \
      ! -name 'lvm2-lvmdbusd.service' -exec rm -rf {} +
    find $out/lib/systemd -mindepth 2 -maxdepth 2 -type d -empty -delete
  '';

  # wrapProgram pulls in bash, which the upstream derivation forbids.
  outputChecks = { };
})

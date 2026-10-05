# GNOME Shell under systemd (logind, system bus), PipeWire, and what the test drives it with.
FROM fedora:44
RUN dnf install -y -q systemd gnome-shell mutter pipewire wireplumber python3-dbus python3-gobject \
      gstreamer1-plugins-base gstreamer1-plugins-good gstreamer1-plugins-bad-free python3 procps-ng dbus-daemon >/dev/null \
    && dnf clean all >/dev/null
CMD ["/usr/lib/systemd/systemd"]

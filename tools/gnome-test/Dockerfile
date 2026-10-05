FROM archlinux:latest
RUN pacman -Sy --noconfirm --disable-sandbox archlinux-keyring >/dev/null && \
    pacman -Su --noconfirm --disable-sandbox >/dev/null && \
    pacman -S --noconfirm --disable-sandbox mutter pipewire wireplumber python python-gobject python-dbus \
      gst-plugins-base gst-plugin-pipewire gstreamer mesa dbus >/dev/null && \
    pacman -Scc --noconfirm >/dev/null

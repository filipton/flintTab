# Builds the Linux release (tools/build.sh) on the oldest system it supports, Ubuntu 24.04: its
# GStreamer 1.24 and PipeWire 1.0 plugin get bundled into the file (Debian 12's PipeWire 0.3
# plugin handed current desktops' frames over in a layout the converter could not read).
FROM ubuntu:24.04
RUN apt-get update && apt-get install -y --no-install-recommends ca-certificates curl build-essential pkg-config \
    zstd python3 libgstreamer1.0-dev libgstreamer-plugins-base1.0-dev gstreamer1.0-plugins-base \
    gstreamer1.0-plugins-good gstreamer1.0-plugins-bad gstreamer1.0-plugins-ugly gstreamer1.0-pipewire \
    gstreamer1.0-x gstreamer1.0-pulseaudio \
    && rm -rf /var/lib/apt/lists/*
RUN curl -sSf https://sh.rustup.rs | sh -s -- -y --profile minimal
ENV PATH=/root/.cargo/bin:$PATH

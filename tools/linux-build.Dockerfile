# Builds the Linux release (tools/release.sh): the oldest system it should run on (Debian 12),
# with the GStreamer that gets bundled into it.
FROM rust:1-bookworm
RUN apt-get update && apt-get install -y --no-install-recommends zstd python3 \
    libgstreamer1.0-dev libgstreamer-plugins-base1.0-dev gstreamer1.0-plugins-base gstreamer1.0-plugins-good \
    gstreamer1.0-plugins-bad gstreamer1.0-plugins-ugly gstreamer1.0-pipewire gstreamer1.0-x gstreamer1.0-pulseaudio \
    && rm -rf /var/lib/apt/lists/*

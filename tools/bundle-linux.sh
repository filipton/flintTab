#!/bin/sh
# Packs the Linux host with the GStreamer libraries and plugins it uses into one .tar.zst,
# which the launcher (launcher/) carries: the release then needs nothing installed.
# Run on the oldest distribution to support (CI: Debian 12), with the GStreamer runtime,
# its -dev packages and zstd installed. Usage: tools/bundle-linux.sh HOST_BINARY OUT.tar.zst
set -eu
host=$1
out=$2
gst=/usr/lib/$(gcc -print-multiarch)/gstreamer-1.0
b=$(mktemp -d)/bundle
mkdir -p "$b/lib/gstreamer-1.0"
cp "$host" "$b/tabdisplay-host"

# What the host's pipelines use (src/linux.rs): capture, conversion, encoders, audio.
for p in coreelements app videoconvertscale videoconvert videoscale videotestsrc pipewire ximagesrc \
         videoparsersbad va nvcodec x264 pulseaudio audioconvert audioresample; do
    [ -f "$gst/libgst$p.so" ] && cp "$gst/libgst$p.so" "$b/lib/gstreamer-1.0/"
done

# Their libraries, except what must come from the user's system: the C library, the GPU and
# video drivers (libva, libdrm, CUDA), the display and sound servers' client libraries.
system='^(ld-linux|linux-vdso|libc\.|libm\.|libdl\.|libpthread\.|librt\.|libresolv\.|libutil\.|libstdc\+\+|libgcc_s|libGL|libEGL|libOpenGL|libgbm|libdrm|libva|libcuda|libnvidia|libpipewire|libpulse|libX|libxcb|libwayland|libudev|libsystemd|libasound|libdbus-1)'
ldd "$b/tabdisplay-host" "$b"/lib/gstreamer-1.0/*.so | awk '/=> \//{print $3}' | sort -u | while read -r lib; do
    basename "$lib" | grep -qE "$system" || cp -L "$lib" "$b/lib/"
done

tar -C "$b" -cf - . | zstd -19 -T0 -q -f -o "$out"
echo "$out: $(du -h "$out" | cut -f1), $(ls "$b/lib" | wc -l) libraries, $(ls "$b/lib/gstreamer-1.0" | wc -l) plugins"

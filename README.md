# TabDisplay – Android tablet as a USB secondary display for macOS and Linux

```
macOS: virtual display -> ScreenCaptureKit (NV12 IOSurface + system audio)
       -> VideoToolbox H.264 in-process (zero-copy, low-latency rate control)
Linux: portal virtual monitor (PipeWire) or X11 region -> GStreamer
       -> VA-API / NVENC / Quick Sync H.264 (x264 fallback)
                 -> TCP 127.0.0.1:27183 -> adb reverse (USB)
Android app: MediaCodec low-latency decode -> SurfaceView, PCM -> AudioTrack (low-latency)
```

## Requirements
- Tablet: USB debugging on, authorize the computer, Android 11+ (minSdk 30)
- Mac: Rust, `brew install android-platform-tools`, Screen Recording permission for your terminal
- Linux: Rust, `adb`, GStreamer 1.22+ with plugins base/good/bad (and ugly for x264), the
  PipeWire GStreamer plugin, and for GPU encoding the VA (`gstreamer1.0-plugins-bad` +
  `intel-media-va-driver`/Mesa VA) or NVIDIA nvcodec plugins. Debian/Ubuntu:
  `sudo apt install libgstreamer1.0-dev libgstreamer-plugins-base1.0-dev gstreamer1.0-plugins-{good,bad,ugly} gstreamer1.0-pipewire adb`

## Build & install the tablet app
```
cd android && gradle installRelease      # or open android/ in Android Studio
```

## Run
```
cargo run --release -p tabdisplay-host
```
Plug in the tablet: the host detects it, opens the app, creates a virtual display matching the
tablet's screen and streams it.

- **macOS:** arrange the new display under System Settings > Displays.
- **GNOME 46+ / KDE Plasma 6 (Wayland):** the first run shows the screen-sharing dialog; pick
  *Virtual monitor*. The choice is remembered. GNOME sizes the monitor to the tablet; KDE uses the
  size chosen in its dialog (Plasma 6.8+) or 1920×1080, and the host scales. Set its scale in
  the display settings like any monitor.
- **Sway:** `swaymsg create_output`, then
  `swaymsg output HEADLESS-1 mode --custom 2560x1600@60Hz scale 2`, and run the host with
  `--portal-monitor` (pick HEADLESS-1, or set `output_name=HEADLESS-1` in xdg-desktop-portal-wlr's config).
- **X11:** make room for the tablet (`xrandr --fb`, `xrandr --setmonitor tablet 2560/0x1600/0+1920+0 none`,
  or an EVDI output) and run with `--x11-region 1920,0`.

**Audio** is off by default. Tap the tablet screen and use the *Audio* switch (top right) any time.
Only system audio is captured; the host mutes nothing on the computer.

The stream runs at the tablet's refresh rate (up to 120 Hz) when its decoder can keep up.

Options: `--fps N --bitrate MBPS --keep-display SECS --max-width 2560 --no-launch --width W --height H`;
macOS: `--ppi 220 --no-hidpi`; Linux: `--encoder NAME --portal-monitor --x11-region X,Y`.

## How it keeps latency low
- **Newest frame wins.** Capture only replaces a single slot the encoder reads from, so nothing
  queues up between the screen and the encoder.
- **Frame acks.** The tablet acknowledges every frame; the host keeps at most 2 unacknowledged.
  adb's own relay buffers would otherwise hide a slow link until latency had piled up.
- **Zero-delay decode.** The H.264 SPS is patched with `max_num_reorder_frames=0` /
  `max_dec_frame_buffering` (as Moonlight does), so Android decoders output each frame at once.
  The decoder gets Moonlight's per-vendor low-latency options and frames are released for the
  next vsync, newest first.
- **Idle sharpening.** When the screen stops changing, the last frame is re-encoded (every
  100 ms, like scrcpy's repeat-frame) so text sharpens after scrolling and the final state
  always arrives.
- **Recovery.** If the tablet's decoder fails it is rebuilt and asks the host for a keyframe; the
  connection and the virtual display stay up. The virtual display also survives a disconnect
  for `--keep-display` seconds (default 15) so windows stay put.

## Testing without a tablet
```
cargo run -p tabdisplay-host -- --no-adb --test-source   # Linux: test pattern, no desktop needed
python3 tools/fake_tablet.py                             # checks frames, flow control, keyframes
```

## Notes
- No touch input yet (display + audio only).
- Protocol: `host/src/protocol.rs`.

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

## The tablet app
The host installs the app over adb when the tablet is plugged in, updates it when it differs
from the one the host has, and opens it. It uses, in order: `--apk PATH`, `tabdisplay.apk` next
to the host or in the current folder, a local build in `android/app/build/outputs/apk/`, and
otherwise the build GitHub Actions publishes for this protocol version (downloaded to the cache
folder with curl, or with the GitHub CLI while the repository is private). `--no-install` turns this off.

To build it yourself (needs JDK 17, the Android SDK and NDK, e.g. from Android Studio, and
Rust with `rustup target add aarch64-linux-android` and `cargo install cargo-ndk`):
```
cd android && ./gradlew assembleRelease
```

## Run
```
cargo run --release -p tabdisplay-host
```
Plug in the tablet: the host detects it, installs or updates the app, opens it, creates a virtual display matching the
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

**Touch and pen** are off by default, so the tablet is purely a display for the computer's own
mouse and keyboard; a tap shows the settings panel. Turn on *Touch controls mouse* there to
use them: tap to click, drag to drag, two-finger drag to scroll, two-finger tap to right-click,
three-finger tap for the panel. A pen moves the cursor while hovering and its barrel button
right-clicks.
- macOS: allow your terminal under System Settings > Privacy & Security > Accessibility.
- Linux (Wayland): the portal dialog asks for pointer control along with the screen. On X11 it
  uses XTEST. Sway's portal has no remote-desktop support, so no touch input there yet.

**Audio** is off by default; use the *Audio* switch in the same panel (top right).
Only system audio is captured; the host mutes nothing on the computer.

The stream runs at the tablet's refresh rate (up to 120 Hz) when its decoder can keep up.

Options: `--fps N --bitrate MBPS --keep-display SECS --max-width 2560 --serial SERIAL --no-aoa --keep-tablet-settings --no-launch --apk PATH --no-install --width W --height H`;
macOS: `--ppi 220 --no-hidpi --cursor-in-video`; Linux: `--encoder NAME --portal-monitor --x11-region X,Y`.

## How it keeps latency low
Measured on a Galaxy Tab S10 FE and an M4 Pro: about 5-6 ms (median) from the Mac
compositing a change to the change being in the tablet's scanned-out buffer, plus the panel's
own scan (up to one refresh).

- **Raw USB, no adb relay.** The host switches the tablet to an Android Open Accessory (the
  protocol wired Android Auto uses) and talks to the app over USB bulk endpoints: ~0.3 ms round
  trips instead of adb's ~4 ms. adb stays available next to it. The first time, the tablet asks
  to allow the app to use the accessory (tick "always"). `--no-aoa` stays on adb.
- **Changed pixels, not video, for small changes.** The host finds which 16x16 blocks really
  changed and sends those as LZ4-compressed pixels: no encoder, no decoder (each costs ~7-9 ms
  per frame on this hardware). Large changes (scrolling, video) still go through H.264.
- **Drawn straight into the scanned-out buffer, timed against the scan.** The screen is one
  NV12 buffer the display hardware scans out itself (front-buffered); native code (Rust) copies
  each update into it while the panel's scan is elsewhere, so nothing tears, and at most one
  new frame per scan pass, so motion stays even. No GPU, no compositor queue (~17 ms at 90 Hz
  otherwise). Turning off "Lowest latency" in the tablet's settings panel uses a regular
  compositor-paced swap chain instead.
- **The Mac renders at the tablet's rate.** The virtual display runs at the panel's refresh rate
  (90 Hz here, not macOS's default 60); if the tablet's rate changes (its caps are lifted just
  after the app starts) the app reconnects and the display follows.
- **The tablet's own caps are lifted while streaming.** Battery saver and "Motion smoothness:
  Standard" both cap the panel at 60 Hz and slow the decoder; the host turns them off over adb
  and restores them on exit (`--keep-tablet-settings` leaves them alone).
- **The mouse skips the video (macOS).** The cursor position is polled every 1 ms and sent ahead
  of the video; the tablet draws it as a sprite.
- **Newest frame wins**, **frame acks** (at most 2 frames unacknowledged), **zero-delay H.264**
  (SPS patched like Moonlight) with the decoder at full clock, and **idle sharpening** (repeats
  only areas last sent lossily).

## Measuring latency
While streaming, the host prints where each update's time went every 5 seconds (median/p95):
capture, waiting for the encoder, encode, send, USB, the tablet's decoder input, decode, and
drawing, plus the total. Tablet timestamps are mapped onto the Mac's clock with a ping/pong.

## Testing without a tablet
```
cargo run -p tabdisplay-host -- --no-adb --test-source   # Linux: test pattern, no desktop needed
python3 tools/fake_tablet.py                             # checks frames, flow control, keyframes
```

## Notes
- Protocol: `host/src/protocol.rs`.

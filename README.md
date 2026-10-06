# TabDisplay – Android tablet as a USB secondary display for macOS and Linux

```
macOS: virtual display -> ScreenCaptureKit (NV12 + system audio)
Linux: portal virtual monitor (PipeWire) or X11 region -> GStreamer (NV12)
   -> changed 16x16 blocks: small changes as LZ4 pixels, the rest as H.264
      (VideoToolbox / VA-API, NVENC, Quick Sync, x264)
   -> raw USB (Android Open Accessory), adb as fallback
Android app: pixels / MediaCodec -> native NV12 front buffer, timed against the panel's scan
```

## Get it
One file per computer, with the tablet app built in. Nothing else to install: if `adb` is
missing, it downloads Android's platform-tools by itself (or prints the command to install it).
Download from the [latest release](https://github.com/filipton/macos-usb-display/releases/latest)
(the repository is private: be logged in, or use `gh release download -R filipton/macos-usb-display -p 'tabdisplay-*'`):

- **macOS (Apple silicon):** `tabdisplay-macos-arm64`
- **Linux (x86-64; Ubuntu 24.04, Debian 13, Fedora 40, Arch or newer):** `tabdisplay-linux-x86_64`.
  It carries its own GStreamer; it uses the desktop's PipeWire and your GPU's video driver
  (VA-API or NVIDIA) for hardware encoding, else encodes on the CPU. The first start unpacks
  it into `~/.cache/tabdisplay` (a few seconds).

```
chmod +x tabdisplay-*
xattr -d com.apple.quarantine tabdisplay-macos-arm64   # macOS, if downloaded with a browser
./tabdisplay-macos-arm64                                # or ./tabdisplay-linux-x86_64
```

On the tablet: USB debugging on (Android 11+), allow the computer when asked. On macOS, allow
Screen Recording for your terminal when asked. On Linux, raw USB needs access to the tablet's
USB device: if it is missing, the host prints the udev rule to add (until then it uses adb).

To build it yourself: `cargo run --release -p tabdisplay-host` (Linux also needs
`libgstreamer1.0-dev libgstreamer-plugins-base1.0-dev` and the GStreamer plugins).

## Building the files
On a Mac with Xcode 26, the Android SDK and Docker: `tools/build.sh` builds both files, the tablet
app inside, into `dist/` (the Linux one in Docker, then tested on clean Ubuntu and Arch). Nothing
is versioned or uploaded; copy `dist/tabdisplay-linux-x86_64` to a Linux computer and run it.

## Releasing
On a Mac with Xcode 26, the Android SDK, Docker and `gh`: `tools/release.sh 0.3.1`. It moves every
version to 0.3.1, turns the commits since the last release into its CHANGELOG.md section (shown to
accept or edit), builds both files with the tablet app in them (Linux in Docker, then run on clean
Ubuntu and Arch), tags, pushes and creates the GitHub release. `--build` only builds; `--draft`
makes a draft release. Commit subjects are `feat:`, `fix:`, `perf:`, ... so the changelog sorts them.

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
Start it and plug in the tablet: the host detects it, installs or updates the app, opens it, creates a virtual display matching the
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

**Several tablets:** run one host per tablet, each with its serial and its own port:
`tabdisplay-host -s SERIAL1 --port 27183` and `tabdisplay-host -s SERIAL2 --port 27184`
(serials from `adb devices`). Each tablet gets its own display.

Everything is set on the host; the tablet only shows what it is told.

**Touch and pen** are off by default, so the tablet is purely a display for the computer's own
mouse and keyboard. With `--touch` they drive the mouse: tap to click, drag to drag, two-finger
drag to scroll, two-finger tap to right-click. A pen moves the cursor while hovering and its
barrel button right-clicks.
- macOS: allow your terminal under System Settings > Privacy & Security > Accessibility.
- Linux (Wayland): the portal dialog asks for pointer control along with the screen. On X11 it
  uses XTEST. Sway's portal has no remote-desktop support, so no touch input there yet.

**Audio** is off by default; `--audio` plays the computer's sound on the tablet.
Only system audio is captured; the host mutes nothing on the computer.

**Lowest latency** (drawing straight into the buffer the panel scans out, Android 13+) is on by
default; `--no-lowest-latency` uses the tablet's compositor-paced swap chain instead.

**Logs:** the host writes everything it prints to `logs/host-<time>.log` in its cache folder
(`~/Library/Caches/tabdisplay` on macOS, `~/.cache/tabdisplay` on Linux), and after each session
saves the tablet's recent Android log (`tablet-<serial>-<time>.log`). If the tablet app crashes,
its report (stack trace, last log lines) is printed by the host on the next connection.

The stream runs at the tablet's refresh rate (up to 120 Hz) when its decoder can keep up.

Options: `--touch --audio --no-lowest-latency --fps N --bitrate MBPS --keep-display SECS --max-width 2560 --serial SERIAL --no-aoa --keep-tablet-settings --no-launch --apk PATH --no-install --width W --height H`;
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
  otherwise). `--no-lowest-latency`, or Android before 13, uses a compositor-paced NV12 swap
  chain instead (the NDK's ASurfaceControl on Android 11 and 12).
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

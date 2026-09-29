# TabDisplay – Android tablet as a USB secondary display for macOS

```
macOS: virtual display -> ScreenCaptureKit (NV12 IOSurface + system audio)
       -> VideoToolbox H.264 in-process (zero-copy, low-latency rate control) -> TCP 127.0.0.1:27183
                 |  adb reverse (USB)
Android app: MediaCodec low-latency decode -> SurfaceView, PCM -> AudioTrack (low-latency)
```

## Requirements
- Mac: Rust, `brew install android-platform-tools`, Screen Recording permission for your terminal
- Tablet: USB debugging on, authorize the Mac, Android 11+ (minSdk 30)

## Build & install the tablet app
```
cd android && gradle installRelease      # or open android/ in Android Studio
```

## Run
```
cargo run --release -p tabdisplay-host
```
Plug in the tablet: the host detects it, opens the app, creates a virtual display matching the
tablet's screen and streams it. Arrange it under System Settings > Displays.

**Audio** is off by default. Tap the tablet screen and use the *Audio* switch (top right) any time.
Only system audio is captured; the host mutes nothing on the Mac.

The stream runs at the tablet's refresh rate (up to 120 Hz) when its decoder can keep up.

Options: `--fps N --bitrate MBPS --keep-display SECS --max-width 2560 --ppi 220 --no-hidpi --width W --height H --no-launch`.

## How it keeps latency low
- **Newest frame wins.** Capture only replaces a single slot the encoder reads from, so nothing
  queues up between the screen and the encoder.
- **Frame acks.** The tablet acknowledges every frame; the Mac keeps at most 2 unacknowledged.
  adb's own relay buffers would otherwise hide a slow link until latency had piled up.
- **Zero-delay decode.** The H.264 SPS is patched with `max_num_reorder_frames=0` /
  `max_dec_frame_buffering` (as Moonlight does), so Android decoders output each frame at once.
- **Idle sharpening.** When the screen stops changing, the last frame is re-encoded a few times
  (every 100 ms, like scrcpy's repeat-frame) so text sharpens after scrolling and the final
  state always arrives.
- **Recovery.** If the tablet's decoder fails it is rebuilt and asks the Mac for a keyframe; the
  connection and the virtual display stay up. The virtual display also survives a disconnect
  for `--keep-display` seconds (default 15) so windows stay put.

## Notes
- Host code is macOS-only (CoreGraphics private virtual display API + ScreenCaptureKit).
- No touch input yet (display + audio only).
- Protocol: `host/src/protocol.rs`.

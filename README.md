# TabDisplay – Android tablet as a USB secondary display for macOS

```
macOS: virtual display -> ScreenCaptureKit (NV12 + system audio)
       -> ffmpeg h264_videotoolbox (low latency) -> TCP 127.0.0.1:27183
                 |  adb reverse (USB)
Android app: MediaCodec low-latency decode -> SurfaceView, PCM -> AudioTrack (low-latency)
```

## Requirements
- Mac: Rust, `brew install ffmpeg android-platform-tools`, Screen Recording permission for your terminal
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

Options: `--fps 60 --bitrate 25 --max-width 2560 --ppi 220 --no-hidpi --width W --height H --no-launch`.

## Notes
- Host code is macOS-only (CoreGraphics private virtual display API + ScreenCaptureKit).
- No touch input yet (display + audio only).
- Protocol: `host/src/protocol.rs`.

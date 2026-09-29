//! Hardware H.264 encoding through an `ffmpeg` child process (h264_videotoolbox).
//! Raw NV12 frames go into its stdin, Annex-B access units come out of its stdout.

use crate::annexb::AuSplitter;
use anyhow::{Context, Result};
use std::{
    io::Read,
    os::fd::AsRawFd,
    process::{Child, ChildStdin, Command, Stdio},
    thread,
};

pub struct Encoder {
    child: Child,
    pub stdin: Option<ChildStdin>,
}

impl Encoder {
    pub fn spawn(
        width: u32,
        height: u32,
        fps: u32,
        bitrate_mbps: u32,
        mut on_au: impl FnMut(Vec<u8>) + Send + 'static,
    ) -> Result<Self> {
        let mut child = Command::new("ffmpeg")
            .args(["-hide_banner", "-loglevel", "warning", "-nostats"])
            .args(["-f", "rawvideo", "-pixel_format", "nv12"])
            .args(["-video_size", &format!("{width}x{height}")])
            .args(["-framerate", &fps.to_string(), "-i", "-"])
            .args(["-c:v", "h264_videotoolbox", "-realtime", "1", "-prio_speed", "1"])
            .args(["-profile:v", "high", "-bf", "0", "-g", &(fps * 2).to_string()])
            .args(["-b:v", &format!("{bitrate_mbps}M")])
            .args(["-maxrate", &format!("{}M", bitrate_mbps * 2)])
            .args(["-colorspace", "bt709", "-color_primaries", "bt709"])
            .args(["-color_trc", "bt709", "-color_range", "tv"])
            .args(["-bsf:v", "h264_metadata=aud=insert"])
            .args(["-flush_packets", "1", "-f", "h264", "pipe:1"])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit())
            .spawn()
            .context("failed to start ffmpeg (is it installed? `brew install ffmpeg`)")?;

        let stdin = child.stdin.take();
        let mut stdout = child.stdout.take().unwrap();

        thread::spawn(move || {
            let fd = stdout.as_raw_fd();
            let mut splitter = AuSplitter::default();
            let mut buf = vec![0u8; 256 * 1024];
            loop {
                let n = match stdout.read(&mut buf) {
                    Ok(0) | Err(_) => return,
                    Ok(n) => n,
                };
                for au in splitter.push(&buf[..n]) {
                    on_au(au);
                }
                // Nothing more within 2ms: the last frame is complete, don't hold it
                // back until the next frame's AUD shows up.
                let mut pfd = libc::pollfd { fd, events: libc::POLLIN, revents: 0 };
                let ready = unsafe { libc::poll(&mut pfd, 1, 2) };
                if ready == 0 {
                    if let Some(au) = splitter.take_pending() {
                        on_au(au);
                    }
                }
            }
        });

        Ok(Self { child, stdin })
    }
}

impl Drop for Encoder {
    fn drop(&mut self) {
        self.stdin = None;
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

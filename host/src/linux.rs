//! Linux backend: a virtual monitor from the compositor (xdg-desktop-portal ScreenCast,
//! "virtual" source) or an X11 screen region, captured and encoded with GStreamer on the
//! best available hardware H.264 encoder (NVENC, VA-API, Quick Sync), x264 as a fallback.

use anyhow::{Context, Result, anyhow, bail};
use ashpd::desktop::{
    PersistMode, Session,
    remote_desktop::{DeviceType, KeyState, NotifyPointerAxisOptions, RemoteDesktop, SelectDevicesOptions},
    screencast::{CursorMode, Screencast, SelectSourcesOptions, SourceType},
};
use gstreamer::{self as gst, prelude::*};
use gstreamer_app as gst_app;
use gstreamer_video::prelude::*;
use std::{
    collections::VecDeque,
    os::fd::{AsRawFd, OwnedFd},
    path::PathBuf,
    sync::{
        Arc, Condvar, Mutex,
        atomic::{AtomicBool, AtomicUsize, Ordering},
        mpsc,
    },
    time::{Duration, Instant},
};

use crate::{
    Args, Button, Host, Input, Pointer, Stream, StreamConfig,
    frames::{Buffer, Frame, Frames},
    protocol, sps,
    tiles::Picture,
};

/// What gets captured.
enum Source {
    /// Moving test pattern, for testing without a desktop session.
    Test,
    /// An X11 screen region (e.g. an output added with xrandr or evdi).
    X11 { x: u32, y: u32 },
    /// A PipeWire stream handed out by the ScreenCast portal.
    Portal(PortalCast),
}

/// A RemoteDesktop session also lets us inject pointer input into the monitor we capture;
/// portals without RemoteDesktop (e.g. wlroots) get a plain ScreenCast session.
enum PortalSession {
    Cast(Session<Screencast>),
    Remote(Arc<RemoteDesktop>, Arc<Session<RemoteDesktop>>),
}

struct PortalCast {
    session: PortalSession,
    fd: OwnedFd,
    node: u32,
    /// The stream's size in the compositor's logical pixels (pointer coordinates use it).
    size: (f64, f64),
    /// Size the tablet asked for when the monitor was created.
    mode: (u32, u32),
}

pub struct LinuxHost {
    rt: Arc<tokio::runtime::Runtime>,
    source: Option<Source>,
    idle_since: Option<Instant>,
    encoder: Option<String>,
    test_source: bool,
    x11_region: Option<(u32, u32)>,
    portal_monitor: bool,
}

impl LinuxHost {
    pub fn new(args: &Args) -> Result<Self> {
        gst::init().context("GStreamer is not installed")?;
        // Started by the release launcher with its bundled libraries on the search path: the
        // loader already has it (for GStreamer's plugins too), and programs we run (adb, ...)
        // should get the system's.
        if let Some(orig) = std::env::var_os("TD_ORIG_LD_LIBRARY_PATH") {
            unsafe {
                if orig.is_empty() {
                    std::env::remove_var("LD_LIBRARY_PATH");
                } else {
                    std::env::set_var("LD_LIBRARY_PATH", orig);
                }
                std::env::remove_var("TD_ORIG_LD_LIBRARY_PATH");
            }
        }
        let x11_region = match &args.x11_region {
            Some(s) => {
                let v: Vec<u32> = s.split(',').map(|p| p.trim().parse()).collect::<Result<_, _>>()?;
                let [x, y] = v[..] else { bail!("--x11-region wants X,Y") };
                Some((x, y))
            }
            None => None,
        };
        Ok(Self {
            // multi-thread so the D-Bus connection keeps running between our block_on calls
            // (input events are sent from the session's reader thread)
            rt: Arc::new(tokio::runtime::Builder::new_multi_thread().worker_threads(1).enable_all().build()?),
            source: None,
            idle_since: None,
            encoder: args.encoder.clone(),
            test_source: args.test_source,
            x11_region,
            portal_monitor: args.portal_monitor,
        })
    }

    fn token_path(name: &str) -> Option<PathBuf> {
        let base = std::env::var_os("XDG_CONFIG_HOME")
            .map(PathBuf::from)
            .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".config")))?;
        Some(base.join("tabdisplay").join(name))
    }

    /// Asks the compositor for a new virtual monitor (GNOME, KDE Plasma 6; wlroots
    /// compositors via xdg-desktop-portal-wlr pick an existing output instead), together
    /// with pointer control when the desktop has the RemoteDesktop portal. The permission
    /// is remembered with a restore token so the dialog only shows once.
    fn open_portal(&self, cfg: &StreamConfig) -> Result<PortalCast> {
        let kind = if self.portal_monitor { SourceType::Monitor } else { SourceType::Virtual };
        let with_input = self.rt.block_on(async {
            match RemoteDesktop::new().await {
                Ok(r) => r.available_device_types().await.map(|t| t.contains(DeviceType::Pointer)).unwrap_or(false),
                Err(_) => false,
            }
        });
        let token_file = Self::token_path(if with_input { "portal-remote-token" } else { "portal-restore-token" });
        let token = token_file.as_ref().and_then(|p| std::fs::read_to_string(p).ok());

        let (session, stream, new_token, fd) = self.rt.block_on(async {
            let cast = Screencast::new().await?;
            let available = cast.available_source_types().await?;
            if !available.contains(kind) {
                return Err(anyhow!(
                    "this desktop's ScreenCast portal cannot create a {kind:?} source \
                     (GNOME 46+ or KDE Plasma 6 can; otherwise use --portal-monitor or --x11-region)"
                ));
            }
            let sources = SelectSourcesOptions::default()
                .set_cursor_mode(CursorMode::Embedded)
                .set_sources(ashpd::enumflags2::BitFlags::from(kind))
                .set_multiple(false);
            if with_input {
                let remote = RemoteDesktop::new().await?;
                let session = remote.create_session(Default::default()).await?;
                remote
                    .select_devices(
                        &session,
                        SelectDevicesOptions::default()
                            .set_devices(ashpd::enumflags2::BitFlags::from(DeviceType::Pointer))
                            .set_persist_mode(PersistMode::ExplicitlyRevoked)
                            .set_restore_token(token.as_deref()),
                    )
                    .await?;
                cast.select_sources(&session, sources).await?;
                let resp = remote.start(&session, None, Default::default()).await?.response()?;
                let stream = resp.streams().first().cloned().ok_or_else(|| anyhow!("portal returned no stream"))?;
                let token = resp.restore_token().map(str::to_owned);
                let fd = cast.open_pipe_wire_remote(&session, Default::default()).await?;
                anyhow::Ok((PortalSession::Remote(Arc::new(remote), Arc::new(session)), stream, token, fd))
            } else {
                let session = cast.create_session(Default::default()).await?;
                cast.select_sources(
                    &session,
                    sources.set_persist_mode(PersistMode::ExplicitlyRevoked).set_restore_token(token.as_deref()),
                )
                .await?;
                let resp = cast.start(&session, None, Default::default()).await?.response()?;
                let stream = resp.streams().first().cloned().ok_or_else(|| anyhow!("portal returned no stream"))?;
                let token = resp.restore_token().map(str::to_owned);
                let fd = cast.open_pipe_wire_remote(&session, Default::default()).await?;
                anyhow::Ok((PortalSession::Cast(session), stream, token, fd))
            }
        })?;
        if let (Some(path), Some(t)) = (token_file, new_token) {
            let _ = std::fs::create_dir_all(path.parent().unwrap());
            let _ = std::fs::write(path, t);
        }
        let size = stream
            .size()
            .map(|(w, h)| (w as f64, h as f64))
            .unwrap_or((cfg.width as f64, cfg.height as f64));
        println!(
            "portal stream node {} ({}x{}){}",
            stream.pipe_wire_node_id(),
            size.0,
            size.1,
            if with_input { ", with pointer input" } else { "" }
        );
        Ok(PortalCast { session, fd, node: stream.pipe_wire_node_id(), size, mode: (cfg.width, cfg.height) })
    }

    fn input(&self, cfg: &StreamConfig) -> Option<Box<dyn Input>> {
        match self.source.as_ref()? {
            Source::Test => None,
            Source::X11 { x, y } => match X11Input::new(*x, *y, cfg.width, cfg.height) {
                Ok(i) => Some(Box::new(i)),
                Err(e) => {
                    eprintln!("X11 input unavailable: {e:#}");
                    None
                }
            },
            Source::Portal(p) => match &p.session {
                PortalSession::Remote(remote, session) => Some(Box::new(PortalInput {
                    rt: self.rt.clone(),
                    remote: remote.clone(),
                    session: session.clone(),
                    node: p.node,
                    size: p.size,
                })),
                PortalSession::Cast(_) => None,
            },
        }
    }

    fn source_desc(&mut self, cfg: &StreamConfig) -> Result<String> {
        let StreamConfig { width: w, height: h, fps, .. } = *cfg;
        if matches!(&self.source, Some(Source::Portal(p)) if p.mode != (w, h)) {
            self.shutdown(); // a different tablet: start over with a monitor of its size
        }
        if self.source.is_none() {
            self.source = Some(if self.test_source {
                Source::Test
            } else if let Some((x, y)) = self.x11_region {
                Source::X11 { x, y }
            } else {
                Source::Portal(self.open_portal(cfg)?)
            });
        }
        Ok(match self.source.as_ref().unwrap() {
            Source::Test => format!(
                "videotestsrc is-live=true pattern=ball ! video/x-raw,width={w},height={h},framerate={fps}/1"
            ),
            Source::X11 { x, y } => format!(
                "ximagesrc use-damage=false show-pointer=true startx={x} starty={y} endx={} endy={} \
                 ! video/x-raw,framerate={fps}/1",
                x + w - 1,
                y + h - 1
            ),
            Source::Portal(p) => {
                // pipewiresrc takes ownership of the fd it is given, so hand it a copy.
                let fd = p.fd.try_clone()?;
                let raw = fd.as_raw_fd();
                std::mem::forget(fd);
                // GNOME creates the virtual monitor at whatever size and rate we negotiate,
                // so asking for the tablet's size through videoscale (passthrough when it
                // matches) sizes it exactly; KDE picks the size in its dialog and then
                // videoscale really scales. PipeWire only sends frames when something changed.
                format!(
                    "pipewiresrc fd={raw} path={} do-timestamp=true \
                     ! video/x-raw,max-framerate={fps}/1 ! videoscale ! video/x-raw,width={w},height={h}",
                    p.node
                )
            }
        })
    }
}

/// Encoder candidates, best first: element, pre-processing into the encoder, properties.
/// Properties an element does not have (they vary between GStreamer versions) are skipped.
fn encoders(cfg: &StreamConfig) -> Vec<(&'static str, &'static str, Vec<(&'static str, String)>)> {
    let kbps = (cfg.bitrate * 1000).to_string();
    // A keyframe every ~10 s when nothing asks for one; 1024 is the most VA's encoders take.
    let gop = (cfg.fps * 10).min(1024).to_string();
    // coded picture buffer of ~2 frames: frame sizes stay even, no bursts on the wire
    let cpb_kbit = (cfg.bitrate * 1000 * 2 / cfg.fps).to_string();
    let cpu = "videoconvert n-threads=4 ! video/x-raw,format=NV12";
    let va = "vapostproc ! video/x-raw(memory:VAMemory),format=NV12";
    let va_props = vec![
        ("rate-control", "cbr".into()),
        ("bitrate", kbps.clone()),
        ("cpb-size", cpb_kbit.clone()),
        ("b-frames", "0".into()),
        ("ref-frames", "1".into()),
        ("key-int-max", gop.clone()),
        ("target-usage", "7".into()),
    ];
    let nv_props = vec![
        ("preset", "p1".into()),
        ("tune", "ultra-low-latency".into()),
        ("rc-mode", "cbr".into()),
        ("rate-control", "cbr".into()),
        ("zerolatency", "true".into()),
        ("zero-reorder-delay", "true".into()),
        ("rc-lookahead", "0".into()),
        ("multi-pass", "disabled".into()),
        ("bitrate", kbps.clone()),
        ("bframes", "0".into()),
        ("b-frames", "0".into()),
        ("gop-size", gop.clone()),
    ];
    vec![
        // Intel / AMD, GStreamer 1.22+ "va" plugin; the low-power entrypoint first
        ("vah264lpenc", va, va_props.clone()),
        ("vah264enc", va, va_props),
        // NVIDIA
        ("nvautogpuh264enc", "cudaupload ! cudaconvert ! video/x-raw(memory:CUDAMemory),format=NV12", nv_props.clone()),
        ("nvh264enc", cpu, nv_props),
        // legacy gstreamer-vaapi (dropped upstream, still shipped by many distros)
        (
            "vaapih264enc",
            "vaapipostproc",
            vec![
                ("rate-control", "cbr".into()),
                ("bitrate", kbps.clone()),
                ("max-bframes", "0".into()),
                ("refs", "1".into()),
                ("keyframe-period", gop.clone()),
            ],
        ),
        (
            "qsvh264enc",
            cpu,
            vec![
                ("rate-control", "cbr".into()),
                ("bitrate", kbps.clone()),
                ("b-frames", "0".into()),
                ("ref-frames", "1".into()),
                ("rc-lookahead", "0".into()),
                ("gop-size", gop.clone()),
            ],
        ),
        // software fallback
        (
            "x264enc",
            cpu,
            vec![
                ("tune", "zerolatency".into()),
                ("speed-preset", "superfast".into()),
                ("bitrate", kbps),
                ("bframes", "0".into()),
                ("key-int-max", gop),
                ("rc-lookahead", "0".into()),
                ("sync-lookahead", "0".into()),
                ("vbv-buf-capacity", (2000 / cfg.fps).max(1).to_string()), // ms, ~2 frames
            ],
        ),
    ]
}

/// Sets a property from its string form; names and enum values differ between GStreamer
/// versions, so anything this element does not understand is skipped with a note.
fn set_if_supported(el: &gst::Element, name: &str, value: &str) {
    let Some(pspec) = el.find_property(name) else { return };
    // Out of range panics in set_property: check first (ranges differ between encoders).
    match gst::glib::Value::deserialize(value, pspec.value_type()) {
        Ok(v) if in_range(&pspec, &v) => el.set_property(name, v),
        _ => eprintln!("note: {} does not accept {name}={value}", el.name()),
    }
}

/// Whether an integer property's value is within the element's range.
fn in_range(pspec: &gst::glib::ParamSpec, v: &gst::glib::Value) -> bool {
    use gst::glib::{ParamSpecInt, ParamSpecInt64, ParamSpecUInt, ParamSpecUInt64};
    if let Some(p) = pspec.downcast_ref::<ParamSpecUInt>() {
        return v.get::<u32>().is_ok_and(|x| (p.minimum()..=p.maximum()).contains(&x));
    }
    if let Some(p) = pspec.downcast_ref::<ParamSpecInt>() {
        return v.get::<i32>().is_ok_and(|x| (p.minimum()..=p.maximum()).contains(&x));
    }
    if let Some(p) = pspec.downcast_ref::<ParamSpecUInt64>() {
        return v.get::<u64>().is_ok_and(|x| (p.minimum()..=p.maximum()).contains(&x));
    }
    if let Some(p) = pspec.downcast_ref::<ParamSpecInt64>() {
        return v.get::<i64>().is_ok_and(|x| (p.minimum()..=p.maximum()).contains(&x));
    }
    true
}

/// A captured frame: a GStreamer buffer of NV12 pixels.
#[derive(Clone)]
struct GstFrame {
    buf: gst::Buffer,
    info: Arc<gstreamer_video::VideoInfo>,
}

impl Buffer for GstFrame {
    type Pic<'a> = GstPic<'a>;
    fn picture(&self) -> Option<GstPic<'_>> {
        let f = gstreamer_video::VideoFrameRef::from_buffer_ref_readable(self.buf.as_ref(), &self.info).ok()?;
        (f.format() == gstreamer_video::VideoFormat::Nv12).then_some(GstPic(f))
    }
}

struct GstPic<'a>(gstreamer_video::VideoFrameRef<&'a gst::BufferRef>);

impl Picture for GstPic<'_> {
    fn size(&self) -> (usize, usize) {
        (self.0.width() as usize, self.0.height() as usize)
    }
    fn row(&self, plane: usize, row: usize) -> Option<&[u8]> {
        let data = self.0.plane_data(plane as u32).ok()?;
        let stride = *self.0.plane_stride().get(plane)? as usize;
        data.get(row * stride..row * stride + self.0.width() as usize)
    }
}

/// Stops capture first (no more frames), then the frame thread, then the encoder and audio.
struct Running {
    capture: gst::Pipeline,
    gate: Arc<crate::gate::Gate<Frame<GstFrame>>>,
    frame_thread: Option<std::thread::JoinHandle<()>>,
    others: Vec<gst::Pipeline>,
}

impl Drop for Running {
    fn drop(&mut self) {
        let _ = self.capture.set_state(gst::State::Null);
        self.gate.close();
        if let Some(t) = self.frame_thread.take() {
            let _ = t.join();
        }
        for p in &self.others {
            let _ = p.set_state(gst::State::Null);
        }
    }
}

/// Reports what goes wrong in the capture after it started (it fails quietly otherwise: a black
/// tablet), and says so when no picture has come at all a few seconds in.
fn watch_capture(p: &gst::Pipeline, frames: Arc<AtomicUsize>) {
    let bus = p.bus().unwrap();
    let pipeline = p.downgrade();
    let started = Instant::now();
    std::thread::spawn(move || {
        let mut warned = false;
        while pipeline.upgrade().is_some() {
            if let Some(msg) = bus.timed_pop(gst::ClockTime::from_mseconds(250)) {
                let src = msg.src().map(|s| s.name().to_string()).unwrap_or_default();
                match msg.view() {
                    gst::MessageView::Error(e) => eprintln!("screen capture failed ({src}): {} ({:?})", e.error(), e.debug()),
                    gst::MessageView::Warning(w) => eprintln!("screen capture warning ({src}): {}", w.error()),
                    _ => {}
                }
            }
            if !warned && started.elapsed() > Duration::from_secs(3) && frames.load(Ordering::Relaxed) == 0 {
                warned = true;
                eprintln!(
                    "no picture from the screen yet (the tablet stays black): run with GST_DEBUG=3 to see why"
                );
            }
        }
    });
}

/// Waits until the pipeline plays, or returns the first error it posts.
fn wait_playing(p: &gst::Pipeline) -> Result<()> {
    p.set_state(gst::State::Playing)?;
    let bus = p.bus().unwrap();
    let deadline = Instant::now() + Duration::from_secs(5);
    while Instant::now() < deadline {
        let Some(msg) = bus.timed_pop(gst::ClockTime::from_mseconds(100)) else { continue };
        match msg.view() {
            gst::MessageView::Error(e) => bail!("{} ({:?})", e.error(), e.debug()),
            gst::MessageView::StateChanged(s)
                if msg.src() == Some(p.upcast_ref()) && s.current() == gst::State::Playing =>
            {
                return Ok(());
            }
            _ => {}
        }
    }
    bail!("pipeline did not start")
}

fn start_audio(audio_on: Arc<AtomicBool>, tx: mpsc::Sender<Vec<u8>>) -> Result<gst::Pipeline> {
    let p = gst::parse::launch(&format!(
        "pulsesrc device=@DEFAULT_MONITOR@ buffer-time=40000 latency-time=10000 \
         ! audioconvert ! audioresample \
         ! audio/x-raw,format=S16LE,layout=interleaved,rate={},channels={} \
         ! appsink name=sink sync=false",
        protocol::AUDIO_RATE,
        protocol::AUDIO_CHANNELS
    ))?
    .downcast::<gst::Pipeline>()
    .unwrap();
    let sink = p.by_name("sink").unwrap().downcast::<gst_app::AppSink>().unwrap();
    sink.set_callbacks(
        gst_app::AppSinkCallbacks::builder()
            .new_sample(move |s| {
                let sample = s.pull_sample().map_err(|_| gst::FlowError::Eos)?;
                if audio_on.load(Ordering::Relaxed) {
                    let buf = sample.buffer().ok_or(gst::FlowError::Error)?;
                    let map = buf.map_readable().map_err(|_| gst::FlowError::Error)?;
                    tx.send(protocol::audio_msg(&map)).ok();
                }
                Ok(gst::FlowSuccess::Ok)
            })
            .build(),
    );
    wait_playing(&p)?;
    Ok(p)
}

/// Frames handed to the encoder and not out of it yet: (pts, changed area), oldest first.
type Pending = Arc<(Mutex<VecDeque<(u64, [u16; 4])>>, Condvar)>;

/// A running H.264 encoder fed by the host (appsrc), not by the capture pipeline: the frame
/// thread decides per frame whether it is encoded at all (small changes go out as tiles).
struct Encoder {
    pipeline: gst::Pipeline,
    src: gst_app::AppSrc,
    enc: gst::Element,
    pending: Pending,
}

impl Encoder {
    /// The first candidate that starts and encodes a test frame.
    fn start(
        wanted: Option<&str>,
        cfg: &StreamConfig,
        frames: &Arc<Frames<GstFrame>>,
        tx: &mpsc::Sender<Vec<u8>>,
    ) -> Result<(Self, &'static str)> {
        let mut last_err = match wanted {
            Some(e) => anyhow!("encoder {e} is not available (see gst-inspect-1.0 {e})"),
            None => anyhow!("no H.264 encoder found (install gstreamer1.0-plugins-bad and -ugly)"),
        };
        let StreamConfig { width: w, height: h, fps, .. } = *cfg;
        for (name, pre, props) in encoders(cfg) {
            if wanted.is_some_and(|e| e != name) || gst::ElementFactory::find(name).is_none() {
                continue;
            }
            let desc = format!(
                "appsrc name=src is-live=true format=time do-timestamp=false \
                 caps=video/x-raw,format=NV12,width={w},height={h},framerate={fps}/1 \
                 ! {pre} ! {name} name=enc \
                 ! h264parse config-interval=-1 ! video/x-h264,stream-format=byte-stream,alignment=au \
                 ! appsink name=sink sync=false"
            );
            let pipeline = match gst::parse::launch(&desc) {
                Ok(p) => p.downcast::<gst::Pipeline>().unwrap(),
                Err(e) => {
                    last_err = anyhow!("{name}: {e}");
                    continue;
                }
            };
            let enc = pipeline.by_name("enc").unwrap();
            for (k, v) in &props {
                set_if_supported(&enc, k, v);
            }
            let src = pipeline.by_name("src").unwrap().downcast::<gst_app::AppSrc>().unwrap();
            let sink = pipeline.by_name("sink").unwrap().downcast::<gst_app::AppSink>().unwrap();
            let pending: Pending = Default::default();
            let (probe_tx, probe_rx) = mpsc::sync_channel::<()>(1);
            let (f, tx, p) = (frames.clone(), tx.clone(), pending.clone());
            let mut first = true;
            sink.set_callbacks(
                gst_app::AppSinkCallbacks::builder()
                    .new_sample(move |s| {
                        let sample = s.pull_sample().map_err(|_| gst::FlowError::Eos)?;
                        let buf = sample.buffer().ok_or(gst::FlowError::Error)?;
                        let map = buf.map_readable().map_err(|_| gst::FlowError::Error)?;
                        let next = {
                            let mut q = p.0.lock().unwrap();
                            let n = q.pop_front();
                            p.1.notify_all();
                            n
                        };
                        // The test frame: the encoder works.
                        let Some((pts, area)) = next else {
                            let _ = probe_tx.try_send(());
                            return Ok(gst::FlowSuccess::Ok);
                        };
                        let patched = sps::patch_annexb(&map);
                        if first && patched.is_none() {
                            return Ok(gst::FlowSuccess::Ok); // wait for the first SPS/IDR
                        }
                        first = false;
                        f.encoded(&tx, pts, area, patched.as_deref().unwrap_or(&map));
                        Ok(gst::FlowSuccess::Ok)
                    })
                    .build(),
            );
            // Fed by us, so it may only finish starting once data arrives: the test frame
            // shows whether it works.
            let tested = pipeline.set_state(gst::State::Playing).map_err(anyhow::Error::from).and_then(|_| {
                // One black frame: many encoders only fail once they see data.
                let info = gstreamer_video::VideoInfo::builder(gstreamer_video::VideoFormat::Nv12, w, h).build()?;
                let mut b = gst::Buffer::with_size(info.size())?;
                {
                    let b = b.get_mut().unwrap();
                    let mut m = b.map_writable()?;
                    let luma = (w * h) as usize;
                    m[..luma].fill(16);
                    m[luma..].fill(128);
                    drop(m);
                    b.set_pts(gst::ClockTime::ZERO);
                }
                src.push_buffer(b)?;
                let bus = pipeline.bus().unwrap();
                let deadline = Instant::now() + Duration::from_secs(3);
                while Instant::now() < deadline {
                    if probe_rx.recv_timeout(Duration::from_millis(50)).is_ok() {
                        return Ok(());
                    }
                    if let Some(msg) = bus.pop_filtered(&[gst::MessageType::Error])
                        && let gst::MessageView::Error(e) = msg.view()
                    {
                        bail!("{} ({:?})", e.error(), e.debug());
                    }
                }
                bail!("no output for a test frame")
            });
            match tested {
                Ok(()) => return Ok((Self { pipeline, src, enc, pending }, name)),
                Err(e) => {
                    let _ = pipeline.set_state(gst::State::Null);
                    eprintln!("{name} failed: {e:#}");
                    last_err = e.context(name);
                }
            }
        }
        Err(last_err)
    }

    /// Hands `f` to the encoder as `pts`. Waits while two frames are still inside it, so a
    /// slow encoder does not queue up latency (the frame thread then picks the newest frame).
    fn encode(&self, f: &Frame<GstFrame>, pts: u64, area: [u16; 4], keyframe: bool) {
        {
            let (q, cv) = &*self.pending;
            let mut q = q.lock().unwrap();
            let deadline = Instant::now() + Duration::from_millis(100);
            while q.len() >= 2 && Instant::now() < deadline {
                q = cv.wait_timeout(q, Duration::from_millis(10)).unwrap().0;
            }
            if q.len() >= 2 {
                q.clear(); // the encoder lost frames: start over
            }
            q.push_back((pts, area));
        }
        if keyframe && let Some(pad) = self.enc.static_pad("src") {
            pad.send_event(gstreamer_video::UpstreamForceKeyUnitEvent::builder().all_headers(true).build());
        }
        let mut b = f.buf.buf.copy(); // shares the pixels
        b.make_mut().set_pts(gst::ClockTime::from_useconds(pts));
        let _ = self.src.push_buffer(b);
    }
}

impl Host for LinuxHost {
    fn start(
        &mut self,
        _args: &Args,
        cfg: &StreamConfig,
        audio_on: Arc<AtomicBool>,
        tx: mpsc::Sender<Vec<u8>>,
    ) -> Result<Stream> {
        self.idle_since = None;
        let StreamConfig { width: w, height: h, .. } = *cfg;
        let timing = cfg.timing.clone();
        // Debugging: TD_TILES=none sends everything through H.264.
        let use_tiles = cfg.tiles && std::env::var("TD_TILES").map_or(true, |v| v != "none");
        let frames = Frames::<GstFrame>::new(timing.clone(), w, h);
        let (encoder, name) = Encoder::start(self.encoder.as_deref(), cfg, &frames, &tx)?;
        println!("encoding with {name}");

        // Capture: NV12 frames of the tablet's size, the newest one handed to the frame thread.
        let src = self.source_desc(cfg)?;
        let capture = gst::parse::launch(&format!(
            "{src} ! videoconvert n-threads=4 ! video/x-raw,format=NV12,width={w},height={h} \
             ! appsink name=raw sync=false max-buffers=1 drop=true"
        ))?
        .downcast::<gst::Pipeline>()
        .unwrap();
        let sink = capture.by_name("raw").unwrap().downcast::<gst_app::AppSink>().unwrap();
        let captured = Arc::new(AtomicUsize::new(0));
        {
            let frames = frames.clone();
            let captured = captured.clone();
            let mut info: Option<(gst::Caps, Arc<gstreamer_video::VideoInfo>)> = None;
            sink.set_callbacks(
                gst_app::AppSinkCallbacks::builder()
                    .new_sample(move |s| {
                        let sample = s.pull_sample().map_err(|_| gst::FlowError::Eos)?;
                        let caps = sample.caps().ok_or(gst::FlowError::Error)?;
                        if info.as_ref().is_none_or(|(c, _)| c.as_ref() != caps) {
                            let i = gstreamer_video::VideoInfo::from_caps(caps).map_err(|_| gst::FlowError::Error)?;
                            info = Some((caps.to_owned(), Arc::new(i)));
                        }
                        let buf = sample.buffer_owned().ok_or(gst::FlowError::Error)?;
                        // How long ago the source timestamped it, per the pipeline clock.
                        let running = sample
                            .segment()
                            .and_then(|seg| seg.downcast_ref::<gst::ClockTime>())
                            .zip(buf.pts())
                            .and_then(|(seg, pts)| seg.to_running_time(pts));
                        let composited = match (s.clock(), s.base_time(), running) {
                            (Some(c), Some(base), Some(rt)) => {
                                let age = c.time().saturating_sub(base).saturating_sub(rt);
                                Some(timing.ago(Duration::from_nanos(age.nseconds())))
                            }
                            _ => None,
                        };
                        let info = info.as_ref().unwrap().1.clone();
                        captured.fetch_add(1, Ordering::Relaxed);
                        frames.push(GstFrame { buf, info }, composited, None);
                        Ok(gst::FlowSuccess::Ok)
                    })
                    .build(),
            );
        }
        let gate = frames.gate.clone();
        let frame_thread = {
            let frames = frames.clone();
            let tx = tx.clone();
            let enc = encoder;
            Some(std::thread::spawn(move || {
                frames.run(use_tiles, tx, |f, pts, area, keyframe| enc.encode(f, pts, area, keyframe));
                let _ = enc.pipeline.set_state(gst::State::Null);
            }))
        };
        let mut running = Running { capture, gate: gate.clone(), frame_thread, others: Vec::new() };
        wait_playing(&running.capture)?;
        watch_capture(&running.capture, captured.clone());
        match start_audio(audio_on, tx) {
            Ok(a) => running.others.push(a),
            Err(e) => eprintln!("audio unavailable: {e:#}"),
        }
        let input = self.input(cfg);
        Ok(Stream { control: gate, input, guard: Box::new(running) })
    }

    fn release(&mut self) {
        self.idle_since = Some(Instant::now());
    }

    fn expire(&mut self, after: Duration) {
        if self.idle_since.is_some_and(|t| t.elapsed() >= after) {
            self.shutdown();
        }
    }

    fn shutdown(&mut self) {
        if let Some(Source::Portal(p)) = self.source.take() {
            let _ = match &p.session {
                PortalSession::Cast(s) => self.rt.block_on(s.close()),
                PortalSession::Remote(_, s) => self.rt.block_on(s.close()),
            };
            println!("virtual monitor removed");
        }
        self.idle_since = None;
    }
}

const BTN_LEFT: i32 = 0x110;
const BTN_RIGHT: i32 = 0x111;

/// Pointer input through the RemoteDesktop portal, in the captured stream's coordinates.
struct PortalInput {
    rt: Arc<tokio::runtime::Runtime>,
    remote: Arc<RemoteDesktop>,
    session: Arc<Session<RemoteDesktop>>,
    node: u32,
    size: (f64, f64),
}

impl Input for PortalInput {
    fn pointer(&mut self, ev: Pointer, x: f64, y: f64, _clicks: u32) {
        let (r, s) = (&self.remote, &*self.session);
        // Inside the stream: its right and bottom edges are one past the last pixel.
        let (px, py) = (x.clamp(0.0, 1.0) * (self.size.0 - 1.0), y.clamp(0.0, 1.0) * (self.size.1 - 1.0));
        let res = self.rt.block_on(async {
            r.notify_pointer_motion_absolute(s, self.node, px, py, Default::default()).await?;
            let button = match ev {
                Pointer::Down(b) => Some((b, KeyState::Pressed)),
                Pointer::Up(b) => Some((b, KeyState::Released)),
                _ => None,
            };
            if let Some((b, state)) = button {
                let code = if b == Button::Left { BTN_LEFT } else { BTN_RIGHT };
                r.notify_pointer_button(s, code, state, Default::default()).await?;
            }
            ashpd::Result::Ok(())
        });
        if let Err(e) = res {
            eprintln!("pointer input failed: {e}");
        }
    }

    fn scroll(&mut self, dx: f64, dy: f64) {
        // The portal's axis is in scroll direction; content following the fingers is the opposite.
        let opts = NotifyPointerAxisOptions::default().set_finish(true);
        let _ = self.rt.block_on(self.remote.notify_pointer_axis(&self.session, -dx, -dy, opts));
    }
}

/// Pointer input on X11 through the XTEST extension, inside the captured region.
struct X11Input {
    conn: x11rb::rust_connection::RustConnection,
    root: u32,
    region: (f64, f64, f64, f64),
    scroll: (f64, f64),
}

impl X11Input {
    fn new(x: u32, y: u32, w: u32, h: u32) -> Result<Self> {
        let (conn, screen) = x11rb::connect(None)?;
        let root = x11rb::connection::Connection::setup(&conn).roots[screen].root;
        Ok(Self { conn, root, region: (x as f64, y as f64, w as f64, h as f64), scroll: (0.0, 0.0) })
    }

    fn fake(&self, kind: u8, detail: u8, x: f64, y: f64) {
        use x11rb::{connection::Connection, protocol::xtest::ConnectionExt};
        let (rx, ry, rw, rh) = self.region;
        let _ = self.conn.xtest_fake_input(kind, detail, 0, self.root, (rx + x * (rw - 1.0)) as i16, (ry + y * (rh - 1.0)) as i16, 0);
        let _ = self.conn.flush();
    }
}

impl Input for X11Input {
    fn pointer(&mut self, ev: Pointer, x: f64, y: f64, _clicks: u32) {
        use x11rb::protocol::xproto::{BUTTON_PRESS_EVENT, BUTTON_RELEASE_EVENT, MOTION_NOTIFY_EVENT};
        self.fake(MOTION_NOTIFY_EVENT, 0, x, y);
        match ev {
            Pointer::Down(b) => self.fake(BUTTON_PRESS_EVENT, if b == Button::Left { 1 } else { 3 }, x, y),
            Pointer::Up(b) => self.fake(BUTTON_RELEASE_EVENT, if b == Button::Left { 1 } else { 3 }, x, y),
            _ => {}
        }
    }

    fn scroll(&mut self, dx: f64, dy: f64) {
        use x11rb::{
            connection::Connection,
            protocol::xproto::{BUTTON_PRESS_EVENT, BUTTON_RELEASE_EVENT},
            protocol::xtest::ConnectionExt,
        };
        // X11 scrolls in wheel clicks (buttons 4-7); one click per 40 px of finger travel.
        const STEP: f64 = 40.0;
        self.scroll.0 += dx;
        self.scroll.1 += dy;
        let mut clicks = Vec::new();
        while self.scroll.1 >= STEP { clicks.push(4); self.scroll.1 -= STEP; } // fingers down: scroll up
        while self.scroll.1 <= -STEP { clicks.push(5); self.scroll.1 += STEP; }
        while self.scroll.0 >= STEP { clicks.push(6); self.scroll.0 -= STEP; }
        while self.scroll.0 <= -STEP { clicks.push(7); self.scroll.0 += STEP; }
        for b in clicks {
            let _ = self.conn.xtest_fake_input(BUTTON_PRESS_EVENT, b, 0, self.root, 0, 0, 0);
            let _ = self.conn.xtest_fake_input(BUTTON_RELEASE_EVENT, b, 0, self.root, 0, 0, 0);
        }
        let _ = self.conn.flush();
    }
}

#[cfg(test)]
mod tests {
    use gstreamer::prelude::*;

    /// Values outside an element's range are skipped, not a panic (VA's key-int-max tops out at 1024).
    #[test]
    fn out_of_range_property_is_skipped() {
        gstreamer::init().unwrap();
        let Ok(enc) = gstreamer::ElementFactory::make("x264enc").build() else { return };
        super::set_if_supported(&enc, "qp-max", "1000"); // range 0-63
        super::set_if_supported(&enc, "key-int-max", "1800");
        assert_eq!(enc.property::<u32>("key-int-max"), 1800);
        assert_ne!(enc.property::<u32>("qp-max"), 1000);
    }
}

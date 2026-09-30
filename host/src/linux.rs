//! Linux backend: a virtual monitor from the compositor (xdg-desktop-portal ScreenCast,
//! "virtual" source) or an X11 screen region, captured and encoded with GStreamer on the
//! best available hardware H.264 encoder (NVENC, VA-API, Quick Sync), x264 as a fallback.

use anyhow::{Context, Result, anyhow, bail};
use ashpd::desktop::{
    PersistMode, Session,
    screencast::{CursorMode, Screencast, SelectSourcesOptions, SourceType},
};
use gstreamer::{self as gst, prelude::*};
use gstreamer_app as gst_app;
use std::{
    os::fd::{AsRawFd, OwnedFd},
    path::PathBuf,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, AtomicUsize, Ordering},
        mpsc,
    },
    time::{Duration, Instant},
};

use crate::{Args, Control, Host, Stream, StreamConfig, gate::MAX_IN_FLIGHT, protocol, sps};

/// What gets captured.
enum Source {
    /// Moving test pattern, for testing without a desktop session.
    Test,
    /// An X11 screen region (e.g. an output added with xrandr or evdi).
    X11 { x: u32, y: u32 },
    /// A PipeWire stream handed out by the ScreenCast portal.
    Portal(PortalCast),
}

struct PortalCast {
    proxy: Screencast,
    session: Session<Screencast>,
    fd: OwnedFd,
    node: u32,
    /// Size the tablet asked for when the monitor was created.
    mode: (u32, u32),
}

pub struct LinuxHost {
    rt: tokio::runtime::Runtime,
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
        let x11_region = match &args.x11_region {
            Some(s) => {
                let v: Vec<u32> = s.split(',').map(|p| p.trim().parse()).collect::<Result<_, _>>()?;
                let [x, y] = v[..] else { bail!("--x11-region wants X,Y") };
                Some((x, y))
            }
            None => None,
        };
        Ok(Self {
            rt: tokio::runtime::Builder::new_current_thread().enable_all().build()?,
            source: None,
            idle_since: None,
            encoder: args.encoder.clone(),
            test_source: args.test_source,
            x11_region,
            portal_monitor: args.portal_monitor,
        })
    }

    fn token_path() -> Option<PathBuf> {
        let base = std::env::var_os("XDG_CONFIG_HOME")
            .map(PathBuf::from)
            .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".config")))?;
        Some(base.join("tabdisplay").join("portal-restore-token"))
    }

    /// Asks the compositor for a new virtual monitor (GNOME, KDE Plasma 6; wlroots
    /// compositors via xdg-desktop-portal-wlr pick an existing output instead). The
    /// permission is remembered with a restore token so the dialog only shows once.
    fn open_portal(&self, cfg: &StreamConfig) -> Result<PortalCast> {
        let token_file = Self::token_path();
        let token = token_file.as_ref().and_then(|p| std::fs::read_to_string(p).ok());
        let kind = if self.portal_monitor { SourceType::Monitor } else { SourceType::Virtual };
        let (proxy, session, stream, new_token) = self.rt.block_on(async {
            let proxy = Screencast::new().await?;
            let available = proxy.available_source_types().await?;
            if !available.contains(kind) {
                return Err(anyhow!(
                    "this desktop's ScreenCast portal cannot create a {kind:?} source \
                     (GNOME 46+ or KDE Plasma 6 can; otherwise use --portal-monitor or --x11-region)"
                ));
            }
            let session = proxy.create_session(Default::default()).await?;
            proxy
                .select_sources(
                    &session,
                    SelectSourcesOptions::default()
                        .set_cursor_mode(CursorMode::Embedded)
                        .set_sources(ashpd::enumflags2::BitFlags::from(kind))
                        .set_multiple(false)
                        .set_persist_mode(PersistMode::ExplicitlyRevoked)
                        .set_restore_token(token.as_deref()),
                )
                .await?;
            let resp = proxy.start(&session, None, Default::default()).await?.response()?;
            let stream = resp.streams().first().cloned().ok_or_else(|| anyhow!("portal returned no stream"))?;
            let new_token = resp.restore_token().map(str::to_owned);
            anyhow::Ok((proxy, session, stream, new_token))
        })?;
        if let (Some(path), Some(t)) = (token_file, new_token) {
            let _ = std::fs::create_dir_all(path.parent().unwrap());
            let _ = std::fs::write(path, t);
        }
        let fd = self.rt.block_on(proxy.open_pipe_wire_remote(&session, Default::default()))?;
        println!("portal stream node {} ({:?})", stream.pipe_wire_node_id(), stream.size());
        Ok(PortalCast { proxy, session, fd, node: stream.pipe_wire_node_id(), mode: (cfg.width, cfg.height) })
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
                // videoscale really scales. keepalive-time resends the last frame while the
                // screen is idle, so the encoder keeps sharpening it and a frame skipped by
                // flow control still lands.
                format!(
                    "pipewiresrc fd={raw} path={} do-timestamp=true keepalive-time=100 \
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
    let gop = (cfg.fps * 20).to_string();
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
    match gst::glib::Value::deserialize(value, pspec.value_type()) {
        Ok(v) => el.set_property(name, v),
        Err(_) => eprintln!("note: {} does not accept {name}={value}", el.name()),
    }
}

/// Flow control + keyframe requests for one running pipeline.
struct Flow {
    in_flight: AtomicUsize,
    sink: gst_app::AppSink,
}

impl Control for Flow {
    fn ack(&self) {
        let _ = self.in_flight.fetch_update(Ordering::Relaxed, Ordering::Relaxed, |n| n.checked_sub(1));
    }
    fn request_keyframe(&self) {
        let ev = gstreamer_video::UpstreamForceKeyUnitEvent::builder().all_headers(true).build();
        self.sink.send_event(ev);
    }
    fn close(&self) {}
}

/// Stops the pipelines when the session ends.
struct Running(Vec<gst::Pipeline>);

impl Drop for Running {
    fn drop(&mut self) {
        for p in &self.0 {
            let _ = p.set_state(gst::State::Null);
        }
    }
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

impl Host for LinuxHost {
    fn start(
        &mut self,
        _args: &Args,
        cfg: &StreamConfig,
        audio_on: Arc<AtomicBool>,
        tx: mpsc::Sender<Vec<u8>>,
    ) -> Result<Stream> {
        self.idle_since = None;
        let mut last_err = match &self.encoder {
            Some(e) => anyhow!("encoder {e} is not available (see gst-inspect-1.0 {e})"),
            None => anyhow!("no H.264 encoder found (install gstreamer1.0-plugins-bad and -ugly)"),
        };
        let candidates = encoders(cfg);
        for (name, pre, props) in candidates {
            if self.encoder.as_deref().is_some_and(|e| e != name) || gst::ElementFactory::find(name).is_none() {
                continue;
            }
            let src = self.source_desc(cfg)?;
            let desc = format!(
                "{src} ! queue name=q leaky=downstream max-size-buffers=1 max-size-bytes=0 max-size-time=0 \
                 ! {pre} ! {name} name=enc \
                 ! h264parse config-interval=-1 ! video/x-h264,stream-format=byte-stream,alignment=au \
                 ! appsink name=sink sync=false"
            );
            let p = match gst::parse::launch(&desc) {
                Ok(p) => p.downcast::<gst::Pipeline>().unwrap(),
                Err(e) => {
                    last_err = anyhow!("{name}: {e}");
                    continue;
                }
            };
            let enc = p.by_name("enc").unwrap();
            for (k, v) in &props {
                set_if_supported(&enc, k, v);
            }
            let sink = p.by_name("sink").unwrap().downcast::<gst_app::AppSink>().unwrap();
            let flow = Arc::new(Flow { in_flight: AtomicUsize::new(0), sink: sink.clone() });

            // Flow control: drop frames before conversion/encoding while the tablet is behind.
            // P-frames only reference what was actually encoded, so this is always safe.
            let q_src = p.by_name("q").unwrap().static_pad("src").unwrap();
            let f = flow.clone();
            q_src.add_probe(gst::PadProbeType::BUFFER, move |_, _| {
                if f.in_flight.load(Ordering::Relaxed) >= MAX_IN_FLIGHT {
                    gst::PadProbeReturn::Drop
                } else {
                    gst::PadProbeReturn::Ok
                }
            });

            let started = Instant::now();
            let f = flow.clone();
            let tx_video = tx.clone();
            let first = Arc::new(Mutex::new(true));
            sink.set_callbacks(
                gst_app::AppSinkCallbacks::builder()
                    .new_sample(move |s| {
                        let sample = s.pull_sample().map_err(|_| gst::FlowError::Eos)?;
                        let buf = sample.buffer().ok_or(gst::FlowError::Error)?;
                        let map = buf.map_readable().map_err(|_| gst::FlowError::Error)?;
                        let patched = sps::patch_annexb(&map);
                        let au: &[u8] = patched.as_deref().unwrap_or(&map);
                        {
                            let mut first = first.lock().unwrap();
                            if *first && patched.is_none() {
                                return Ok(gst::FlowSuccess::Ok); // wait for the first SPS/IDR
                            }
                            *first = false;
                        }
                        f.in_flight.fetch_add(1, Ordering::Relaxed);
                        tx_video.send(protocol::video_msg(started.elapsed().as_micros() as u64, au)).ok();
                        Ok(gst::FlowSuccess::Ok)
                    })
                    .build(),
            );

            match wait_playing(&p) {
                Ok(()) => {
                    println!("encoding with {name}");
                    let mut pipelines = vec![p];
                    match start_audio(audio_on.clone(), tx.clone()) {
                        Ok(a) => pipelines.push(a),
                        Err(e) => eprintln!("audio unavailable: {e:#}"),
                    }
                    return Ok(Stream { control: flow, guard: Box::new(Running(pipelines)) });
                }
                Err(e) => {
                    let _ = p.set_state(gst::State::Null);
                    eprintln!("{name} failed: {e:#}");
                    last_err = e.context(name);
                }
            }
        }
        Err(last_err)
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
            let _ = self.rt.block_on(p.session.close());
            drop(p.proxy);
            println!("virtual monitor removed");
        }
        self.idle_since = None;
    }
}

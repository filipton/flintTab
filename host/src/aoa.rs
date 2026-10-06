//! Raw USB transport: Android Open Accessory (AOA), the protocol wired Android Auto uses.
//!
//! adb relays the stream through the adb server, adbd and a loopback socket on the tablet,
//! and acknowledges every packet: ~4 ms round trips. AOA hands the tablet app the USB bulk
//! endpoints directly; the round trip is a fraction of a millisecond. With USB debugging on,
//! the tablet keeps adb next to the accessory (product 0x2D01), so installing and launching
//! the app over adb still works.

use rusb::{Context, DeviceHandle, Direction, TransferType, UsbContext};
use std::{
    io::{self, Read, Write},
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    thread,
    time::{Duration, Instant},
};

const GOOGLE: u16 = 0x18d1;
const ACCESSORY: std::ops::RangeInclusive<u16> = 0x2d00..=0x2d05;
const GET_PROTOCOL: u8 = 51;
const SEND_STRING: u8 = 52;
const START: u8 = 53;
const IO_TIMEOUT: Duration = Duration::from_millis(200);

/// What the tablet app's accessory filter (res/xml/accessory_filter.xml) matches.
pub const MANUFACTURER: &str = "tabdisplay";
pub const MODEL: &str = "TabDisplay";

/// An open accessory: the bulk endpoint pair the tablet app reads and writes.
pub struct Link {
    handle: Arc<DeviceHandle<Context>>,
    ep_in: u8,
    ep_out: u8,
    max_packet: usize,
    /// A handshake that arrived during a session, for the next one.
    next: Arc<std::sync::Mutex<Vec<u8>>>,
}

fn serial_of<T: UsbContext>(dev: &rusb::Device<T>) -> Option<String> {
    let desc = dev.device_descriptor().ok()?;
    let h = open_dev(dev)?;
    h.read_serial_number_string_ascii(&desc).ok()
}

/// An Android device with USB debugging on (it has an adb interface); no need to open it.
fn has_adb<T: UsbContext>(dev: &rusb::Device<T>) -> bool {
    dev.active_config_descriptor().is_ok_and(|c| {
        c.interfaces().any(|i| {
            i.descriptors().any(|d| (d.class_code(), d.sub_class_code(), d.protocol_code()) == (0xff, 0x42, 0x01))
        })
    })
}

/// Opens a USB device; on Linux without permission, says once how to grant it.
fn open_dev<T: UsbContext>(dev: &rusb::Device<T>) -> Option<DeviceHandle<T>> {
    match dev.open() {
        Ok(h) => Some(h),
        Err(rusb::Error::Access) => {
            static TOLD: AtomicBool = AtomicBool::new(false);
            if cfg!(target_os = "linux") && !TOLD.swap(true, Ordering::Relaxed) {
                let vendor = dev.device_descriptor().map(|d| format!("{:04x}", d.vendor_id())).unwrap_or_default();
                eprintln!(
                    "raw USB: no permission to open the tablet, staying on adb (slower). To allow it:\n  \
                     echo 'SUBSYSTEM==\"usb\", ATTR{{idVendor}}==\"{vendor}\", TAG+=\"uaccess\"\n\
                     SUBSYSTEM==\"usb\", ATTR{{idVendor}}==\"18d1\", ATTR{{idProduct}}==\"2d0?\", TAG+=\"uaccess\"' \\\n    \
                     | sudo tee /etc/udev/rules.d/70-tabdisplay.rules && sudo udevadm control --reload && sudo udevadm trigger\n  \
                     then unplug and replug the tablet."
                );
            }
            None
        }
        Err(_) => None,
    }
}

/// Switches the device with this adb serial to accessory mode (it re-enumerates).
fn switch(ctx: &Context, serial: &str) -> bool {
    let Ok(devices) = ctx.devices() else { return false };
    for dev in devices.iter() {
        let Ok(desc) = dev.device_descriptor() else { continue };
        if desc.vendor_id() == GOOGLE && ACCESSORY.contains(&desc.product_id()) {
            continue;
        }
        if !has_adb(&dev) || serial_of(&dev).as_deref() != Some(serial) {
            continue;
        }
        let Some(h) = open_dev(&dev) else { return false };
        let mut v = [0u8; 2];
        let t = Duration::from_secs(1);
        let in_vendor = rusb::request_type(Direction::In, rusb::RequestType::Vendor, rusb::Recipient::Device);
        let out_vendor = rusb::request_type(Direction::Out, rusb::RequestType::Vendor, rusb::Recipient::Device);
        if h.read_control(in_vendor, GET_PROTOCOL, 0, 0, &mut v, t).is_err() || u16::from_le_bytes(v) == 0 {
            return false; // no AOA support
        }
        let strings = [MANUFACTURER, MODEL, "Tablet display over USB", "3", "https://github.com/filipton/macos-usb-display", serial];
        for (i, s) in strings.iter().enumerate() {
            let mut b = s.as_bytes().to_vec();
            b.push(0);
            if h.write_control(out_vendor, SEND_STRING, 0, i as u16, &b, t).is_err() {
                return false;
            }
        }
        return h.write_control(out_vendor, START, 0, 0, &[], t).is_ok();
    }
    false
}

/// The tablet with adb serial `serial` as an accessory, switching it first if needed.
pub fn open(serial: &str) -> Option<Link> {
    let ctx = Context::new().ok()?;
    let find = || -> Option<rusb::Device<Context>> {
        let devices = ctx.devices().ok()?;
        let acc: Vec<_> = devices
            .iter()
            .filter(|d| d.device_descriptor().is_ok_and(|x| x.vendor_id() == GOOGLE && ACCESSORY.contains(&x.product_id())))
            .collect();
        // The accessory keeps the adb serial. Only if it cannot be read does a single accessory
        // do: another tablet may still be an accessory from an earlier session.
        acc.iter().find(|d| serial_of(d).as_deref() == Some(serial)).cloned().or_else(|| {
            (acc.len() == 1 && serial_of(&acc[0]).is_none()).then(|| acc[0].clone())
        })
    };
    let dev = match find() {
        Some(d) => d,
        None => {
            if !switch(&ctx, serial) {
                return None;
            }
            let until = Instant::now() + Duration::from_secs(5);
            loop {
                thread::sleep(Duration::from_millis(200));
                if let Some(d) = find() {
                    break d;
                }
                if Instant::now() > until {
                    return None;
                }
            }
        }
    };
    // The accessory interface: the one with a bulk IN and a bulk OUT endpoint (not adb's).
    let config = dev.active_config_descriptor().ok()?;
    let (iface, ep_in, ep_out, max_packet) = config.interfaces().find_map(|i| {
        let d = i.descriptors().next()?;
        if d.class_code() == 0xff && d.sub_class_code() == 0x42 {
            return None; // adb
        }
        let bulk = |dir| d.endpoint_descriptors().find(|e| e.transfer_type() == TransferType::Bulk && e.direction() == dir);
        let (i_, o) = (bulk(Direction::In)?, bulk(Direction::Out)?);
        Some((d.interface_number(), i_.address(), o.address(), o.max_packet_size() as usize))
    })?;
    let handle = open_dev(&dev)?;
    handle.claim_interface(iface).ok()?;
    Some(Link { handle: Arc::new(handle), ep_in, ep_out, max_packet, next: Default::default() })
}

impl Link {
    /// A reader for the next session: it starts with the handshake an earlier reader ran into.
    pub fn reader(&self) -> LinkReader {
        let mut buf = vec![0; 16384];
        let carried = std::mem::take(&mut *self.next.lock().unwrap());
        buf[..carried.len()].copy_from_slice(&carried);
        LinkReader {
            handle: self.handle.clone(),
            ep: self.ep_in,
            buf,
            pos: 0,
            len: carried.len(),
            closed: Arc::new(AtomicBool::new(false)),
            in_session: false,
            next: self.next.clone(),
        }
    }

    /// The tablet app reconnected during the last session (its handshake is waiting).
    pub fn reconnected(&self) -> bool {
        !self.next.lock().unwrap().is_empty()
    }

    pub fn writer(&self) -> LinkWriter {
        LinkWriter { handle: self.handle.clone(), ep: self.ep_out, max_packet: self.max_packet }
    }
}

pub struct LinkReader {
    handle: Arc<DeviceHandle<Context>>,
    ep: u8,
    buf: Vec<u8>,
    pos: usize,
    len: usize,
    closed: Arc<AtomicBool>,
    /// Past the handshake: a new one means the app reconnected.
    in_session: bool,
    next: Arc<std::sync::Mutex<Vec<u8>>>,
}

impl LinkReader {
    /// Makes this reader fail from its next timeout on (the session is over).
    pub fn closer(&self) -> impl Fn() + Send + 'static {
        let c = self.closed.clone();
        move || c.store(true, Ordering::Relaxed)
    }

    /// Waits until the tablet app sends its handshake (a transfer starting with the magic;
    /// anything before it is left over from an earlier app instance), or `stop()` says so.
    pub fn wait(&mut self, stop: impl Fn() -> bool) -> io::Result<()> {
        while !self.buf[self.pos..self.len].starts_with(crate::protocol::MAGIC) {
            if stop() {
                return Err(io::ErrorKind::Interrupted.into());
            }
            self.pos = self.len;
            self.fill()?;
        }
        // An app that lost its host retries every few seconds, each time with a new handshake
        // that waits in the pipe: take the newest, and what the app sent after it (its log).
        let mut more = vec![0; self.buf.len()];
        while let Ok(n) = self.handle.read_bulk(self.ep, &mut more, Duration::from_millis(30)) {
            if more[..n].starts_with(crate::protocol::MAGIC) {
                self.buf[..n].copy_from_slice(&more[..n]);
                (self.pos, self.len) = (0, n);
            } else if self.len + n <= self.buf.len() {
                self.buf[self.len..self.len + n].copy_from_slice(&more[..n]);
                self.len += n;
            }
        }
        self.in_session = true;
        Ok(())
    }

    fn fill(&mut self) -> io::Result<()> {
        if self.closed.load(Ordering::Relaxed) {
            return Err(io::ErrorKind::ConnectionAborted.into());
        }
        match self.handle.read_bulk(self.ep, &mut self.buf, IO_TIMEOUT) {
            Ok(n) => {
                // The app closed the accessory and opened it again (closing it does not end
                // anything on this side): this session is over, the handshake starts the next.
                if self.in_session && self.buf[..n].starts_with(crate::protocol::MAGIC) {
                    *self.next.lock().unwrap() = self.buf[..n].to_vec();
                    self.closed.store(true, Ordering::Relaxed);
                    return Err(io::ErrorKind::ConnectionReset.into());
                }
                self.pos = 0;
                self.len = n;
                Ok(())
            }
            Err(rusb::Error::Timeout) => Ok(()),
            Err(e) => Err(io::Error::other(e)),
        }
    }
}

impl Read for LinkReader {
    fn read(&mut self, out: &mut [u8]) -> io::Result<usize> {
        while self.pos == self.len {
            self.fill()?;
        }
        let n = out.len().min(self.len - self.pos);
        out[..n].copy_from_slice(&self.buf[self.pos..self.pos + n]);
        self.pos += n;
        Ok(n)
    }
}

pub struct LinkWriter {
    handle: Arc<DeviceHandle<Context>>,
    ep: u8,
    max_packet: usize,
}

impl Write for LinkWriter {
    /// One bulk transfer per message. A transfer that fills its last packet exactly is followed
    /// by a zero-length packet, or the tablet's accessory driver would wait for more data.
    fn write(&mut self, data: &[u8]) -> io::Result<usize> {
        let mut done = 0;
        while done < data.len() {
            match self.handle.write_bulk(self.ep, &data[done..], Duration::from_secs(2)) {
                Ok(n) => done += n,
                Err(e) => return Err(io::Error::other(e)),
            }
        }
        if !data.is_empty() && data.len() % self.max_packet == 0 {
            let _ = self.handle.write_bulk(self.ep, &[], IO_TIMEOUT);
        }
        Ok(done)
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

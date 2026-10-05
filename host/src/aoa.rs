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
}

fn serial_of<T: UsbContext>(dev: &rusb::Device<T>) -> Option<String> {
    let desc = dev.device_descriptor().ok()?;
    let h = dev.open().ok()?;
    h.read_serial_number_string_ascii(&desc).ok()
}

/// Switches the device with this adb serial to accessory mode (it re-enumerates).
fn switch(ctx: &Context, serial: &str) -> bool {
    let Ok(devices) = ctx.devices() else { return false };
    for dev in devices.iter() {
        let Ok(desc) = dev.device_descriptor() else { continue };
        if desc.vendor_id() == GOOGLE && ACCESSORY.contains(&desc.product_id()) {
            continue;
        }
        if serial_of(&dev).as_deref() != Some(serial) {
            continue;
        }
        let Ok(h) = dev.open() else { return false };
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
        // The accessory keeps the adb serial; if it cannot be read, a single accessory will do.
        acc.iter().find(|d| serial_of(d).as_deref() == Some(serial)).cloned().or_else(|| (acc.len() == 1).then(|| acc[0].clone()))
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
    let handle = dev.open().ok()?;
    handle.claim_interface(iface).ok()?;
    Some(Link { handle: Arc::new(handle), ep_in, ep_out, max_packet })
}

impl Link {
    pub fn reader(&self) -> LinkReader {
        LinkReader { handle: self.handle.clone(), ep: self.ep_in, buf: vec![0; 16384], pos: 0, len: 0, closed: Arc::new(AtomicBool::new(false)) }
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
        Ok(())
    }

    fn fill(&mut self) -> io::Result<()> {
        if self.closed.load(Ordering::Relaxed) {
            return Err(io::ErrorKind::ConnectionAborted.into());
        }
        match self.handle.read_bulk(self.ep, &mut self.buf, IO_TIMEOUT) {
            Ok(n) => {
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

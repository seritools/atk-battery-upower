//! Minimal user-space HID device via `/dev/uhid` (see `linux/uhid.h`).

use std::fmt::Write as _;
use std::fs::{self, File, OpenOptions};
use std::io::{self, Read, Write};
use std::path::Path;
use std::thread;
use std::time::{Duration, Instant};

const UHID_DESTROY: u32 = 1;
const UHID_GET_REPORT: u32 = 9;
const UHID_GET_REPORT_REPLY: u32 = 10;
const UHID_CREATE2: u32 = 11;
const UHID_INPUT2: u32 = 12;
const UHID_SET_REPORT: u32 = 13;
const UHID_SET_REPORT_REPLY: u32 = 14;

pub const BUS_VIRTUAL: u16 = 0x06;
const EIO: u16 = 5;
const EVENT_SIZE: usize = 4380;
const MAX_DESCRIPTOR_SIZE: usize = 4096;
const SYSFS_DIR: &str = "/sys/devices/virtual/misc/uhid";
const BIND_TIMEOUT: Duration = Duration::from_secs(1);

pub struct DeviceInfo<'a> {
    pub name: &'a str,
    pub uniq: &'a str,
    pub bus: u16,
    pub vendor: u32,
    pub product: u32,
}

pub enum Event {
    GetReport { id: u32, rnum: u8 },
    SetReport { id: u32 },
    Other,
}

/// One HID device slot; closing it destroys the device.
pub struct Uhid(File);

impl Uhid {
    pub fn open() -> io::Result<Self> {
        OpenOptions::new()
            .read(true)
            .write(true)
            .open("/dev/uhid")
            .map(Self)
    }

    /// Waits for the driver to bind, as input sent while it's probing is dropped.
    pub fn create(&self, info: &DeviceInfo, descriptor: &[u8]) -> io::Result<()> {
        assert!(descriptor.len() <= MAX_DESCRIPTOR_SIZE);
        let mut ev = EventBuf::new(UHID_CREATE2);
        ev.put_str(128, info.name)
            .put_str(64, "") // phys
            .put_str(64, info.uniq)
            .put(&(descriptor.len() as u16).to_ne_bytes())
            .put(&info.bus.to_ne_bytes())
            .put(&info.vendor.to_ne_bytes())
            .put(&info.product.to_ne_bytes())
            .put(&0u32.to_ne_bytes()) // version
            .put(&0u32.to_ne_bytes()) // country
            .put(descriptor);
        self.send(&ev)?;

        let mut prefix = heapless::String::<24>::new();
        write!(
            prefix,
            "{:04X}:{:04X}:{:04X}.",
            info.bus, info.vendor, info.product
        )
        .unwrap();
        let deadline = Instant::now() + BIND_TIMEOUT;
        while !is_bound(&prefix) && Instant::now() < deadline {
            thread::sleep(Duration::from_millis(10));
        }
        Ok(())
    }

    pub fn destroy(&self) -> io::Result<()> {
        self.send(&EventBuf::new(UHID_DESTROY))
    }

    /// `report` starts with the report ID for numbered reports.
    pub fn input(&self, report: &[u8]) -> io::Result<()> {
        let mut ev = EventBuf::new(UHID_INPUT2);
        ev.put(&(report.len() as u16).to_ne_bytes()).put(report);
        self.send(&ev)
    }

    /// `None` fails the request.
    pub fn reply_get_report(&self, id: u32, report: Option<&[u8]>) -> io::Result<()> {
        let err = if report.is_some() { 0 } else { EIO };
        let report = report.unwrap_or_default();
        let mut ev = EventBuf::new(UHID_GET_REPORT_REPLY);
        ev.put(&id.to_ne_bytes())
            .put(&err.to_ne_bytes())
            .put(&(report.len() as u16).to_ne_bytes())
            .put(report);
        self.send(&ev)
    }

    pub fn reply_set_report(&self, id: u32, ok: bool) -> io::Result<()> {
        let err = if ok { 0 } else { EIO };
        let mut ev = EventBuf::new(UHID_SET_REPORT_REPLY);
        ev.put(&id.to_ne_bytes()).put(&err.to_ne_bytes());
        self.send(&ev)
    }

    /// Blocks until the kernel sends an event.
    pub fn read_event(&self) -> io::Result<Event> {
        let mut buf = [0u8; EVENT_SIZE];
        let n = (&self.0).read(&mut buf)?;
        if n < 10 {
            return Ok(Event::Other);
        }
        let u32_at = |i: usize| u32::from_ne_bytes(buf[i..i + 4].try_into().unwrap());
        Ok(match u32_at(0) {
            UHID_GET_REPORT => Event::GetReport {
                id: u32_at(4),
                rnum: buf[8],
            },
            UHID_SET_REPORT => Event::SetReport { id: u32_at(4) },
            _ => Event::Other,
        })
    }

    /// The kernel zero-fills short writes, so only the used prefix is sent.
    fn send(&self, ev: &EventBuf) -> io::Result<()> {
        (&self.0).write_all(&ev.buf[..ev.len])
    }
}

fn is_bound(sysfs_prefix: &str) -> bool {
    fs::read_dir(SYSFS_DIR)
        .into_iter()
        .flatten()
        .flatten()
        .any(|e| {
            let name = e.file_name();
            let Some(name) = name.to_str().filter(|n| n.starts_with(sysfs_prefix)) else {
                return false;
            };
            let mut driver = heapless::String::<128>::new();
            write!(driver, "{SYSFS_DIR}/{name}/driver").is_ok() && Path::new(&*driver).exists()
        })
}

struct EventBuf {
    buf: [u8; EVENT_SIZE],
    len: usize,
}

impl EventBuf {
    fn new(kind: u32) -> Self {
        let mut ev = Self {
            buf: [0; EVENT_SIZE],
            len: 0,
        };
        ev.put(&kind.to_ne_bytes());
        ev
    }

    fn put(&mut self, bytes: &[u8]) -> &mut Self {
        self.buf[self.len..self.len + bytes.len()].copy_from_slice(bytes);
        self.len += bytes.len();
        self
    }

    /// NUL-terminated, truncated to fit `len`.
    fn put_str(&mut self, len: usize, s: &str) -> &mut Self {
        let s = &s.as_bytes()[..s.len().min(len - 1)];
        self.put(s);
        self.len += len - s.len();
        self
    }
}

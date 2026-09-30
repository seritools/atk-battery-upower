//! Exposes the mouse battery to UPower through a virtual HID device.
//!
//! The kernel turns a HID battery usage into a `power_supply` node that UPower
//! picks up; the mouse collection makes UPower classify it as a mouse.

mod hotplug;
mod uhid;

use std::convert::Infallible;
use std::fmt;
use std::io;
use std::process::{self, ExitCode};
use std::sync::Arc;
use std::sync::atomic::{AtomicU32, Ordering::Relaxed};
use std::thread;
use std::time::{Duration, Instant};

use atk_battery::{Battery, Device, VID};
use hidapi::{HidApi, HidError};
use hotplug::Hotplug;
use uhid::{BUS_VIRTUAL, DeviceInfo, Event, Uhid};

/// Disconnects aren't reported, so the radio link is checked this often.
const LINK_POLL: Duration = Duration::from_secs(10);
/// Fallback in case a battery change goes unreported.
const BATTERY_POLL: Duration = Duration::from_secs(300);
/// While unlinked, a missed reconnect notification or a plugged-in cable is
/// noticed within this.
const RESCAN_INTERVAL: Duration = Duration::from_secs(10);

const BATTERY_REPORT_ID: u8 = 2;

#[rustfmt::skip]
const REPORT_DESCRIPTOR: &[u8] = &[
    0x05, 0x01,       // Usage Page (Generic Desktop)
    0x09, 0x02,       // Usage (Mouse)
    0xa1, 0x01,       // Collection (Application)
    0x85, 0x01,       //   Report ID (1)
    0x09, 0x01,       //   Usage (Pointer)
    0xa1, 0x00,       //   Collection (Physical)
    0x05, 0x09,       //     Usage Page (Button)
    0x19, 0x01,       //     Usage Minimum (1)
    0x29, 0x03,       //     Usage Maximum (3)
    0x15, 0x00,       //     Logical Minimum (0)
    0x25, 0x01,       //     Logical Maximum (1)
    0x95, 0x03,       //     Report Count (3)
    0x75, 0x01,       //     Report Size (1)
    0x81, 0x02,       //     Input (Data, Variable, Absolute)
    0x95, 0x01,       //     Report Count (1)
    0x75, 0x05,       //     Report Size (5)
    0x81, 0x01,       //     Input (Constant)
    0x05, 0x01,       //     Usage Page (Generic Desktop)
    0x09, 0x30,       //     Usage (X)
    0x09, 0x31,       //     Usage (Y)
    0x15, 0x81,       //     Logical Minimum (-127)
    0x25, 0x7f,       //     Logical Maximum (127)
    0x75, 0x08,       //     Report Size (8)
    0x95, 0x02,       //     Report Count (2)
    0x81, 0x06,       //     Input (Data, Variable, Relative)
    0xc0,             //   End Collection
    0x85, BATTERY_REPORT_ID, // Report ID
    0x05, 0x85,       //   Usage Page (Battery System)
    0x09, 0x65,       //   Usage (Absolute State Of Charge)
    0x15, 0x00,       //   Logical Minimum (0)
    0x25, 0x64,       //   Logical Maximum (100)
    0x75, 0x08,       //   Report Size (8)
    0x95, 0x01,       //   Report Count (1)
    0x81, 0x02,       //   Input (Data, Variable, Absolute)
    0x09, 0x44,       //   Usage (Charging)
    0x25, 0x01,       //   Logical Maximum (1)
    0x75, 0x01,       //   Report Size (1)
    0x81, 0x02,       //   Input (Data, Variable, Absolute)
    0x75, 0x07,       //   Report Size (7)
    0x81, 0x01,       //   Input (Constant)
    0xc0,             // End Collection
];

const DEVICE: DeviceInfo = DeviceInfo {
    name: "ATK A9 Ultimate (battery)",
    uniq: "atk-a9-ultimate-battery",
    bus: BUS_VIRTUAL,
    vendor: VID as u32,
    product: 0,
};

struct State {
    uhid: Uhid,
    /// Last battery report, zero-padded to 4 bytes; 0 while the virtual
    /// device doesn't exist.
    report: AtomicU32,
}

fn main() -> ExitCode {
    let Err(e) = run();
    eprintln!("{e}");
    ExitCode::FAILURE
}

enum Fatal {
    HidInit(HidError),
    UhidOpen(io::Error),
    Uhid(io::Error),
    Hotplug(io::Error),
}

impl fmt::Display for Fatal {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::HidInit(e) => write!(f, "hidapi init failed: {e}"),
            Self::UhidOpen(e) => write!(f, "can't open /dev/uhid: {e}"),
            Self::Uhid(e) => write!(f, "uhid write failed: {e}"),
            Self::Hotplug(e) => write!(f, "udev monitor failed: {e}"),
        }
    }
}

fn run() -> Result<Infallible, Fatal> {
    let mut api = HidApi::new().map_err(Fatal::HidInit)?;
    let uhid = Uhid::open().map_err(Fatal::UhidOpen)?;
    let hotplug = Hotplug::new().map_err(Fatal::Hotplug)?;
    let state = Arc::new(State {
        uhid,
        report: AtomicU32::new(0),
    });

    let reader = Arc::clone(&state);
    thread::spawn(move || {
        let Err(e) = serve_requests(&reader);
        eprintln!("uhid read failed: {e}");
        process::exit(1);
    });

    let mut devices = Vec::new();
    let mut rescan = true;
    let mut has_receiver = false;
    let mut has_wired = false;
    // whether the failed device was the receiver, and why
    let mut lost: Option<(bool, HidError)> = None;
    loop {
        // every path below sets `rescan` for the next iteration
        if hotplug.pending() || rescan {
            match api.refresh_devices() {
                Ok(()) => Device::open_all(&api, &mut devices),
                Err(_) => devices.clear(),
            }
            let receiver = devices.iter().any(Device::is_receiver);
            log_presence("receiver", &mut has_receiver, receiver);
            let wired = devices.iter().any(|d| !d.is_receiver());
            log_presence("wired mouse", &mut has_wired, wired);
            // unplugging is reported above, anything else is an error
            if let Some((was_receiver, e)) = lost.take()
                && devices.iter().any(|d| d.is_receiver() == was_receiver)
            {
                eprintln!("hid error: {e}");
            }
        }
        if let Some(dev) = devices.iter().find(|d| d.is_linked().unwrap_or(false)) {
            match track(dev, &state) {
                // the link just dropped, no need to ask again
                Ok(()) => {}
                Err(Stop::DeviceLost(e)) => {
                    lost = Some((dev.is_receiver(), e));
                    rescan = true;
                    continue;
                }
                Err(Stop::Uhid(e)) => return Err(Fatal::Uhid(e)),
            }
        }
        remove(&state).map_err(Fatal::Uhid)?;
        match devices.iter().find(|d| d.is_receiver()) {
            // the mouse notifies the receiver when it links up again
            Some(r) => rescan = r.wait_battery_changed(RESCAN_INTERVAL).is_err(),
            None => {
                hotplug.wait().map_err(Fatal::Hotplug)?;
                rescan = true;
            }
        }
    }
}

fn log_presence(what: &str, was: &mut bool, is: bool) {
    if *was != is {
        *was = is;
        let change = if is { "connected" } else { "disconnected" };
        eprintln!("{what} {change}");
    }
}

enum Stop {
    DeviceLost(HidError),
    Uhid(io::Error),
}

impl From<HidError> for Stop {
    fn from(e: HidError) -> Self {
        Self::DeviceLost(e)
    }
}

impl From<io::Error> for Stop {
    fn from(e: io::Error) -> Self {
        Self::Uhid(e)
    }
}

/// Expects the mouse to be linked through `dev`, returns once it no longer is.
fn track(dev: &Device, state: &State) -> Result<(), Stop> {
    let mut next_poll = Instant::now();
    loop {
        if Instant::now() >= next_poll
            && let Some(battery) = dev.battery()?
        {
            publish(state, battery)?;
            next_poll = Instant::now() + BATTERY_POLL;
        }
        if dev.wait_battery_changed(LINK_POLL)? {
            next_poll = Instant::now();
        }
        if !dev.is_linked()? {
            return Ok(());
        }
    }
}

fn publish(state: &State, battery: Battery) -> io::Result<()> {
    let report = [
        BATTERY_REPORT_ID,
        battery.percent.min(100),
        battery.charging.into(),
        0,
    ];
    let packed = u32::from_ne_bytes(report);
    // set before the first input, which the kernel may drop while still
    // probing; it then asks via GET_REPORT instead
    if state.report.swap(packed, Relaxed) == 0 {
        state.uhid.create(&DEVICE, REPORT_DESCRIPTOR)?;
    }
    state.uhid.input(&report[..3])
}

fn remove(state: &State) -> io::Result<()> {
    if state.report.swap(0, Relaxed) != 0 {
        state.uhid.destroy()?;
    }
    Ok(())
}

fn serve_requests(state: &State) -> io::Result<Infallible> {
    loop {
        match state.uhid.read_event()? {
            Event::GetReport { id, rnum } => {
                let report = state.report.load(Relaxed).to_ne_bytes();
                let valid = rnum == BATTERY_REPORT_ID && report[0] == BATTERY_REPORT_ID;
                state
                    .uhid
                    .reply_get_report(id, valid.then_some(&report[..3]))?;
            }
            Event::SetReport { id } => state.uhid.reply_set_report(id, false)?,
            Event::Other => {}
        }
    }
}

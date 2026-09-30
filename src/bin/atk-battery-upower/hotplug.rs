//! Notices the mouse's hidraw nodes appearing, via udev.

use std::io;
use std::os::fd::AsRawFd;

use udev::{EventType, MonitorBuilder, MonitorSocket};

/// Set by `contrib/50-usb-atkmouse.rules`; the kernel filters on it, so other
/// devices don't wake us.
const TAG: &str = "atk-battery";

pub struct Hotplug(MonitorSocket);

impl Hotplug {
    pub fn new() -> io::Result<Self> {
        MonitorBuilder::new()?
            .match_subsystem("hidraw")?
            .match_tag(TAG)?
            .listen()
            .map(Self)
    }

    /// Blocks until a node appears or changes.
    pub fn wait(&self) -> io::Result<()> {
        loop {
            let mut pfd = libc::pollfd {
                fd: self.0.as_raw_fd(),
                events: libc::POLLIN,
                revents: 0,
            };
            if unsafe { libc::poll(&mut pfd, 1, -1) } < 0 {
                let e = io::Error::last_os_error();
                if e.kind() != io::ErrorKind::Interrupted {
                    return Err(e);
                }
            }
            if self.pending() {
                return Ok(());
            }
        }
    }

    /// Whether a node appeared or changed since the last call, without
    /// blocking. Events arrive after udev applied permissions.
    pub fn pending(&self) -> bool {
        // drains the whole queue, unlike `any`
        let added = self
            .0
            .iter()
            .filter(|e| e.event_type() != EventType::Remove);
        added.count() > 0
    }
}

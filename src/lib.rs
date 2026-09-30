//! Query battery status from the ATK A9 Ultimate (and/or its dongle) via HID.

use std::cell::Cell;
use std::fmt;
use std::time::{Duration, Instant};

use hidapi::{HidApi, HidDevice, HidResult};

pub const VID: u16 = 0x373b;
const MOUSE_PID: u16 = 0x11b6;
const RECEIVER_PID: u16 = 0x11d9;
const USAGE_PAGE: u16 = 0xff04;
const USAGE: u16 = 0x02;

const REPORT_ID: u8 = 0x08;
const CMD_GET_MOUSE_ONLINE: u8 = 0x03;
const CMD_GET_BATTERY: u8 = 0x04;
const CMD_REPORT_MOUSE_STATUS: u8 = 0x0a;
const STATUS_BATTERY_CHANGED: u8 = 0x40;
const CHECKSUM_BASE: u8 = 0x55;
const PAYLOAD_LEN: usize = 16;
/// Command ID, status, EEPROM address (2), data length, then data.
const DATA_OFFSET: usize = 5;
const REPLY_TIMEOUT: Duration = Duration::from_secs(1);

type Payload = [u8; PAYLOAD_LEN];

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Battery {
    pub percent: u8,
    pub charging: bool,
    pub millivolts: u16,
}

impl Battery {
    fn parse(payload: &[u8]) -> Option<Self> {
        let [
            CMD_GET_BATTERY,
            _,
            _,
            _,
            _,
            percent,
            charging,
            mv_hi,
            mv_lo,
            ..,
        ] = *payload
        else {
            return None;
        };
        Some(Self {
            percent,
            charging: charging != 0,
            millivolts: u16::from_be_bytes([mv_hi, mv_lo]),
        })
    }
}

impl fmt::Display for Battery {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let state = if self.charging {
            "charging"
        } else {
            "discharging"
        };
        write!(
            f,
            "battery {}%  {}  {} mV",
            self.percent, state, self.millivolts
        )
    }
}

/// Report ID followed by the payload, whose last byte makes the sum of
/// everything (report ID included) equal `CHECKSUM_BASE` mod 256.
fn request(cmd: u8) -> [u8; 1 + PAYLOAD_LEN] {
    let mut out = [0u8; 1 + PAYLOAD_LEN];
    out[0] = REPORT_ID;
    out[1] = cmd;
    let sum = out.iter().fold(0u8, |acc, &b| acc.wrapping_add(b));
    out[PAYLOAD_LEN] = CHECKSUM_BASE.wrapping_sub(sum);
    out
}

/// The vendor interface of the receiver or the wired mouse.
pub struct Device {
    dev: HidDevice,
    receiver: bool,
    battery_changed: Cell<bool>,
}

impl Device {
    /// Replaces `devices`' contents, wired mouse first: plugging it in drops
    /// its link to the receiver.
    pub fn open_all(api: &HidApi, devices: &mut Vec<Self>) {
        devices.clear();
        devices.extend(
            api.device_list()
                .filter(|i| {
                    i.vendor_id() == VID
                        && [MOUSE_PID, RECEIVER_PID].contains(&i.product_id())
                        && i.usage_page() == USAGE_PAGE
                        && i.usage() == USAGE
                })
                .filter_map(|i| {
                    Some(Self {
                        dev: api.open_path(i.path()).ok()?,
                        receiver: i.product_id() == RECEIVER_PID,
                        battery_changed: Cell::new(false),
                    })
                }),
        );
        devices.sort_unstable_by_key(|d| d.receiver);
    }

    pub fn is_receiver(&self) -> bool {
        self.receiver
    }

    /// `Ok(None)` if the mouse doesn't answer, e.g. while asleep or off.
    pub fn battery(&self) -> HidResult<Option<Battery>> {
        Ok(self
            .command(CMD_GET_BATTERY)?
            .and_then(|p| Battery::parse(&p)))
    }

    /// Whether the receiver has a radio link to the mouse, which drops within
    /// a second of it turning off or falling asleep. Always true when wired.
    pub fn is_linked(&self) -> HidResult<bool> {
        if !self.receiver {
            return Ok(true);
        }
        let reply = self.command(CMD_GET_MOUSE_ONLINE)?;
        Ok(reply.is_some_and(|p| p[DATA_OFFSET] == 1))
    }

    /// Waits up to `timeout` for the mouse to report a battery change, which
    /// it also does when it (re)connects to the receiver.
    pub fn wait_battery_changed(&self, timeout: Duration) -> HidResult<bool> {
        let deadline = Instant::now() + timeout;
        while !self.battery_changed.get() && self.read_until(deadline)?.is_some() {}
        Ok(self.battery_changed.take())
    }

    /// Payload of the reply to `cmd`, `None` on timeout.
    fn command(&self, cmd: u8) -> HidResult<Option<Payload>> {
        self.dev.write(&request(cmd))?;
        let deadline = Instant::now() + REPLY_TIMEOUT;
        while let Some(payload) = self.read_until(deadline)? {
            if payload[0] == cmd {
                return Ok(Some(payload));
            }
        }
        Ok(None)
    }

    /// Next vendor report before `deadline`, recording status notifications.
    fn read_until(&self, deadline: Instant) -> HidResult<Option<Payload>> {
        let mut buf = [0u8; 64];
        loop {
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                return Ok(None);
            }
            let n = self
                .dev
                .read_timeout(&mut buf, remaining.as_millis().max(1) as i32)?;
            let report = &buf[..n];
            // the report ID prefix is present on multi-report interfaces
            let payload = report.strip_prefix(&[REPORT_ID]).unwrap_or(report);
            let Some(&payload) = payload.first_chunk::<PAYLOAD_LEN>() else {
                continue;
            };
            if payload[0] == CMD_REPORT_MOUSE_STATUS
                && payload[DATA_OFFSET] & STATUS_BATTERY_CHANGED != 0
            {
                self.battery_changed.set(true);
            }
            return Ok(Some(payload));
        }
    }
}

pub fn read_battery(api: &HidApi) -> Option<Battery> {
    let mut devices = Vec::new();
    Device::open_all(api, &mut devices);
    devices.iter().find_map(|d| d.battery().ok().flatten())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn request_checksum() {
        let req = request(CMD_GET_BATTERY);
        assert_eq!(req[..2], [REPORT_ID, CMD_GET_BATTERY]);
        assert_eq!(req[PAYLOAD_LEN], 0x49);
    }

    #[test]
    fn parse_battery() {
        let p = [CMD_GET_BATTERY, 0, 0, 0, 0, 87, 1, 0x0f, 0xa0];
        assert_eq!(
            Battery::parse(&p),
            Some(Battery {
                percent: 87,
                charging: true,
                millivolts: 4000
            })
        );
        assert_eq!(Battery::parse(&p[..8]), None);
        assert_eq!(Battery::parse(&[0x05, 0, 0, 0, 0, 87, 1, 0x0f, 0xa0]), None);
    }
}

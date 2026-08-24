//! Keyboard backlight, mic mute LED and fn lock over Bluetooth.
//!
//! The keyboard accepts the same vendor HID feature reports over Bluetooth as it does
//! over USB, so the payloads here are the ones `keyboard_usb` already sends. Only the
//! transport differs: an HIDIOCSFEATURE ioctl on the Bluetooth hidraw node instead of a
//! USB control transfer.

use std::fs;
use std::os::fd::AsRawFd as _;
use std::path::PathBuf;

use log::{debug, warn};

use crate::parse_hex_string;
use crate::state::KeyboardBacklightState;

// HIDIOCSFEATURE(len) = _IOC(READ | WRITE, 'H', 0x06, len)
nix::ioctl_readwrite_buf!(hid_set_feature, b'H', 0x06, u8);
// HIDIOCGFEATURE(len) = _IOC(READ | WRITE, 'H', 0x07, len)
nix::ioctl_readwrite_buf!(hid_get_feature, b'H', 0x07, u8);

/// Bluetooth bus, hid-generic keyboard interface, ASUS vendor. The sibling `g0004` node
/// is the touchpad and ignores these reports. The product id is deliberately not part of
/// the match: it differs per model (`1b2d` on the 2024, `1bf3` on the 2025) and no other
/// ASUS Bluetooth keyboard interface turns up on this machine.
const MODALIAS_PREFIX: &str = "hid:b0005g0001v00000B05";

/// Resolved on every write rather than cached, because hidraw node numbers are handed
/// out in connection order and change when the keyboard reconnects.
fn find_bt_hidraw() -> Option<PathBuf> {
    for entry in fs::read_dir("/sys/class/hidraw").ok()?.flatten() {
        let modalias = fs::read_to_string(entry.path().join("device/modalias")).unwrap_or_default();
        if modalias.trim().starts_with(MODALIAS_PREFIX) {
            return Some(PathBuf::from("/dev").join(entry.file_name()));
        }
    }
    None
}

fn send_feature_report(hex: &str, what: &str) {
    let Some(node) = find_bt_hidraw() else {
        debug!("No Bluetooth keyboard hidraw node, skipping {what}");
        return;
    };

    let file = match fs::OpenOptions::new().read(true).write(true).open(&node) {
        Ok(file) => file,
        Err(e) => {
            warn!("Failed to open {} for {}: {}", node.display(), what, e);
            return;
        }
    };

    let mut data = parse_hex_string(hex);
    // SAFETY: HIDIOCSFEATURE reads data.len() bytes out of a buffer we own, and the
    // length is encoded into the request number by the ioctl_readwrite_buf macro.
    match unsafe { hid_set_feature(file.as_raw_fd(), &mut data) } {
        Ok(_) => debug!("Sent {} over Bluetooth on {}", what, node.display()),
        Err(e) => warn!("Failed to send {} over Bluetooth: {}", what, e),
    }
}

/// Reads report 0x5a back. The keyboard has no state registers: this returns whichever
/// report was last written to 0x5a, so the caller has to check the prefix before trusting
/// what it finds.
fn read_feature_report() -> Option<[u8; 16]> {
    let node = find_bt_hidraw()?;
    let file = fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(&node)
        .inspect_err(|e| warn!("Failed to open {} to read: {}", node.display(), e))
        .ok()?;

    let mut data = [0u8; 16];
    data[0] = 0x5a;
    // SAFETY: HIDIOCGFEATURE writes at most data.len() bytes into a buffer we own, and the
    // length is encoded into the request number by the ioctl_readwrite_buf macro.
    match unsafe { hid_get_feature(file.as_raw_fd(), &mut data) } {
        Ok(_) => Some(data),
        Err(e) => {
            warn!("Failed to read feature report over Bluetooth: {}", e);
            None
        }
    }
}

/// The backlight level the keyboard is actually at, or `None` when the last report
/// written to 0x5a was something else (a mic mute write, say) and the level is unknown.
pub fn read_backlight_state() -> Option<KeyboardBacklightState> {
    parse_backlight_report(&read_feature_report()?)
}

/// Shared with the wired path, which gets the same reply out of a USB GET_REPORT.
pub fn parse_backlight_report(report: &[u8]) -> Option<KeyboardBacklightState> {
    if report.len() < 5 || report[..4] != [0x5a, 0xba, 0xc5, 0xc4] {
        debug!(
            "Report 0x5a holds {:02x?}, not a backlight level",
            &report[..4]
        );
        return None;
    }
    match report[4] {
        0 => Some(KeyboardBacklightState::Off),
        1 => Some(KeyboardBacklightState::Low),
        2 => Some(KeyboardBacklightState::Medium),
        3 => Some(KeyboardBacklightState::High),
        other => {
            warn!("Unknown backlight level {} in report 0x5a", other);
            None
        }
    }
}

fn backlight_report(state: KeyboardBacklightState) -> &'static str {
    match state {
        KeyboardBacklightState::Off => "5abac5c4000000000000000000000000",
        KeyboardBacklightState::Low => "5abac5c4010000000000000000000000",
        KeyboardBacklightState::Medium => "5abac5c4020000000000000000000000",
        KeyboardBacklightState::High => "5abac5c4030000000000000000000000",
    }
}

fn mic_mute_report(enabled: bool) -> &'static str {
    if enabled {
        "5ad07c01000000000000000000000000"
    } else {
        "5ad07c00000000000000000000000000"
    }
}

/// `true` means the F1-F12 row sends its media functions and Fn is needed for F1-F12,
/// which is what ASUS calls Fn-Lock *off*. Same payload `keyboard_usb` sends on attach.
pub fn fn_lock_report(fn_lock: bool) -> &'static str {
    if fn_lock {
        "5ad04e00000000000000000000000000"
    } else {
        "5ad04e01000000000000000000000000"
    }
}

pub fn send_backlight_state(state: KeyboardBacklightState) {
    send_feature_report(backlight_report(state), "backlight state");
}

pub fn send_mic_mute_state(enabled: bool) {
    send_feature_report(mic_mute_report(enabled), "mic mute state");
}

pub fn send_fn_lock_state(fn_lock: bool) {
    send_feature_report(fn_lock_report(fn_lock), "fn lock state");
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The reports are hand written hex, and a typo in one of them is silent: the ioctl
    /// still succeeds, the keyboard just ignores it.
    #[test]
    fn reports_are_16_byte_frames_carrying_the_right_level() {
        let cases = [
            (KeyboardBacklightState::Off, 0),
            (KeyboardBacklightState::Low, 1),
            (KeyboardBacklightState::Medium, 2),
            (KeyboardBacklightState::High, 3),
        ];
        for (state, level) in cases {
            let report = parse_hex_string(backlight_report(state));
            assert_eq!(report.len(), 16, "{state:?} report is not 16 bytes");
            assert_eq!(
                report[..4],
                [0x5a, 0xba, 0xc5, 0xc4],
                "{state:?} wrong prefix"
            );
            assert_eq!(report[4], level, "{state:?} wrong level byte");
            assert!(
                report[5..].iter().all(|b| *b == 0),
                "{state:?} dirty padding"
            );
        }

        for (enabled, expected) in [(true, 1), (false, 0)] {
            let report = parse_hex_string(mic_mute_report(enabled));
            assert_eq!(report.len(), 16);
            assert_eq!(report[..3], [0x5a, 0xd0, 0x7c]);
            assert_eq!(report[3], expected);
        }

        for (fn_lock, expected) in [(true, 0), (false, 1)] {
            let report = parse_hex_string(fn_lock_report(fn_lock));
            assert_eq!(report.len(), 16);
            assert_eq!(report[..3], [0x5a, 0xd0, 0x4e]);
            assert_eq!(report[3], expected);
        }
    }

    fn report(hex: &str) -> [u8; 16] {
        parse_hex_string(hex).try_into().unwrap()
    }

    /// Bytes captured off a real keyboard: report 0x5a hands back the last report written
    /// to it, which is a backlight level only some of the time.
    #[test]
    fn a_read_back_report_is_only_trusted_when_it_is_a_backlight_one() {
        let levels = [
            (KeyboardBacklightState::Off, "5abac5c400"),
            (KeyboardBacklightState::Low, "5abac5c401"),
            (KeyboardBacklightState::Medium, "5abac5c402"),
            (KeyboardBacklightState::High, "5abac5c403"),
        ];
        for (state, prefix) in levels {
            let hex = format!("{prefix}{}", "0".repeat(32 - prefix.len()));
            assert_eq!(parse_backlight_report(&report(&hex)), Some(state));
        }

        // A mic mute report, and an unknown level, are both "level unknown".
        assert_eq!(
            parse_backlight_report(&report("5ad07c00000000000000000000000000")),
            None
        );
        assert_eq!(
            parse_backlight_report(&report("5abac5c409000000000000000000000000"[..32].into())),
            None
        );
    }
}

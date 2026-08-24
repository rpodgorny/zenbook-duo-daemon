use crate::events::Event;
use log::warn;
use std::sync::{Arc, RwLock};
use tokio::sync::broadcast;

/// The backlight level outlives the daemon: on connect we push our idea of it to the
/// keyboard, so without this a restart would light up a keyboard the user had turned off.
const BACKLIGHT_PATH: &str = "/var/lib/zenbook-duo-daemon/backlight";

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum KeyboardBacklightState {
    Off,
    Low,
    Medium,
    High,
}

impl KeyboardBacklightState {
    fn as_str(&self) -> &'static str {
        match self {
            Self::Off => "off",
            Self::Low => "low",
            Self::Medium => "medium",
            Self::High => "high",
        }
    }

    fn parse(s: &str) -> Option<Self> {
        match s {
            "off" => Some(Self::Off),
            "low" => Some(Self::Low),
            "medium" => Some(Self::Medium),
            "high" => Some(Self::High),
            _ => None,
        }
    }

    /// Falls back to `Low` for a missing or unreadable file, which is what the daemon
    /// used to start with unconditionally.
    fn load() -> Self {
        std::fs::read_to_string(BACKLIGHT_PATH)
            .ok()
            .and_then(|s| Self::parse(s.trim()))
            .unwrap_or(Self::Low)
    }

    /// ponytail: plain blocking write on every level change, a few bytes a keypress. A
    /// half written file just reads back as `Low` next boot; make it write-rename if that
    /// ever bites.
    fn save(&self) {
        let path = std::path::Path::new(BACKLIGHT_PATH);
        if let Some(dir) = path.parent()
            && let Err(e) = std::fs::create_dir_all(dir)
        {
            warn!("Failed to create {}: {}", dir.display(), e);
            return;
        }
        if let Err(e) = std::fs::write(path, self.as_str()) {
            warn!("Failed to save backlight state: {}", e);
        }
    }

    pub fn next(&self) -> Self {
        match self {
            Self::Off => Self::Low,
            Self::Low => Self::Medium,
            Self::Medium => Self::High,
            Self::High => Self::Off,
        }
    }
}

/// Inner state structure containing all keyboard state
struct InnerState {
    backlight: KeyboardBacklightState,
    mic_mute_led: bool,

    /// when suspended, both backlight and mic mute led are disabled
    is_suspended: bool,

    /// when idle, only backlight is disabled
    is_idle: bool,
    is_usb_attached: bool,
    is_secondary_display_enabled: bool,
}

/// Shared state manager that maintains keyboard state across attach/detach cycles
#[derive(Clone)]
pub struct KeyboardStateManager {
    state: Arc<RwLock<InnerState>>,
    sender: broadcast::Sender<Event>,
}

impl KeyboardStateManager {
    pub fn new(is_usb_attached: bool, sender: broadcast::Sender<Event>) -> Self {
        Self {
            state: Arc::new(RwLock::new(InnerState {
                backlight: KeyboardBacklightState::load(),
                mic_mute_led: false,
                is_suspended: false,
                is_idle: false,
                is_usb_attached,
                is_secondary_display_enabled: !is_usb_attached,
            })),
            sender,
        }
    }

    pub fn suspend_start(&self) {
        let mut state = self.state.write().unwrap();
        state.is_suspended = true;
        self.sender.send(Event::MicMuteLed(false)).ok();
        self.sender
            .send(Event::Backlight(KeyboardBacklightState::Off))
            .ok();
    }

    pub fn suspend_end(&self) {
        let mut state = self.state.write().unwrap();
        state.is_suspended = false;
        drop(state);
        self.sender
            .send(Event::MicMuteLed(self.get_mic_mute_led()))
            .ok();
        self.sender
            .send(Event::Backlight(self.get_keyboard_backlight()))
            .ok();
    }

    pub fn idle_start(&self) {
        let mut state = self.state.write().unwrap();
        state.is_idle = true;
        self.sender
            .send(Event::Backlight(KeyboardBacklightState::Off))
            .ok();
    }

    pub fn idle_end(&self) {
        let mut state = self.state.write().unwrap();
        state.is_idle = false;
        drop(state);
        self.sender
            .send(Event::Backlight(self.get_keyboard_backlight()))
            .ok();
    }

    pub fn set_mic_mute_led(&self, enabled: bool) {
        let mut state = self.state.write().unwrap();
        // Callers poll and re-assert the same value, so only emit on a real change.
        // Reconnecting devices push the current state directly instead of going
        // through here, so nothing depends on a redundant event.
        if state.mic_mute_led == enabled {
            return;
        }
        state.mic_mute_led = enabled;
        if !state.is_suspended {
            self.sender.send(Event::MicMuteLed(enabled)).ok();
        }
    }

    pub fn toggle_mic_mute_led(&self) {
        let mut state = self.state.write().unwrap();
        state.mic_mute_led = !state.mic_mute_led;
        if !state.is_suspended {
            self.sender.send(Event::MicMuteLed(state.mic_mute_led)).ok();
        }
    }

    pub fn get_mic_mute_led(&self) -> bool {
        let state = self.state.read().unwrap();
        if state.is_suspended {
            false
        } else {
            state.mic_mute_led
        }
    }

    pub fn set_keyboard_backlight(&self, new_state: KeyboardBacklightState) {
        let mut state = self.state.write().unwrap();
        if state.backlight == new_state {
            return;
        }
        state.backlight = new_state;
        new_state.save();
        if !state.is_idle && !state.is_suspended {
            self.sender.send(Event::Backlight(new_state)).ok();
        }
    }

    pub fn toggle_keyboard_backlight(&self) {
        let mut state = self.state.write().unwrap();
        state.backlight = state.backlight.next();
        state.backlight.save();
        if !state.is_idle && !state.is_suspended {
            self.sender.send(Event::Backlight(state.backlight)).ok();
        }
    }

    pub fn get_keyboard_backlight(&self) -> KeyboardBacklightState {
        let state = self.state.read().unwrap();
        if state.is_suspended || state.is_idle {
            KeyboardBacklightState::Off
        } else {
            state.backlight
        }
    }

    pub fn set_secondary_display(&self, enabled: bool) {
        let mut state = self.state.write().unwrap();
        state.is_secondary_display_enabled = enabled;

        if state.is_usb_attached {
            state.is_secondary_display_enabled = false;
        }

        self.sender
            .send(Event::SecondaryDisplay(state.is_secondary_display_enabled))
            .ok();
    }

    pub fn toggle_secondary_display(&self) {
        let mut state = self.state.write().unwrap();
        state.is_secondary_display_enabled = !state.is_secondary_display_enabled;

        if state.is_usb_attached {
            state.is_secondary_display_enabled = false;
        }

        self.sender
            .send(Event::SecondaryDisplay(state.is_secondary_display_enabled))
            .ok();
    }

    pub fn set_usb_keyboard_attached(&self, attached: bool) {
        let mut state = self.state.write().unwrap();
        state.is_usb_attached = attached;

        if attached {
            state.is_secondary_display_enabled = false;
        } else {
            state.is_secondary_display_enabled = true;
        }

        self.sender
            .send(Event::SecondaryDisplay(state.is_secondary_display_enabled))
            .ok();
    }

    pub fn is_secondary_display_enabled(&self) -> bool {
        let state = self.state.read().unwrap();
        state.is_secondary_display_enabled
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The two halves are hand written match arms, so a renamed level would silently
    /// start reading back as Low.
    #[test]
    fn every_backlight_level_survives_a_save_load_round_trip() {
        for state in [
            KeyboardBacklightState::Off,
            KeyboardBacklightState::Low,
            KeyboardBacklightState::Medium,
            KeyboardBacklightState::High,
        ] {
            assert_eq!(KeyboardBacklightState::parse(state.as_str()), Some(state));
        }
        assert_eq!(KeyboardBacklightState::parse("nonsense"), None);
    }

    fn drain<F: Fn(&Event) -> bool>(rx: &mut broadcast::Receiver<Event>, want: F) -> usize {
        let mut n = 0;
        while let Ok(event) = rx.try_recv() {
            if want(&event) {
                n += 1;
            }
        }
        n
    }

    /// The mute-state poller re-asserts the same value on every retry. Without change
    /// detection each retry became a real write to the keyboard, once a second forever.
    #[test]
    fn repeating_a_mic_mute_value_emits_once() {
        let (sender, mut rx) = broadcast::channel(64);
        let state = KeyboardStateManager::new(false, sender);

        state.set_mic_mute_led(true);
        state.set_mic_mute_led(true);
        state.set_mic_mute_led(true);
        assert_eq!(
            drain(&mut rx, |e| matches!(e, Event::MicMuteLed(_))),
            1,
            "repeated sets of the same value must not re-emit"
        );

        state.set_mic_mute_led(false);
        assert_eq!(
            drain(&mut rx, |e| matches!(e, Event::MicMuteLed(_))),
            1,
            "an actual change must still emit"
        );
    }

    #[test]
    fn repeating_a_backlight_value_emits_once() {
        let (sender, mut rx) = broadcast::channel(64);
        let state = KeyboardStateManager::new(false, sender);

        state.set_keyboard_backlight(KeyboardBacklightState::High);
        state.set_keyboard_backlight(KeyboardBacklightState::High);
        assert_eq!(
            drain(&mut rx, |e| matches!(e, Event::Backlight(_))),
            1,
            "repeated sets of the same value must not re-emit"
        );

        state.set_keyboard_backlight(KeyboardBacklightState::Low);
        assert_eq!(
            drain(&mut rx, |e| matches!(e, Event::Backlight(_))),
            1,
            "an actual change must still emit"
        );
    }
}

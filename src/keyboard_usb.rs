use std::{sync::Arc, time::Duration};

use futures::stream::StreamExt;
use log::{debug, info, warn};
use nusb::{
    Device, DeviceId, DeviceInfo,
    hotplug::HotplugEvent,
    transfer::{ControlIn, ControlOut, ControlType, In, Interrupt, Recipient},
};
use tokio::sync::{Mutex, broadcast, mpsc};

use crate::{
    KeyboardBacklightState, config::Config, events::Event, hidraw,
    idle_detection::ActivityNotifier, parse_hex_string, state::KeyboardStateManager,
    virtual_keyboard::VirtualKeyboard,
};

/// Why a wired keyboard task is being shut down.
#[derive(Debug, Clone, Copy)]
pub enum Shutdown {
    /// The device is gone, report the keyboard as detached.
    Detached,
    /// The device is still there and is about to be re-opened, keep the attached state.
    Reopening,
}

/// The device is not usable again the instant logind says we resumed, and the old
/// task needs a moment to drop its interface claim first.
const REOPEN_ATTEMPTS: usize = 10;
const REOPEN_RETRY_DELAY: Duration = Duration::from_millis(200);

pub async fn find_wired_keyboard(config: &Config) -> Option<DeviceInfo> {
    nusb::list_devices()
        .await
        .unwrap()
        .find(|d| d.vendor_id() == config.vendor_id() && d.product_id() == config.product_id())
}

/// Monitor USB keyboard hotplug events and start wired_keyboard_task when keyboard connects
pub fn start_usb_keyboard_monitor_task(
    config: &Config,
    mut current_keyboard: Option<(DeviceId, broadcast::Sender<Shutdown>)>,
    event_sender: broadcast::Sender<Event>,
    virtual_keyboard: Arc<Mutex<VirtualKeyboard>>,
    state_manager: KeyboardStateManager,
    activity_notifier: ActivityNotifier,
    mut reconnect_rx: mpsc::Receiver<()>,
) {
    let config = config.clone();
    tokio::spawn(async move {
        let mut watch = nusb::watch_devices().unwrap();

        loop {
            tokio::select! {
                event = watch.next() => {
                    match event {
                        Some(HotplugEvent::Connected(device))
                            if device.vendor_id() == config.vendor_id()
                                && device.product_id() == config.product_id() =>
                        {
                            current_keyboard = start_usb_keyboard_task(
                                &config,
                                device,
                                event_sender.subscribe(),
                                virtual_keyboard.clone(),
                                state_manager.clone(),
                                activity_notifier.clone(),
                            )
                            .await;
                        }
                        Some(HotplugEvent::Disconnected(device_id)) => {
                            if let Some((id, shutdown_tx)) = &current_keyboard
                                && id == &device_id
                            {
                                shutdown_tx.send(Shutdown::Detached).ok();
                                current_keyboard = None;
                            }
                        }
                        Some(_) => {}
                        None => break,
                    }
                }
                request = reconnect_rx.recv() => {
                    if request.is_none() {
                        break;
                    }
                    current_keyboard = reopen_wired_keyboard(
                        &config,
                        current_keyboard.take(),
                        &event_sender,
                        &virtual_keyboard,
                        &state_manager,
                        &activity_notifier,
                    )
                    .await;
                }
            }
        }
    });
}

/// Tear down the current wired keyboard task and open the device again.
///
/// The kernel drops our usbfs claim on interface 4 across suspend without emitting any
/// hotplug event, so nothing re-opens the device and every transfer afterwards fails
/// with EBUSY ("usbfs: did not claim interface 4 before use"). This does in software
/// what physically detaching and re-attaching the keyboard does.
async fn reopen_wired_keyboard(
    config: &Config,
    current_keyboard: Option<(DeviceId, broadcast::Sender<Shutdown>)>,
    event_sender: &broadcast::Sender<Event>,
    virtual_keyboard: &Arc<Mutex<VirtualKeyboard>>,
    state_manager: &KeyboardStateManager,
    activity_notifier: &ActivityNotifier,
) -> Option<(DeviceId, broadcast::Sender<Shutdown>)> {
    // Nothing was open. A keyboard attached during suspend arrives via hotplug instead.
    let (_, shutdown_tx) = current_keyboard?;
    shutdown_tx.send(Shutdown::Reopening).ok();

    for _ in 0..REOPEN_ATTEMPTS {
        tokio::time::sleep(REOPEN_RETRY_DELAY).await;

        let Some(keyboard) = find_wired_keyboard(config).await else {
            continue;
        };
        let keyboard = start_usb_keyboard_task(
            config,
            keyboard,
            event_sender.subscribe(),
            virtual_keyboard.clone(),
            state_manager.clone(),
            activity_notifier.clone(),
        )
        .await;
        if keyboard.is_some() {
            info!("Re-opened wired keyboard after resume");
            return keyboard;
        }
    }

    warn!("Wired keyboard did not come back after resume");
    None
}

pub async fn start_usb_keyboard_task(
    config: &Config,
    keyboard: DeviceInfo,
    mut event_receiver: broadcast::Receiver<Event>,
    virtual_keyboard: Arc<Mutex<VirtualKeyboard>>,
    state_manager: KeyboardStateManager,
    activity_notifier: ActivityNotifier,
) -> Option<(DeviceId, broadcast::Sender<Shutdown>)> {
    let (shutdown_tx, mut shutdown_rx1) = broadcast::channel::<Shutdown>(1);
    let device_id = keyboard.id();

    // Opening and claiming can fail on a device that is enumerated but not ready yet,
    // which is the normal state for a second or so after resume.
    let keyboard_device = match keyboard.open().await {
        Ok(device) => Arc::new(device),
        Err(e) => {
            warn!("Failed to open USB keyboard: {}", e);
            return None;
        }
    };
    let interface_4 = match keyboard_device.detach_and_claim_interface(4).await {
        Ok(interface) => interface,
        Err(e) => {
            warn!("Failed to claim USB keyboard interface 4: {}", e);
            return None;
        }
    };
    let mut endpoint_5 = match interface_4.endpoint::<Interrupt, In>(0x85) {
        Ok(endpoint) => endpoint,
        Err(e) => {
            warn!("Failed to open USB keyboard endpoint 0x85: {}", e);
            return None;
        }
    };

    state_manager.set_usb_keyboard_attached(true);
    activity_notifier.notify();
    info!("USB connected");

    // Set the fn key mode. Unset in the config means don't touch it, so whatever the BIOS
    // Fn Lock setting put there stays.
    if let Some(fn_lock) = config.fn_lock {
        keyboard_device
            .control_out(
                ControlOut {
                    control_type: ControlType::Class,
                    recipient: Recipient::Interface,
                    request: 0x09,
                    value: 0x035a,
                    index: 4,
                    data: &parse_hex_string(hidraw::fn_lock_report(fn_lock)),
                },
                Duration::from_millis(100),
            )
            .await
            .inspect_err(|e| warn!("Failed to set fn lock: {}", e))
            .ok();
    }

    // A keyboard that docks with its battery charged keeps the backlight it had, so ask
    // before overwriting. Pushing is the fallback for a keyboard that came back from a
    // real power cycle and forgot, and only once we have a level worth restoring.
    match read_backlight_state(&keyboard_device).await {
        Some(level) => state_manager.adopt_keyboard_backlight(level),
        None if state_manager.is_backlight_known() => {
            send_backlight_state(&keyboard_device, state_manager.get_keyboard_backlight()).await;
        }
        None => debug!("Backlight level unknown on attach, leaving the keyboard alone"),
    }

    // Restore mic mute LED state
    let mic_mute_state = state_manager.get_mic_mute_led();
    send_mute_microphone_state(&keyboard_device, mic_mute_state).await;

    // Create a cancellation token for the control task

    // Spawn a task to handle backlight/mic mute events
    let keyboard_device2 = keyboard_device.clone();
    let state_manager_events = state_manager.clone();
    let mut shutdown_rx2 = shutdown_rx1.resubscribe();
    tokio::spawn(async move {
        loop {
            tokio::select! {
                _ = shutdown_rx1.recv() => {
                    info!("USB control task shutting down");
                    break;
                }
                result = event_receiver.recv() => {
                    match result {
                        Ok(Event::Backlight(state)) => {
                            send_backlight_state(&keyboard_device2, state).await;
                        }
                        Ok(Event::MicMuteLed(enabled)) => {
                            send_mute_microphone_state(&keyboard_device2, enabled).await;
                            // Report 0x5a hands back whatever was written to it last, and
                            // that read is how a later attach learns the level. Put the
                            // level back on top; the keyboard is already at it, so nothing
                            // changes visibly.
                            send_backlight_state(
                                &keyboard_device2,
                                state_manager_events.get_keyboard_backlight(),
                            )
                            .await;
                        }
                        Ok(_) => {
                            // dont care about other events
                        }
                        Err(broadcast::error::RecvError::Lagged(_)) => {
                            // Skip lagged messages
                            continue;
                        }
                        Err(broadcast::error::RecvError::Closed) => {
                            break;
                        }
                    }
                }
            }
        }
    });

    let config = config.clone();
    tokio::spawn(async move {
        loop {
            while endpoint_5.pending() < 3 {
                endpoint_5.submit(vec![0u8; 64].into());
            }

            tokio::select! {
                reason = shutdown_rx2.recv() => {
                    info!("USB receive task shutting down");
                    // Re-opening keeps the keyboard attached, so don't flip the
                    // secondary display on and straight back off again.
                    if !matches!(reason, Ok(Shutdown::Reopening)) {
                        state_manager.set_usb_keyboard_attached(false);
                    }
                    virtual_keyboard.lock().await.release_all_keys();
                    break;
                }
                completion = endpoint_5.next_complete() => {
                    match completion.status {
                        Ok(_) => {
                            let data = &completion.buffer[..completion.actual_len];
                            // endpoint 5 is not a HID device so the idle detection module needs to be notified manually
                            activity_notifier.notify();
                            parse_keyboard_data(data, &config, &virtual_keyboard, &state_manager)
                                .await;
                        }
                        Err(e) => {
                            warn!("USB error: {:?}", e);
                            tokio::time::sleep(Duration::from_millis(100)).await;
                            continue;
                        }
                    }
                }
            }
        }
    });

    Some((device_id, shutdown_tx))
}

async fn parse_keyboard_data(
    data: &[u8],
    config: &Config,
    virtual_keyboard: &Arc<Mutex<VirtualKeyboard>>,
    state_manager: &KeyboardStateManager,
) {
    // Only one function key can be pressed at a time, this is a hardware limitation
    match data {
        [90, 0, 0, 0, 0, 0] => {
            debug!("No key pressed");
            virtual_keyboard.lock().await.release_all_keys();
        }
        [90, 199, 0, 0, 0, 0] => {
            debug!("Backlight key pressed");
            config
                .keyboard_backlight_key
                .execute(&virtual_keyboard, &state_manager)
                .await;
        }
        [90, 16, 0, 0, 0, 0] => {
            debug!("Brightness down key pressed");
            config
                .brightness_down_key
                .execute(&virtual_keyboard, &state_manager)
                .await;
        }
        [90, 32, 0, 0, 0, 0] => {
            debug!("Brightness up key pressed");
            config
                .brightness_up_key
                .execute(&virtual_keyboard, &state_manager)
                .await;
        }
        [90, 156, 0, 0, 0, 0] => {
            debug!("Swap up down display key pressed");
            config
                .swap_up_down_display_key
                .execute(&virtual_keyboard, &state_manager)
                .await;
        }
        [90, 124, 0, 0, 0, 0] => {
            debug!("Microphone mute key pressed");
            config
                .microphone_mute_key
                .execute(&virtual_keyboard, &state_manager)
                .await;
        }
        [90, 126, 0, 0, 0, 0] => {
            debug!("Emoji picker key pressed");
            config
                .emoji_picker_key
                .execute(&virtual_keyboard, &state_manager)
                .await;
        }
        [90, 134, 0, 0, 0, 0] => {
            debug!("MyASUS key pressed");
            config
                .myasus_key
                .execute(&virtual_keyboard, &state_manager)
                .await;
        }
        [90, 106, 0, 0, 0, 0] => {
            debug!("Toggle secondary display key pressed");
            config
                .toggle_secondary_display_key
                .execute(&virtual_keyboard, &state_manager)
                .await;
        }
        _ => {
            debug!("Unknown key pressed: {:?}", data);
            virtual_keyboard.lock().await.release_all_keys();
        }
    }
}

/// The wired counterpart of `hidraw::read_backlight_state`: a GET_REPORT for the same
/// vendor report, with the same caveat that it returns whatever was written there last.
async fn read_backlight_state(keyboard: &Arc<Device>) -> Option<KeyboardBacklightState> {
    let reply = keyboard
        .control_in(
            ControlIn {
                control_type: ControlType::Class,
                recipient: Recipient::Interface,
                request: 0x01,
                value: 0x035a,
                index: 4,
                length: 16,
            },
            Duration::from_millis(100),
        )
        .await
        .inspect_err(|e| debug!("Failed to read backlight state: {:?}", e))
        .ok()?;
    hidraw::parse_backlight_report(&reply)
}

async fn send_backlight_state(keyboard: &Arc<Device>, state: KeyboardBacklightState) {
    let data = match state {
        KeyboardBacklightState::Off => parse_hex_string("5abac5c4000000000000000000000000"),
        KeyboardBacklightState::Low => parse_hex_string("5abac5c4010000000000000000000000"),
        KeyboardBacklightState::Medium => parse_hex_string("5abac5c4020000000000000000000000"),
        KeyboardBacklightState::High => parse_hex_string("5abac5c4030000000000000000000000"),
    };

    if let Err(e) = keyboard
        .control_out(
            ControlOut {
                control_type: ControlType::Class,
                recipient: Recipient::Interface,
                request: 0x09,
                value: 0x035a,
                index: 4,
                data: &data,
            },
            Duration::from_millis(100),
        )
        .await
    {
        warn!("Failed to send backlight state: {:?}", e);
    }
}

async fn send_mute_microphone_state(keyboard: &Arc<Device>, state: bool) {
    let data = if state {
        // turn on microphone mute led
        parse_hex_string("5ad07c01000000000000000000000000")
    } else {
        parse_hex_string("5ad07c00000000000000000000000000")
    };

    if let Err(e) = keyboard
        .control_out(
            ControlOut {
                control_type: ControlType::Class,
                recipient: Recipient::Interface,
                request: 0x09,
                value: 0x035a,
                index: 4,
                data: &data,
            },
            Duration::from_millis(100),
        )
        .await
    {
        warn!("Failed to send mic mute state: {:?}", e);
    }
}

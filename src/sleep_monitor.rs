use std::time::Duration;

use futures::StreamExt;
use log::{info, warn};
use tokio::sync::mpsc;
use zbus::zvariant::OwnedFd;
use zbus::{Connection, proxy};

use crate::idle_detection::ActivityNotifier;
use crate::state::KeyboardStateManager;

/// How long to keep the system awake after `PrepareForSleep(true)` so the LED-off
/// writes reach the keyboard over USB. Must stay below logind's InhibitDelayMaxSec
/// (5s by default), otherwise logind suspends anyway and the writes are lost.
const SUSPEND_WRITE_GRACE: Duration = Duration::from_millis(500);

#[proxy(
    interface = "org.freedesktop.login1.Manager",
    default_service = "org.freedesktop.login1",
    default_path = "/org/freedesktop/login1"
)]
trait LogindManager {
    /// Returns a file descriptor. The lock is held for as long as the fd is open.
    fn inhibit(&self, what: &str, who: &str, why: &str, mode: &str) -> zbus::Result<OwnedFd>;

    #[zbus(signal)]
    fn prepare_for_sleep(&self, start: bool) -> zbus::Result<()>;
}

/// Take a delay inhibitor lock. Dropping the returned fd releases it.
async fn take_delay_lock(manager: &LogindManagerProxy<'_>) -> Option<OwnedFd> {
    match manager
        .inhibit(
            "sleep",
            "zenbook-duo-daemon",
            "Turn off keyboard LEDs before sleep",
            "delay",
        )
        .await
    {
        Ok(fd) => Some(fd),
        Err(e) => {
            // Not fatal: PrepareForSleep still fires, we just lose the grace period.
            warn!("Failed to take sleep inhibitor lock: {}", e);
            None
        }
    }
}

/// Watch logind for suspend/resume and drive the keyboard LED state across it.
/// Replaces the pre-sleep and post-sleep systemd units.
pub fn start_sleep_monitor_task(
    state_manager: KeyboardStateManager,
    activity_notifier: ActivityNotifier,
    reconnect_tx: mpsc::Sender<()>,
) {
    tokio::spawn(async move {
        let connection = match Connection::system().await {
            Ok(connection) => connection,
            Err(e) => {
                warn!("No system bus ({}), suspend handling disabled", e);
                return;
            }
        };

        let manager = match LogindManagerProxy::new(&connection).await {
            Ok(manager) => manager,
            Err(e) => {
                warn!("No logind ({}), suspend handling disabled", e);
                return;
            }
        };

        let mut signals = match manager.receive_prepare_for_sleep().await {
            Ok(signals) => signals,
            Err(e) => {
                warn!(
                    "Failed to subscribe to PrepareForSleep ({}), suspend handling disabled",
                    e
                );
                return;
            }
        };

        // Must be held before the first suspend, not taken when the signal arrives.
        let mut lock = take_delay_lock(&manager).await;
        info!("Sleep monitor started");

        while let Some(signal) = signals.next().await {
            let start = match signal.args() {
                Ok(args) => args.start,
                Err(e) => {
                    warn!("Malformed PrepareForSleep signal: {}", e);
                    continue;
                }
            };

            if start {
                info!("Suspending");
                state_manager.suspend_start();
                tokio::time::sleep(SUSPEND_WRITE_GRACE).await;
                lock.take(); // drop the fd -> release the delay -> the system suspends
            } else {
                info!("Resumed");
                state_manager.suspend_end();
                activity_notifier.notify();
                // The kernel dropped our usbfs interface claim while we were out.
                reconnect_tx.send(()).await.ok();
                lock = take_delay_lock(&manager).await; // the fd is one-shot, re-arm it
            }
        }

        warn!("PrepareForSleep stream ended, suspend handling stopped");
    });
}

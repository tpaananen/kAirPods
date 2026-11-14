//! `AirPods` D-Bus Service for KDE Plasma
//!
//! This service provides a D-Bus interface for managing `AirPods` devices
//! in KDE Plasma, including battery monitoring, noise control, and
//! feature management.

use std::{sync::Arc, time::Duration};

use crossbeam::queue::SegQueue;
use log::{debug, info, warn};
use tokio::{
   select,
   signal::{self, unix::SignalKind},
   sync::Notify,
   time,
};
use zbus::{Connection, connection, object_server::InterfaceRef};

use bluetooth::manager::BluetoothManager;
use dbus::AirPodsService;
use event::{AirPodsEvent, EventBus};
use media_control::MediaControl;

mod airpods;
mod battery_provider;
mod battery_study;
mod bluetooth;
mod config;
mod dbus;
mod error;
mod event;
mod media_control;
mod ringbuf;

use battery_provider::BluezBatteryManager;

use crate::{airpods::device::AirPods, dbus::AirPodsServiceSignals, error::Result};

/// Maximum time to wait for devices to disconnect and data to be saved on shutdown
const SHUTDOWN_TIMEOUT: Duration = Duration::from_secs(2);

#[tokio::main]
async fn main() -> Result<()> {
   // Parse command line arguments
   let args: Vec<String> = std::env::args().collect();
   if args.len() > 1 {
      match args[1].as_str() {
         "--version" | "-v" => {
            println!("kairpodsd {}", env!("CARGO_PKG_VERSION"));
            return Ok(());
         },
         "--help" | "-h" => {
            println!("Usage: {} [OPTIONS]", args[0]);
            println!();
            println!("Options:");
            println!("  -v, --version    Print version information and exit");
            println!("  -h, --help       Print this help message and exit");
            return Ok(());
         },
         arg => {
            eprintln!("Unknown argument: {arg}");
            eprintln!("Try '{} --help' for more information.", args[0]);
            std::process::exit(1);
         },
      }
   }

   let (config, config_err) = match config::Config::load() {
      Ok(config) => (config, None),
      Err(e) => (config::Config::default(), Some(e)),
   };

   let default_filter = config.log_filter.as_deref().unwrap_or("info");
   env_logger::Builder::from_env(env_logger::Env::default().default_filter_or(default_filter))
      .init();
   info!("Starting kAirPods D-Bus service...");

   if let Some(err) = config_err {
      warn!("Failed to load configuration: {err:?}");
   } else {
      info!(
         "Loaded configuration with {} known devices",
         config.known_devices.len()
      );
   }

   // Create event channel
   let event_bus = EventProcessor::new();

   // Initialize battery study database
   let battery_study = match battery_study::BatteryStudy::open() {
      Ok(study) => {
         info!("Battery study database initialized");
         Some(study)
      },
      Err(e) => {
         warn!("Failed to initialize battery study database: {e}");
         None
      },
   };

   let export_bluez_battery = config.export_bluez_battery;

   // Create Bluetooth manager with event sender and config
   let bluetooth_manager = BluetoothManager::new(event_bus.clone(), config, battery_study).await?;

   // Create D-Bus service
   let service = AirPodsService::new(bluetooth_manager.clone());

   // Build D-Bus connection
   let connection = connection::Builder::session()?
      .name("org.kairpods")?
      .serve_at("/org/kairpods/manager", service)?
      .build()
      .await?;

   info!("kAirPods D-Bus service started at org.kairpods");

   // Export battery levels to BlueZ (and thus UPower) if enabled in configuration
   let bluez_battery = if export_bluez_battery {
      BluezBatteryManager::new()
         .await
         .inspect_err(|e| warn!("BlueZ battery export unavailable: {e}"))
         .ok()
   } else {
      info!("BlueZ battery export disabled by configuration");
      None
   };

   // Start event processor
   event_bus
      .spawn_dispatcher(connection, bluez_battery)
      .await?;

   // Wait for shutdown signal
   shutdown_signal().await?;
   info!("Shutting down kAirPods service...");

   // Disconnect devices and save battery study data before exiting
   if time::timeout(SHUTDOWN_TIMEOUT, bluetooth_manager.shutdown())
      .await
      .is_err()
   {
      warn!("Bluetooth manager did not shut down within {SHUTDOWN_TIMEOUT:?}");
   }

   Ok(())
}

/// Waits for SIGINT (Ctrl-C) or SIGTERM (systemd stop, session logout).
async fn shutdown_signal() -> Result<()> {
   let mut sigterm = signal::unix::signal(SignalKind::terminate())?;
   select! {
      result = signal::ctrl_c() => result?,
      _ = sigterm.recv() => {},
   }
   Ok(())
}

struct EventProcessor {
   queue: SegQueue<(AirPods, AirPodsEvent)>,
   notifier: Notify,
}

impl EventProcessor {
   fn new() -> Arc<Self> {
      Arc::new(Self {
         queue: SegQueue::new(),
         notifier: Notify::new(),
      })
   }
}

impl EventProcessor {
   async fn recv(self: &Arc<Self>) -> Option<(AirPods, AirPodsEvent)> {
      loop {
         if let Some(event) = self.queue.pop() {
            return Some(event);
         }
         let notify = self.notifier.notified();
         if let Some(event) = self.queue.pop() {
            return Some(event);
         }
         if Arc::strong_count(self) == 1 {
            return None;
         }
         let _ = time::timeout(Duration::from_secs(1), notify).await;
      }
   }

   async fn dispatch(
      &self,
      iface: &InterfaceRef<AirPodsService>,
      (device, event): (AirPods, AirPodsEvent),
      bluez_battery: Option<&BluezBatteryManager>,
      media: &MediaControl,
   ) -> Result<()> {
      let addr_str = device.address_str();
      match event {
         AirPodsEvent::DeviceConnected => {
            iface.device_connected(addr_str).await?;
            if let Some(mgr) = bluez_battery
               && let Some(level) = device.battery_info().and_then(|b| b.aggregate_percentage())
            {
               mgr.update_device_battery(addr_str, level).await;
            }
            // Emit property changes
            iface
               .get()
               .await
               .devices_changed(iface.signal_emitter())
               .await?;
            iface
               .get()
               .await
               .connected_count_changed(iface.signal_emitter())
               .await?;
         },
         AirPodsEvent::DeviceDisconnected => {
            if let Some(mgr) = bluez_battery {
               mgr.remove_device(addr_str).await;
            }
            iface.device_disconnected(addr_str).await?;
            // Emit property changes
            iface
               .get()
               .await
               .devices_changed(iface.signal_emitter())
               .await?;
            iface
               .get()
               .await
               .connected_count_changed(iface.signal_emitter())
               .await?;
         },
         AirPodsEvent::BatteryUpdated(battery) => {
            if let Some(mgr) = bluez_battery
               && let Some(level) = battery.aggregate_percentage()
            {
               mgr.update_device_battery(addr_str, level).await;
            }
            iface
               .battery_updated(addr_str, &battery.to_json().to_string())
               .await?;
            // Emit property change for devices (battery state changed)
            iface
               .get()
               .await
               .devices_changed(iface.signal_emitter())
               .await?;
         },
         AirPodsEvent::NoiseControlChanged(mode) => {
            iface.noise_control_changed(addr_str, mode.to_str()).await?;
            // Emit property change for devices (noise control state changed)
            iface
               .get()
               .await
               .devices_changed(iface.signal_emitter())
               .await?;
         },
         AirPodsEvent::EarDetectionChanged {
            status: ear_detection,
            first_report,
         } => {
            iface
               .ear_detection_changed(addr_str, &ear_detection.to_json().to_string())
               .await?;
            // Emit property change for devices (ear detection state changed)
            iface
               .get()
               .await
               .devices_changed(iface.signal_emitter())
               .await?;

            // Handle play/pause based on ear detection
            // Pause when at least one earbud is removed, play only when both are in.
            // Connecting alone must not touch playback, so the first report is skipped.
            let both_in_ear = ear_detection.is_left_in_ear() && ear_detection.is_right_in_ear();
            if first_report {
               debug!("Initial ear detection state for {addr_str}, leaving playback alone");
            } else if both_in_ear {
               // Both AirPods are in ear - send play command
               media.play();
            } else {
               // At least one AirPod is out of ear - send pause command
               media.pause();
            }
         },
         AirPodsEvent::DeviceNameChanged(name) => {
            iface.device_name_changed(addr_str, &name).await?;
            // Emit property change for devices (name changed)
            iface
               .get()
               .await
               .devices_changed(iface.signal_emitter())
               .await?;
         },
         AirPodsEvent::DeviceError => {
            iface.device_error(addr_str).await?;
            // Emit property change for devices (error state might affect device info)
            iface
               .get()
               .await
               .devices_changed(iface.signal_emitter())
               .await?;
         },
      }
      Ok(())
   }

   async fn spawn_dispatcher(
      self: Arc<Self>,
      connection: Connection,
      bluez_battery: Option<Arc<BluezBatteryManager>>,
   ) -> Result<()> {
      let iface = connection
         .object_server()
         .interface::<_, AirPodsService>("/org/kairpods/manager")
         .await?;
      // Media players are driven from their own task, so they cannot delay the signals
      let media = MediaControl::spawn(connection);
      tokio::spawn(async move {
         while let Some(event) = self.recv().await {
            if let Err(e) = self
               .dispatch(&iface, event, bluez_battery.as_deref(), &media)
               .await
            {
               warn!("Error dispatching event: {e}");
            }
         }
      });

      Ok(())
   }
}

impl EventBus for EventProcessor {
   fn emit(&self, device: &AirPods, event: AirPodsEvent) {
      self.queue.push((device.clone(), event));
      self.notifier.notify_waiters();
   }
}

#[cfg(test)]
mod tests {
   use std::{
      pin::pin,
      process::{self, Command},
   };

   use super::*;

   #[tokio::test]
   async fn sigterm_ends_shutdown_wait() {
      let mut wait = pin!(shutdown_signal());
      // The first poll installs the signal handlers; without them SIGTERM would kill the test
      assert!(futures::poll!(wait.as_mut()).is_pending());

      let status = Command::new("kill")
         .args(["-TERM", &process::id().to_string()])
         .status()
         .unwrap();
      assert!(status.success());

      time::timeout(Duration::from_secs(5), wait)
         .await
         .expect("SIGTERM should end the wait")
         .unwrap();
   }
}

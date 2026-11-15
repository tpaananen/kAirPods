//! `AirPods` D-Bus Service for KDE Plasma
//!
//! This service provides a D-Bus interface for managing `AirPods` devices
//! in KDE Plasma, including battery monitoring, noise control, and
//! feature management.

use std::{sync::Arc, time::Duration};

use log::{debug, info, warn};
use tokio::{
   select,
   signal::{self, unix::SignalKind},
   sync::mpsc,
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
   let (event_bus, events) = EventProcessor::new();

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
   spawn_dispatcher(events, connection, bluez_battery).await?;

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

/// A device and one of its events, on their way to the dispatcher.
type DeviceEvent = (AirPods, AirPodsEvent);

/// Event bus of the service: queues events for the dispatcher.
struct EventProcessor {
   tx: mpsc::UnboundedSender<DeviceEvent>,
}

impl EventProcessor {
   /// Creates the bus and the receiver its events go to.
   ///
   /// The receiver ends as soon as every handle to the bus is dropped.
   fn new() -> (Arc<Self>, mpsc::UnboundedReceiver<DeviceEvent>) {
      let (tx, rx) = mpsc::unbounded_channel();
      (Arc::new(Self { tx }), rx)
   }
}

impl EventBus for EventProcessor {
   fn emit(&self, device: &AirPods, event: AirPodsEvent) {
      // Fails only once the dispatcher is gone, at shutdown
      let _ = self.tx.send((device.clone(), event));
   }
}

/// Properties of the manager interface that a batch of events changed.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
struct PropertyChanges {
   devices: bool,
   connected_count: bool,
}

impl PropertyChanges {
   /// Every event changes the device JSON; only connections change the count.
   const fn of(event: &AirPodsEvent) -> Self {
      Self {
         devices: true,
         connected_count: matches!(
            event,
            AirPodsEvent::DeviceConnected | AirPodsEvent::DeviceDisconnected
         ),
      }
   }

   const fn merge(self, other: Self) -> Self {
      Self {
         devices: self.devices || other.devices,
         connected_count: self.connected_count || other.connected_count,
      }
   }

   /// What the events change together.
   fn of_batch<'a>(events: impl IntoIterator<Item = &'a DeviceEvent>) -> Self {
      events
         .into_iter()
         .fold(Self::default(), |changes, (_, event)| {
            changes.merge(Self::of(event))
         })
   }
}

/// `first` and the events queued behind it.
///
/// A connection brings several events at once; handled together, they cost one update
/// of each property instead of one per event.
fn take_queued(
   first: DeviceEvent,
   events: &mut mpsc::UnboundedReceiver<DeviceEvent>,
) -> Vec<DeviceEvent> {
   let mut batch = vec![first];
   while let Ok(event) = events.try_recv() {
      batch.push(event);
   }
   batch
}

/// Sends the property changes of a batch of events.
///
/// Each one makes clients read the property, and the device JSON asks the manager for
/// every device and serializes it.
async fn emit_property_changes(
   iface: &InterfaceRef<AirPodsService>,
   changes: PropertyChanges,
) -> Result<()> {
   let service = iface.get().await;
   if changes.devices {
      service.devices_changed(iface.signal_emitter()).await?;
   }
   if changes.connected_count {
      service
         .connected_count_changed(iface.signal_emitter())
         .await?;
   }
   Ok(())
}

/// Sends the signals of one event, and acts on it. The property changes are sent per
/// batch, by [`emit_property_changes`].
async fn dispatch(
   iface: &InterfaceRef<AirPodsService>,
   (device, event): DeviceEvent,
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
      },
      AirPodsEvent::DeviceDisconnected => {
         if let Some(mgr) = bluez_battery {
            mgr.remove_device(addr_str).await;
         }
         iface.device_disconnected(addr_str).await?;
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
      },
      AirPodsEvent::NoiseControlChanged(mode) => {
         iface.noise_control_changed(addr_str, mode.to_str()).await?;
      },
      AirPodsEvent::EarDetectionChanged {
         status: ear_detection,
         previous,
      } => {
         iface
            .ear_detection_changed(addr_str, &ear_detection.to_json().to_string())
            .await?;

         // Pause when a bud leaves an ear, resume when one goes back in
         if previous.is_none() {
            debug!("Initial ear detection state for {addr_str}, leaving playback alone");
         } else if let Some(action) = media_control::playback_action(previous, ear_detection) {
            media.apply(action);
         }
      },
      AirPodsEvent::DeviceError => {
         iface.device_error(addr_str).await?;
      },
   }
   Ok(())
}

async fn spawn_dispatcher(
   mut events: mpsc::UnboundedReceiver<DeviceEvent>,
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
      while let Some(first) = events.recv().await {
         let batch = take_queued(first, &mut events);
         let changes = PropertyChanges::of_batch(&batch);
         for event in batch {
            if let Err(e) = dispatch(&iface, event, bluez_battery.as_deref(), &media).await {
               warn!("Error dispatching event: {e}");
            }
         }
         if let Err(e) = emit_property_changes(&iface, changes).await {
            warn!("Error emitting property changes: {e}");
         }
      }
   });

   Ok(())
}

#[cfg(test)]
mod tests {
   use std::{
      pin::pin,
      process::{self, Command},
   };

   use super::*;

   #[tokio::test]
   async fn dispatcher_stops_as_soon_as_the_event_bus_is_dropped() {
      let (processor, mut events) = EventProcessor::new();
      let device = AirPods::new(Default::default(), "test".into(), None, None);

      // An event reaches the dispatcher as it is emitted
      processor.emit(&device, AirPodsEvent::DeviceConnected);
      let received = time::timeout(Duration::from_millis(200), events.recv()).await;
      assert!(matches!(
         received,
         Ok(Some((_, AirPodsEvent::DeviceConnected)))
      ));

      let waiting = tokio::spawn(async move { events.recv().await.is_none() });
      tokio::task::yield_now().await;

      drop(processor);
      let stopped = time::timeout(Duration::from_millis(200), waiting).await;
      assert!(matches!(stopped, Ok(Ok(true))), "dispatcher still waiting");
   }

   #[test]
   fn queued_events_are_taken_together() {
      let (processor, mut events) = EventProcessor::new();
      let device = AirPods::new(Default::default(), "test".into(), None, None);
      processor.emit(&device, AirPodsEvent::DeviceConnected);
      processor.emit(
         &device,
         AirPodsEvent::NoiseControlChanged(Default::default()),
      );
      processor.emit(&device, AirPodsEvent::DeviceError);

      let first = events.try_recv().unwrap();
      let batch = take_queued(first, &mut events);
      assert!(matches!(
         batch.as_slice(),
         [
            (_, AirPodsEvent::DeviceConnected),
            (_, AirPodsEvent::NoiseControlChanged(_)),
            (_, AirPodsEvent::DeviceError),
         ]
      ));
      assert!(events.try_recv().is_err());
   }

   #[test]
   fn a_batch_changes_each_property_once_and_the_count_only_on_connections() {
      let device = AirPods::new(Default::default(), "test".into(), None, None);
      let batch = |events: Vec<AirPodsEvent>| -> PropertyChanges {
         let events: Vec<_> = events.into_iter().map(|e| (device.clone(), e)).collect();
         PropertyChanges::of_batch(&events)
      };
      let devices_only = PropertyChanges {
         devices: true,
         connected_count: false,
      };
      let both = PropertyChanges {
         devices: true,
         connected_count: true,
      };

      assert_eq!(
         batch(vec![
            AirPodsEvent::NoiseControlChanged(Default::default()),
            AirPodsEvent::DeviceError,
         ]),
         devices_only
      );
      assert_eq!(
         batch(vec![
            AirPodsEvent::DeviceError,
            AirPodsEvent::DeviceDisconnected,
         ]),
         both
      );
      assert_eq!(batch(vec![AirPodsEvent::DeviceConnected]), both);
      assert_eq!(batch(vec![]), PropertyChanges::default());
   }

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

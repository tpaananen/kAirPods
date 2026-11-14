//! Bluetooth device manager for `AirPods`.
//!
//! This module handles Bluetooth adapter management, device discovery,
//! and connection lifecycle for `AirPods` devices.

use std::{collections::HashMap, time::Duration};

use bluer::{
   Adapter, AdapterEvent, AdapterProperty, Address, DeviceEvent, DeviceProperty, Session,
};
use futures::stream::StreamExt;
use log::{debug, error, info, warn};
use smol_str::SmolStr;
use tokio::{
   select,
   sync::{mpsc, oneshot},
   task::JoinHandle,
   time::{self, MissedTickBehavior},
};

use crate::{
   airpods::{self, device::AirPods},
   battery_study::BatteryStudy,
   config::Config,
   error::{AirPodsError, Result},
   event::{AirPodsEvent, EventSender},
};
use rand::Rng;

/// Interval of the scan that catches connection changes the device events missed
const HEALTH_CHECK_INTERVAL: Duration = Duration::from_secs(60);
/// Interval to check for new adapters
const ADAPTER_CHECK_INTERVAL: Duration = Duration::from_secs(10);
/// Delay before the first adapter recovery attempt; doubles per failure
const ADAPTER_RECOVERY_DELAY: Duration = Duration::from_secs(5);
/// Maximum delay between adapter recovery attempts
const MAX_ADAPTER_RECOVERY_DELAY: Duration = Duration::from_secs(120);
/// Maximum time to wait for AAP connection
const AAP_CONNECTION_TIMEOUT: Duration = Duration::from_secs(30);
/// Base of the AAP connection retry delay; doubles per failure
const AAP_RETRY_DELAY: Duration = Duration::from_secs(2);
/// Maximum AAP connection retry delay
const MAX_AAP_RETRY_DELAY: Duration = Duration::from_secs(120);
/// Device tick interval
const DEVICE_TICK_INTERVAL: Duration = Duration::from_secs(10);
/// Channel buffer size
const CHANNEL_BUFFER_SIZE: usize = 1000;

// === Adapter Management ===

/// What the manager knows about an adapter, and whether devices on it can use AAP.
#[derive(Debug, Clone, PartialEq)]
enum AdapterState {
   /// Powered and set up: its devices are monitored and AAP connections may be made
   Active,
   /// Turned off by the user or the system; it is left off until powered on
   PoweredOff,
   /// Its event stream ended, typically because `BlueZ` removed it; set up again after a
   /// backoff, or as soon as it reports a power-on
   Lost,
   /// Setting it up failed (reading its power state or watching its events); retried
   /// after a backoff. Holds the error for the log.
   Failed(String),
}

struct AdapterInfo {
   adapter: Adapter,
   state: AdapterState,
   monitor_handle: Option<JoinHandle<()>>,
   retry_count: u32,
   next_retry: Option<time::Instant>,
   name: SmolStr,
}

// === Device Management ===

/// Watches one `BlueZ` device for Bluetooth connection changes.
struct DeviceMonitor {
   adapter_name: SmolStr,
   handle: JoinHandle<()>,
}

impl Drop for DeviceMonitor {
   fn drop(&mut self) {
      self.handle.abort();
   }
}

#[derive(Debug, Copy, Clone, PartialEq)]
enum BluetoothState {
   Connected,
   Disconnected,
}

#[derive(Debug, Copy, Clone, PartialEq)]
enum AAPState {
   Disconnected,
   Connecting,
   Connected,
   Failed(&'static str),
   WaitingToReconnect,
}

struct ManagedDevice {
   device: AirPods,
   bluetooth_state: BluetoothState,
   aap_state: AAPState,
   adapter_name: SmolStr,
   aap_retry_count: u32,
   last_aap_error: Option<String>,
   aap_handle: Option<JoinHandle<()>>,
   /// Identifies the connection attempt whose task may report back; 0 when there is none
   aap_attempt: u64,
   /// Pending delayed reconnect after a failed connection
   retry_handle: Option<JoinHandle<()>>,
   /// The user disconnected AAP: do not reconnect on our own until Bluetooth reconnects
   user_disconnected: bool,
}

impl ManagedDevice {
   /// Stops the connection task and any pending reconnect.
   ///
   /// Messages they already queued are ignored from here on, because the attempt they
   /// carry is no longer current.
   fn stop_aap(&mut self) {
      if let Some(handle) = self.aap_handle.take() {
         handle.abort();
      }
      if let Some(handle) = self.retry_handle.take() {
         handle.abort();
      }
      self.aap_attempt = 0;
   }

   /// Stops connecting and closes the AAP connection.
   ///
   /// Reports the disconnect only if a connection was open, so a connection whose packet
   /// processor already reported its end is not reported twice.
   async fn close_aap(&mut self, event_tx: &EventSender) {
      self.stop_aap();
      if self.device.disconnect().await {
         event_tx.emit(&self.device, AirPodsEvent::DeviceDisconnected);
      }
   }

   /// Records that the Bluetooth link is down, and closes AAP with it.
   ///
   /// AAP cannot outlive the link: close it now instead of leaving the device reported
   /// as connected until the socket notices. Pending reconnects are cancelled, because an
   /// L2CAP connect without a link would page the device and pull it onto this computer.
   async fn mark_bluetooth_disconnected(&mut self, event_tx: &EventSender) {
      self.bluetooth_state = BluetoothState::Disconnected;
      self.aap_state = AAPState::Disconnected;
      self.close_aap(event_tx).await;
   }

   /// Whether the manager should establish the AAP connection on its own.
   fn wants_aap(&self) -> bool {
      self.bluetooth_state == BluetoothState::Connected
         && !self.user_disconnected
         && matches!(self.aap_state, AAPState::Disconnected | AAPState::Failed(_))
   }
}

// === Commands ===

#[derive(Debug)]
enum ManagerCommand {
   // Adapter events
   AdapterPowered(SmolStr),
   AdapterPoweredOff(SmolStr),
   AdapterLost(SmolStr),
   AdapterError(SmolStr, String), // adapter_name, error

   // Device events
   DeviceAdded(Address, SmolStr),        // address, adapter_name
   DeviceDiscovered(Address, SmolStr),   // address, adapter_name
   BluetoothConnected(Address, SmolStr), // address, adapter_name
   BluetoothDisconnected(Address),
   AAPConnected(Address, u64),          // address, attempt
   AAPDisconnected(Address, u64, bool), // address, attempt, is_error
   RetryAAP(Address, u64),              // address, attempt
   DeviceLost(Address, SmolStr),        // address, adapter_name

   // User commands
   EstablishAAP(Address, oneshot::Sender<Result<()>>),
   DisconnectAAP(Address, Option<oneshot::Sender<Result<()>>>),
   GetDeviceState(Address, oneshot::Sender<Option<AirPods>>),
   GetAllDeviceStates(oneshot::Sender<Vec<AirPods>>),
   CountConnectedDevices(oneshot::Sender<u32>),
   Shutdown(oneshot::Sender<()>),
}

// === Main Manager ===

/// Main Bluetooth manager that handles device discovery and connections.
///
/// This type provides a high-level interface for managing `AirPods` devices
/// across all available Bluetooth adapters. Clones share the same manager.
#[derive(Clone)]
pub struct BluetoothManager {
   inbox: mpsc::Sender<ManagerCommand>,
}

impl BluetoothManager {
   pub async fn new(
      event_tx: EventSender,
      config: Config,
      battery_study: Option<BatteryStudy>,
   ) -> Result<Self> {
      let (command_tx, command_rx) = mpsc::channel(CHANNEL_BUFFER_SIZE);
      let actor = ManagerActor::new(config, event_tx, command_rx, battery_study).await?;
      tokio::spawn(actor.run());
      Ok(Self { inbox: command_tx })
   }

   /// Disconnects all devices and stops the manager.
   ///
   /// Returns once cleanup has finished, or immediately if the manager is already gone.
   pub async fn shutdown(&self) {
      let (tx, rx) = oneshot::channel();
      if self.inbox.send(ManagerCommand::Shutdown(tx)).await.is_ok() {
         let _ = rx.await;
      }
   }

   pub async fn establish_aap(&self, address: Address) -> Result<()> {
      let (tx, rx) = oneshot::channel();
      self
         .inbox
         .send(ManagerCommand::EstablishAAP(address, tx))
         .await
         .map_err(|_| AirPodsError::ManagerShutdown)?;
      rx.await.map_err(|_| AirPodsError::ManagerShutdown)?
   }

   pub async fn disconnect_aap(&self, address: Address) -> Result<()> {
      let (tx, rx) = oneshot::channel();
      self
         .inbox
         .send(ManagerCommand::DisconnectAAP(address, Some(tx)))
         .await
         .map_err(|_| AirPodsError::ManagerShutdown)?;
      rx.await.map_err(|_| AirPodsError::ManagerShutdown)?
   }

   pub async fn get_device(&self, address: Address) -> Result<AirPods> {
      let (tx, rx) = oneshot::channel();
      self
         .inbox
         .send(ManagerCommand::GetDeviceState(address, tx))
         .await
         .map_err(|_| AirPodsError::DeviceNotFound(address))?;

      rx.await
         .ok()
         .flatten()
         .ok_or(AirPodsError::DeviceNotFound(address))
   }

   pub async fn all_devices(&self) -> Vec<AirPods> {
      let (tx, rx) = oneshot::channel();
      if self
         .inbox
         .send(ManagerCommand::GetAllDeviceStates(tx))
         .await
         .is_err()
      {
         return Vec::new();
      }
      rx.await.unwrap_or_default()
   }

   /// Number of devices with an established AAP connection.
   pub async fn count_connected_devices(&self) -> u32 {
      let (tx, rx) = oneshot::channel();
      if self
         .inbox
         .send(ManagerCommand::CountConnectedDevices(tx))
         .await
         .is_err()
      {
         return 0;
      }
      rx.await.unwrap_or_default()
   }
}

// === Manager Actor ===

struct ManagerActor {
   config: Config,
   event_tx: EventSender,
   command_rx: mpsc::Receiver<ManagerCommand>,
   loopback_rx: mpsc::Receiver<ManagerCommand>,
   loopback_tx: mpsc::Sender<ManagerCommand>,
   session: Session,
   battery_study: Option<BatteryStudy>,
   /// Answered once cleanup has finished after a shutdown request
   shutdown_reply: Option<oneshot::Sender<()>>,

   // State
   adapters: HashMap<SmolStr, AdapterInfo>,
   devices: HashMap<Address, ManagedDevice>,
   /// Tasks reporting Bluetooth connection changes, for every device of an active adapter
   device_monitors: HashMap<Address, DeviceMonitor>,
   /// Source of unique AAP connection attempt ids
   next_attempt: u64,
}

impl ManagerActor {
   async fn new(
      config: Config,
      event_tx: EventSender,
      command_rx: mpsc::Receiver<ManagerCommand>,
      battery_study: Option<BatteryStudy>,
   ) -> Result<Self> {
      let session = Session::new().await?;

      let (loopback_tx, loopback_rx) = mpsc::channel(CHANNEL_BUFFER_SIZE);
      Ok(Self {
         config,
         event_tx,
         command_rx,
         loopback_rx,
         loopback_tx,
         session,
         battery_study,
         shutdown_reply: None,
         adapters: HashMap::new(),
         devices: HashMap::new(),
         device_monitors: HashMap::new(),
         next_attempt: 0,
      })
   }

   async fn run(mut self) {
      info!("Bluetooth manager starting up");

      // Initialize adapters
      self.initialize_adapters().await;

      // Start periodic checks
      // The first scan waits a full interval: at startup the device monitors report
      // what is connected, and a scan racing them would log their work as missed
      let mut health_check_interval = time::interval_at(
         time::Instant::now() + HEALTH_CHECK_INTERVAL,
         HEALTH_CHECK_INTERVAL,
      );
      health_check_interval.set_missed_tick_behavior(MissedTickBehavior::Skip);

      let mut adapter_check_interval = time::interval(ADAPTER_CHECK_INTERVAL);
      adapter_check_interval.set_missed_tick_behavior(MissedTickBehavior::Skip);

      let mut device_tick_interval = time::interval(DEVICE_TICK_INTERVAL);
      device_tick_interval.set_missed_tick_behavior(MissedTickBehavior::Skip);

      // Main event loop
      loop {
         let next_retry = self
            .adapters
            .values()
            .filter_map(|info| info.next_retry)
            .min();
         select! {
             _ = async {
                 match next_retry {
                     Some(deadline) => time::sleep_until(deadline).await,
                     None => std::future::pending().await,
                 }
             } => {
                 let now = time::Instant::now();
                 let due: Vec<_> = self.adapters.iter()
                     .filter(|(_, info)| info.next_retry.is_some_and(|deadline| deadline <= now))
                     .map(|(name, _)| name.clone())
                     .collect();
                 for name in due {
                     self.initialize_adapter(name).await;
                 }
             }
             _ = health_check_interval.tick() => {
                 // Check connection health and scan for new devices
                 self.check_connection_health().await;
                 self.scan_for_connected_airpods().await;
             }
             _ = adapter_check_interval.tick() => {
                 // Check for new adapters
                 self.discover_new_adapters().await;
             }
             _ = device_tick_interval.tick() => {
                 // Tick all devices
                 self.tick_all_devices();
             }
             cmd = self.command_rx.recv() => {
                 let Some(cmd) = cmd else {
                     info!("Bluetooth manager shutting down");
                     break;
                 };
                 if !self.handle_command(cmd).await {
                     break;
                 }
             }
             Some(cmd) = self.loopback_rx.recv() => {
                 if !self.handle_command(cmd).await {
                     break;
                 }
             }
         }
      }

      // Cleanup
      self.cleanup().await;
      if let Some(reply) = self.shutdown_reply.take() {
         let _ = reply.send(());
      }
   }

   async fn initialize_adapters(&mut self) {
      match self.session.adapter_names().await {
         Ok(names) => {
            for name in names {
               self.initialize_adapter(name.into()).await;
            }
         },
         Err(e) => {
            error!("Failed to get adapter names: {e}");
         },
      }

      // If no adapters found, try default
      if self.adapters.is_empty() {
         self.initialize_adapter(SmolStr::new_static("hci0")).await;
      }
   }

   async fn initialize_adapter(&mut self, name: SmolStr) {
      if !self.adapters.contains_key(&name) {
         let adapter = match self.session.adapter(&name) {
            Ok(adapter) => adapter,
            Err(e) => {
               warn!("Failed to initialize adapter {name}: {e}");
               return;
            },
         };
         info!("Initializing adapter: {name}");
         self.adapters.insert(
            name.clone(),
            AdapterInfo {
               state: AdapterState::Lost,
               monitor_handle: None,
               adapter,
               retry_count: 0,
               next_retry: None,
               name: name.clone(),
            },
         );
      }

      let info = self.adapters.get_mut(&name).expect("adapter inserted");
      info.next_retry = None;
      if info.monitor_handle.is_none() {
         info.monitor_handle = Some(Self::start_adapter_monitor(
            self.loopback_tx.clone(),
            name.clone(),
            info.adapter.clone(),
         ));
      }
      // Bluetooth turned off is a choice of the user or the system: wait for it to be
      // powered on instead of overriding it. The adapter monitor reports the power-on.
      match info.adapter.is_powered().await {
         Ok(true) => {},
         Ok(false) => {
            Self::mark_powered_off(info);
            return;
         },
         Err(e) => {
            self.schedule_adapter_retry(
               &name,
               format!("Failed to read power of adapter {name}: {e}"),
            );
            return;
         },
      }

      self.handle_adapter_available(name).await;
   }

   fn mark_powered_off(info: &mut AdapterInfo) {
      if info.state != AdapterState::PoweredOff {
         info!(
            "Adapter {} is powered off, waiting for it to be powered on",
            info.name
         );
      }
      info.state = AdapterState::PoweredOff;
      info.retry_count = 0;
      info.next_retry = None;
   }

   fn schedule_adapter_retry(&mut self, name: &SmolStr, error: String) {
      if let Some(info) = self.adapters.get_mut(name) {
         if matches!(info.state, AdapterState::Failed(_)) {
            debug!("{error}");
         } else {
            warn!("{error}");
         }
         info.state = AdapterState::Failed(error);
         info.next_retry = Some(time::Instant::now() + adapter_retry_delay(info.retry_count));
         info.retry_count = info.retry_count.saturating_add(1);
      }
   }

   fn start_adapter_monitor(
      loopback: mpsc::Sender<ManagerCommand>,
      name: SmolStr,
      adapter: Adapter,
   ) -> JoinHandle<()> {
      tokio::spawn(async move {
         let Ok(mut events) = adapter.events().await else {
            if let Err(e) = loopback
               .send(ManagerCommand::AdapterError(
                  name.clone(),
                  "Failed to get adapter events".to_string(),
               ))
               .await
            {
               warn!("Channel overflow sending adapter error: {e}");
            }
            return;
         };

         // Report an adapter that is already powered: a power-on between the caller's
         // check and this subscription would otherwise go unnoticed
         if adapter.is_powered().await == Ok(true)
            && loopback
               .send(ManagerCommand::AdapterPowered(name.clone()))
               .await
               .is_err()
         {
            return;
         }

         while let Some(event) = events.next().await {
            match event {
               AdapterEvent::DeviceAdded(addr) => {
                  debug!("Device added on {name}: {addr}");
                  let _ = loopback
                     .send(ManagerCommand::DeviceAdded(addr, name.clone()))
                     .await;
               },
               AdapterEvent::DeviceRemoved(addr) => {
                  debug!("Device removed on {name}: {addr}");
                  let _ = loopback
                     .send(ManagerCommand::DeviceLost(addr, name.clone()))
                     .await;
               },
               AdapterEvent::PropertyChanged(AdapterProperty::Powered(powered)) => {
                  let command = if powered {
                     ManagerCommand::AdapterPowered(name.clone())
                  } else {
                     ManagerCommand::AdapterPoweredOff(name.clone())
                  };
                  let _ = loopback.send(command).await;
               },
               // Connection changes are reported by the device monitors
               _ => {},
            }
         }

         // If we exit the event loop, adapter is probably gone
         if let Err(e) = loopback.send(ManagerCommand::AdapterLost(name)).await {
            warn!("Channel overflow sending adapter lost: {e}");
         }
      })
   }

   /// Starts reporting Bluetooth connection changes of the device at `addr`.
   ///
   /// The monitor first reports the device if it is already connected, so devices
   /// connected before it started are found too.
   fn monitor_device(&mut self, addr: Address, adapter_name: &SmolStr) {
      if self
         .device_monitors
         .get(&addr)
         .is_some_and(|monitor| !monitor.handle.is_finished())
      {
         return;
      }
      let Some(adapter_info) = self.adapters.get(adapter_name) else {
         return;
      };
      let device = match adapter_info.adapter.device(addr) {
         Ok(device) => device,
         Err(e) => {
            debug!("Cannot monitor {addr}: {e}");
            return;
         },
      };

      let loopback = self.loopback_tx.clone();
      let name = adapter_name.clone();
      let handle = tokio::spawn(async move {
         let events = match device.events().await {
            Ok(events) => events,
            Err(e) => {
               warn!("Failed to watch {addr} for connection changes: {e}");
               return;
            },
         };
         futures::pin_mut!(events);

         // Report the current state both ways: a monitor restarted after its stream ended
         // must not miss a change that happened in between
         let initial = match device.is_connected().await {
            Ok(true) => Some(ManagerCommand::BluetoothConnected(addr, name.clone())),
            Ok(false) => Some(ManagerCommand::BluetoothDisconnected(addr)),
            Err(_) => None,
         };
         if let Some(command) = initial
            && loopback.send(command).await.is_err()
         {
            return;
         }

         // Ends when BlueZ removes the device, or any one of its interfaces
         while let Some(DeviceEvent::PropertyChanged(property)) = events.next().await {
            let command = match property {
               DeviceProperty::Connected(true) => {
                  ManagerCommand::BluetoothConnected(addr, name.clone())
               },
               DeviceProperty::Connected(false) => ManagerCommand::BluetoothDisconnected(addr),
               _ => continue,
            };
            if loopback.send(command).await.is_err() {
               return;
            }
         }
      });
      self.device_monitors.insert(
         addr,
         DeviceMonitor {
            adapter_name: adapter_name.clone(),
            handle,
         },
      );
   }

   /// Starts monitoring every device of the adapter.
   async fn monitor_adapter_devices(&mut self, adapter_name: &SmolStr) {
      let Some(adapter_info) = self.adapters.get(adapter_name) else {
         return;
      };
      match adapter_info.adapter.device_addresses().await {
         Ok(addresses) => {
            for addr in addresses {
               self.monitor_device(addr, adapter_name);
            }
         },
         Err(e) => warn!("Failed to list devices of {adapter_name}: {e}"),
      }
   }

   /// Stops monitoring the devices of the adapter.
   fn stop_adapter_device_monitors(&mut self, adapter_name: &SmolStr) {
      self
         .device_monitors
         .retain(|_, monitor| monitor.adapter_name != *adapter_name);
   }

   async fn is_airpods_device(&self, device: &bluer::Device) -> bool {
      // Check known addresses
      let addr = device.address();
      if self.config.is_known_device(addr) {
         return true;
      }
      airpods::recognition::is_device_airpods(device).await
   }

   async fn handle_command(&mut self, cmd: ManagerCommand) -> bool {
      match cmd {
         ManagerCommand::AdapterPowered(name) => {
            self.handle_adapter_powered(name).await;
         },
         ManagerCommand::AdapterPoweredOff(name) => {
            self.handle_adapter_powered_off(&name);
         },
         ManagerCommand::AdapterLost(name) => {
            self.handle_adapter_lost(name).await;
         },
         ManagerCommand::AdapterError(name, error) => {
            self.handle_adapter_error(&name, error);
         },
         ManagerCommand::DeviceAdded(addr, adapter_name) => {
            self.monitor_device(addr, &adapter_name);
         },
         ManagerCommand::DeviceDiscovered(addr, adapter_name) => {
            self.handle_device_discovered(addr, adapter_name).await;
         },
         ManagerCommand::BluetoothConnected(addr, adapter_name) => {
            self.handle_bluetooth_connected(addr, adapter_name).await;
         },
         ManagerCommand::BluetoothDisconnected(addr) => {
            self.handle_bluetooth_disconnected(addr).await;
         },
         ManagerCommand::AAPConnected(addr, attempt) => {
            self.handle_aap_connected(addr, attempt);
         },
         ManagerCommand::AAPDisconnected(addr, attempt, is_error) => {
            self.handle_aap_disconnected(addr, attempt, is_error);
         },
         ManagerCommand::RetryAAP(addr, attempt) => {
            self.handle_aap_retry(addr, attempt).await;
         },
         ManagerCommand::DeviceLost(addr, adapter_name) => {
            self.handle_device_lost(addr, adapter_name).await;
         },
         ManagerCommand::EstablishAAP(addr, reply) => {
            self.handle_establish_aap(addr, reply).await;
         },
         ManagerCommand::DisconnectAAP(addr, reply) => {
            self.handle_disconnect_aap(addr, reply).await;
         },
         ManagerCommand::GetDeviceState(addr, reply) => {
            self.handle_get_device_state(addr, reply);
         },
         ManagerCommand::GetAllDeviceStates(reply) => {
            self.handle_get_all_device_states(reply);
         },
         ManagerCommand::CountConnectedDevices(reply) => {
            self.handle_count_connected_devices(reply);
         },
         ManagerCommand::Shutdown(reply) => {
            self.handle_shutdown(reply);
            return false;
         },
      }
      true
   }

   // === User commands ===

   /// Connects AAP on request. An explicit request lifts an earlier user disconnect.
   async fn handle_establish_aap(&mut self, addr: Address, reply: oneshot::Sender<Result<()>>) {
      if let Some(device) = self.devices.get_mut(&addr) {
         device.user_disconnected = false;
      }
      let result = self.establish_aap_connection(addr).await;
      let _ = reply.send(result);
   }

   /// Disconnects AAP on request, answering if the caller waits for the result.
   async fn handle_disconnect_aap(
      &mut self,
      addr: Address,
      reply: Option<oneshot::Sender<Result<()>>>,
   ) {
      let result = self.disconnect_aap(addr).await;
      if let Some(reply) = reply {
         let _ = reply.send(result);
      }
   }

   fn handle_get_device_state(&self, addr: Address, reply: oneshot::Sender<Option<AirPods>>) {
      let state = self.devices.get(&addr).map(|d| d.device.clone());
      let _ = reply.send(state);
   }

   fn handle_get_all_device_states(&self, reply: oneshot::Sender<Vec<AirPods>>) {
      let states = self.devices.values().map(|d| d.device.clone()).collect();
      let _ = reply.send(states);
   }

   /// Counts devices with an AAP connection: the same notion of "connected" as the
   /// device JSON.
   fn handle_count_connected_devices(&self, reply: oneshot::Sender<u32>) {
      let count = self
         .devices
         .values()
         .filter(|d| d.device.is_connected())
         .count() as u32;
      let _ = reply.send(count);
   }

   /// Records who to answer once cleanup has finished. The caller then ends the loop.
   fn handle_shutdown(&mut self, reply: oneshot::Sender<()>) {
      info!("Bluetooth manager shutting down");
      self.shutdown_reply = Some(reply);
   }

   async fn handle_adapter_available(&mut self, name: SmolStr) {
      if let Some(info) = self.adapters.get_mut(&name) {
         if matches!(info.state, AdapterState::Failed(_)) {
            info!("Adapter recovered: {name}");
         } else {
            info!("Adapter available: {name}");
         }
         info.state = AdapterState::Active;
         info.retry_count = 0;
         info.next_retry = None;

         // Restart monitor if needed
         if info.monitor_handle.is_none() {
            info.monitor_handle = Some(Self::start_adapter_monitor(
               self.loopback_tx.clone(),
               name.clone(),
               info.adapter.clone(),
            ));
         }

         // Watch its devices; connected ones are reported right away
         self.monitor_adapter_devices(&name).await;

         // Try to reconnect failed AAP connections on this adapter
         let devices_to_reconnect: Vec<Address> = self
            .devices
            .iter()
            .filter(|(_, d)| d.adapter_name == name && d.wants_aap())
            .map(|(addr, _)| *addr)
            .collect();

         for addr in devices_to_reconnect {
            let _ = self.establish_aap_connection(addr).await;
         }
      }
   }

   async fn handle_adapter_lost(&mut self, name: SmolStr) {
      warn!("Adapter lost: {name}");
      self.stop_adapter_device_monitors(&name);

      let Some(info) = self.adapters.get_mut(&name) else {
         return;
      };
      info.state = AdapterState::Lost;
      // Abort the monitor handle
      if let Some(handle) = info.monitor_handle.take() {
         handle.abort();
      }
      info.next_retry = Some(time::Instant::now() + adapter_retry_delay(info.retry_count));
      info.retry_count = info.retry_count.saturating_add(1);

      // A lost adapter has no links: close AAP on its devices and mark them failed. When
      // the adapter returns, the device monitors report the devices that are connected.
      for device in self.devices.values_mut() {
         if device.adapter_name == name {
            device.mark_bluetooth_disconnected(&self.event_tx).await;
            device.aap_state = AAPState::Failed("Adapter lost");
            self
               .event_tx
               .emit(&device.device, AirPodsEvent::DeviceError);
         }
      }
   }

   /// Handles an adapter that reported it is powered.
   ///
   /// Sets it up unless it is already active: the adapter monitor also reports an adapter
   /// that was powered when it subscribed, which is usually one already set up.
   async fn handle_adapter_powered(&mut self, name: SmolStr) {
      if self
         .adapters
         .get(&name)
         .is_some_and(|info| info.state != AdapterState::Active)
      {
         self.initialize_adapter(name).await;
      }
   }

   /// Handles an adapter that was turned off while in use.
   ///
   /// Its links drop with it; the device monitors report those disconnects. Adapters in
   /// any other state are left alone: they are not in use, and a lost or failed one is
   /// checked again on its next setup attempt.
   fn handle_adapter_powered_off(&mut self, name: &SmolStr) {
      if let Some(info) = self.adapters.get_mut(name)
         && info.state == AdapterState::Active
      {
         Self::mark_powered_off(info);
      }
   }

   fn handle_adapter_error(&mut self, name: &SmolStr, error: String) {
      self.stop_adapter_device_monitors(name);
      if let Some(info) = self.adapters.get_mut(name)
         && let Some(handle) = info.monitor_handle.take()
      {
         handle.abort();
      }
      self.schedule_adapter_retry(name, format!("Adapter error on {name}: {error}"));
   }

   async fn handle_device_discovered(&mut self, addr: Address, adapter_name: SmolStr) {
      // A known device is reported again when the periodic scan finds it connected without
      // AAP. Reconnect it unless a retry is already pending or the user disconnected it.
      if let Some(device) = self.devices.get(&addr) {
         if device.wants_aap()
            && let Err(e) = self.establish_aap_connection(addr).await
         {
            debug!("AAP connection to {addr} not started: {e}");
         }
         return;
      }

      // Verify it's an AirPods device
      let Some(adapter_info) = self.adapters.get(&adapter_name) else {
         return;
      };

      let Ok(device) = adapter_info.adapter.device(addr) else {
         return;
      };

      if !self.is_airpods_device(&device).await {
         return;
      }

      // Only proceed if already connected by bluetoothd
      if !device.is_connected().await.unwrap_or(false) {
         debug!("Discovered AirPods at {addr} but not connected by system");
         return;
      }

      let name = device
         .name()
         .await
         .ok()
         .flatten()
         .unwrap_or_else(|| addr.to_string());
      info!("Found connected AirPods: {name} ({addr})");

      // Create managed device
      let product_id = airpods::recognition::product_id(&device).await;
      let airpods = AirPods::new(addr, name, product_id, self.battery_study.clone());
      let managed = ManagedDevice {
         device: airpods,
         bluetooth_state: BluetoothState::Connected,
         aap_state: AAPState::Disconnected,
         adapter_name,
         aap_retry_count: 0,
         last_aap_error: None,
         aap_handle: None,
         aap_attempt: 0,
         retry_handle: None,
         user_disconnected: false,
      };

      self.devices.insert(addr, managed);

      // Establish AAP connection for already-connected device
      let _ = self.establish_aap_connection(addr).await;
   }

   async fn handle_bluetooth_connected(&mut self, addr: Address, adapter_name: SmolStr) {
      let Some(device) = self.devices.get_mut(&addr) else {
         // Not managed yet: adopted if it is AirPods
         self.handle_device_discovered(addr, adapter_name).await;
         return;
      };
      // Reported again by the scan or a restarted monitor: nothing changed
      if device.bluetooth_state == BluetoothState::Connected {
         return;
      }

      device.bluetooth_state = BluetoothState::Connected;
      // A new Bluetooth session connects automatically again
      device.user_disconnected = false;
      if let Err(e) = self.establish_aap_connection(addr).await {
         debug!("AAP connection to {addr} not started: {e}");
      }
   }

   async fn handle_bluetooth_disconnected(&mut self, addr: Address) {
      if let Some(device) = self.devices.get_mut(&addr)
         && device.bluetooth_state == BluetoothState::Connected
      {
         device.mark_bluetooth_disconnected(&self.event_tx).await;
      }
   }

   fn handle_aap_connected(&mut self, addr: Address, attempt: u64) {
      let Some(device) = self.devices.get_mut(&addr) else {
         return;
      };
      if device.aap_attempt != attempt {
         debug!("Ignoring AAP connect of a superseded attempt for {addr}");
         return;
      }

      device.aap_state = AAPState::Connected;
      device.aap_retry_count = 0;
      device.last_aap_error = None;

      self
         .event_tx
         .emit(&device.device, AirPodsEvent::DeviceConnected);
   }

   fn handle_aap_disconnected(&mut self, addr: Address, attempt: u64, is_error: bool) {
      let Some(device) = self.devices.get_mut(&addr) else {
         return;
      };
      if device.aap_attempt != attempt {
         debug!("Ignoring AAP disconnect of a superseded attempt for {addr}");
         return;
      }
      device.aap_handle = None;

      if is_error && device.bluetooth_state == BluetoothState::Connected {
         // Only retry AAP if Bluetooth is still connected
         device.aap_state = AAPState::WaitingToReconnect;
         device.aap_retry_count = device.aap_retry_count.saturating_add(1);

         // Schedule AAP reconnection with backoff
         let loopback = self.loopback_tx.clone();
         let delay = calc_retry_delay(device.aap_retry_count);
         info!("AAP connection to {addr} failed, retrying in {delay:?}");

         device.retry_handle = Some(tokio::spawn(async move {
            time::sleep(delay).await;
            let _ = loopback.send(ManagerCommand::RetryAAP(addr, attempt)).await;
         }));
      } else {
         device.aap_state = AAPState::Disconnected;
         device.aap_retry_count = 0;
      }
   }

   /// Runs the delayed reconnect scheduled by [`Self::handle_aap_disconnected`].
   async fn handle_aap_retry(&mut self, addr: Address, attempt: u64) {
      let Some(device) = self.devices.get_mut(&addr) else {
         return;
      };
      if device.aap_attempt != attempt || device.aap_state != AAPState::WaitingToReconnect {
         return;
      }

      // No longer waiting: if the attempt cannot start now, the periodic scan tries again
      device.retry_handle = None;
      device.aap_state = AAPState::Disconnected;
      if let Err(e) = self.establish_aap_connection(addr).await {
         debug!("AAP reconnect to {addr} not started: {e}");
      }
   }

   /// Handles a device reported as removed from `BlueZ`.
   ///
   /// bluer reports the removal of any interface of a device as the device being removed,
   /// and ends its event stream. `BlueZ` removes `Battery1` on every disconnect, so the
   /// device is forgotten only if `BlueZ` no longer lists it; otherwise it is watched again.
   async fn handle_device_lost(&mut self, addr: Address, adapter_name: SmolStr) {
      // The monitor's event stream has ended either way
      self.device_monitors.remove(&addr);

      let still_listed = match self.adapters.get(&adapter_name) {
         Some(info) => info
            .adapter
            .device_addresses()
            .await
            .is_ok_and(|addresses| addresses.contains(&addr)),
         None => false,
      };
      if still_listed {
         debug!("{addr} lost a BlueZ interface but is still present, watching it again");
         self.monitor_device(addr, &adapter_name);
         return;
      }

      if let Some(mut device) = self.devices.remove(&addr) {
         device.close_aap(&self.event_tx).await;
      }
   }

   async fn establish_aap_connection(&mut self, addr: Address) -> Result<()> {
      let device = self
         .devices
         .get_mut(&addr)
         .ok_or(AirPodsError::DeviceNotFound(addr))?;

      match device.aap_state {
         // Nothing to do: a second connection would only tear down the first
         AAPState::Connected => return Ok(()),
         AAPState::Connecting => return Err(AirPodsError::AlreadyConnecting),
         _ => {},
      }

      // Check adapter is available
      let adapter_info = self
         .adapters
         .get(&device.adapter_name)
         .ok_or(AirPodsError::AdapterNotFound)?;

      if adapter_info.state != AdapterState::Active {
         return Err(AirPodsError::AdapterNotAvailable);
      }

      // Only work with bluetooth-connected devices
      if device.bluetooth_state != BluetoothState::Connected {
         return Err(AirPodsError::DeviceNotConnected);
      }

      // Get BlueZ device to verify it's paired
      let bluer_device = adapter_info.adapter.device(addr)?;
      if !bluer_device.is_paired().await.unwrap_or(false) {
         return Err(AirPodsError::DeviceNotPaired);
      }

      // The cached state can be stale (missed event, adapter restart). Connecting AAP
      // without a link would page the device, so ask BlueZ.
      if !bluer_device.is_connected().await? {
         debug!("{addr} has no Bluetooth link, not connecting AAP");
         device.mark_bluetooth_disconnected(&self.event_tx).await;
         return Err(AirPodsError::DeviceNotConnected);
      }

      // This attempt replaces a pending retry and any earlier task
      device.stop_aap();
      self.next_attempt += 1;
      let attempt = self.next_attempt;
      device.aap_attempt = attempt;

      // Spawn AAP connection task
      let airpods = device.device.clone();
      let event_tx = self.event_tx.clone();
      let loopback = self.loopback_tx.clone();

      let handle = tokio::spawn(async move {
         let err = match time::timeout(AAP_CONNECTION_TIMEOUT, airpods.connect(&event_tx)).await {
            Ok(Err(e)) => {
               warn!("Failed to establish AAP connection to {addr}: {e}");
               Some(e)
            },
            Err(_) => {
               warn!("AAP connection to {addr} timed out");
               Some(AirPodsError::RequestTimeout)
            },
            Ok(Ok(jhandle)) => {
               if let Err(e) = loopback
                  .send(ManagerCommand::AAPConnected(addr, attempt))
                  .await
               {
                  warn!("Channel overflow sending AAP connected: {e}");
                  return;
               }

               let err = match jhandle.await {
                  Ok(x) => x,
                  Err(x) => Some(AirPodsError::ActorPanicked(x)),
               };

               if let Some(err) = &err {
                  warn!("AAP connection to {addr} terminated: {err:?}");
               } else {
                  info!("AAP connection to {addr} closed cleanly");
               }
               err
            },
         };
         if let Err(e) = loopback
            .send(ManagerCommand::AAPDisconnected(
               addr,
               attempt,
               err.is_some(),
            ))
            .await
         {
            warn!("Channel overflow sending AAP disconnected: {e}");
         }
      });

      // Track AAP handle
      device.aap_handle = Some(handle);
      device.aap_state = AAPState::Connecting;

      Ok(())
   }

   async fn disconnect_aap(&mut self, addr: Address) -> Result<()> {
      let device = self
         .devices
         .get_mut(&addr)
         .ok_or(AirPodsError::DeviceNotFound(addr))?;

      // Stop the connection and keep it down until the user or Bluetooth reconnects
      device.aap_state = AAPState::Disconnected;
      device.user_disconnected = true;
      device.close_aap(&self.event_tx).await;

      Ok(())
   }

   async fn cleanup(&mut self) {
      use tokio::time::timeout;
      info!("Cleaning up Bluetooth manager");

      // Abort adapter monitors with timeout
      for info in self.adapters.values_mut() {
         if let Some(handle) = info.monitor_handle.take() {
            handle.abort();
            // Give it a moment to finish
            let _ = timeout(Duration::from_secs(1), handle).await;
         }
      }

      self.device_monitors.clear();

      // Abort AAP handles and disconnect all devices
      for device in self.devices.values_mut() {
         if let Some(handle) = device.retry_handle.take() {
            handle.abort();
         }
         if let Some(handle) = device.aap_handle.take() {
            handle.abort();
            // Give it a moment to finish
            let _ = timeout(Duration::from_secs(1), handle).await;
         }
         device.device.disconnect().await;
      }
   }

   async fn discover_new_adapters(&mut self) {
      match self.session.adapter_names().await {
         Ok(names) => {
            for name in names.into_iter().map(SmolStr::from) {
               if !self.adapters.contains_key(&name) {
                  self.initialize_adapter(name).await;
               }
            }
         },
         Err(e) => {
            warn!("Failed to poll adapter names: {e}. Retrying later.");
         },
      }
   }

   async fn scan_for_connected_airpods(&self) {
      for adapter_info in self.adapters.values() {
         if adapter_info.state != AdapterState::Active {
            continue;
         }

         // Check all connected devices
         if let Ok(addresses) = adapter_info.adapter.device_addresses().await {
            for addr in addresses {
               // Managed devices count only if they should be connecting on their own:
               // connected, connecting, waiting for a retry or disconnected by the user
               // are all as expected
               if self.devices.get(&addr).is_none_or(ManagedDevice::wants_aap)
                  && let Ok(device) = adapter_info.adapter.device(addr)
                  && device.is_connected().await.unwrap_or(false)
                  && self.is_airpods_device(&device).await
               {
                  info!("Scan found connected AirPods {addr} without AAP; events missed it");
                  let _ = self
                     .loopback_tx
                     .send(ManagerCommand::DeviceDiscovered(
                        addr,
                        adapter_info.name.clone(),
                     ))
                     .await;
               }
            }
         }
      }
   }

   /// Runs the periodic device work, which may write to the battery study database.
   ///
   /// The writes wait for the disk, so they run on the blocking pool instead of holding
   /// up the commands queued for the manager.
   fn tick_all_devices(&self) {
      for device in self.devices.values() {
         let device = device.device.clone();
         tokio::task::spawn_blocking(move || device.tick());
      }
   }

   async fn check_connection_health(&self) {
      for (addr, device) in &self.devices {
         if let Some(adapter_info) = self.adapters.get(&device.adapter_name)
            && let Ok(bluer_device) = adapter_info.adapter.device(*addr)
         {
            // A failed read says nothing about the link; the events or the next scan decide
            let is_connected = match bluer_device.is_connected().await {
               Ok(is_connected) => is_connected,
               Err(e) => {
                  debug!("Cannot read connection state of {addr}: {e}");
                  continue;
               },
            };

            match (device.bluetooth_state, is_connected) {
               (BluetoothState::Connected, false) => {
                  info!("Scan found {addr} disconnected; events missed it");
                  let _ = self
                     .loopback_tx
                     .send(ManagerCommand::BluetoothDisconnected(*addr))
                     .await;
               },
               (BluetoothState::Disconnected, true) => {
                  info!("Scan found {addr} connected; events missed it");
                  let _ = self
                     .loopback_tx
                     .send(ManagerCommand::BluetoothConnected(
                        *addr,
                        device.adapter_name.clone(),
                     ))
                     .await;
               },
               _ => {},
            }
         }
      }
   }
}

fn adapter_retry_delay(retry_count: u32) -> Duration {
   ADAPTER_RECOVERY_DELAY
      .saturating_mul(1_u32 << retry_count.min(5))
      .min(MAX_ADAPTER_RECOVERY_DELAY)
}

/// Delay before AAP retry number `retry_count` (from 1): 4 s, doubling up to
/// [`MAX_AAP_RETRY_DELAY`], plus up to a second of jitter.
fn calc_retry_delay(retry_count: u32) -> Duration {
   AAP_RETRY_DELAY
      .saturating_mul(1_u32 << retry_count.min(6))
      .min(MAX_AAP_RETRY_DELAY)
      + Duration::from_millis(rand::thread_rng().gen_range(0..1000))
}

#[cfg(test)]
mod managed_device_tests {
   use std::sync::Arc;

   use super::*;

   fn managed_device() -> ManagedDevice {
      ManagedDevice {
         device: AirPods::new(Address::default(), "test".into(), None, None),
         bluetooth_state: BluetoothState::Connected,
         aap_state: AAPState::Disconnected,
         adapter_name: SmolStr::new_static("hci0"),
         aap_retry_count: 0,
         last_aap_error: None,
         aap_handle: None,
         aap_attempt: 0,
         retry_handle: None,
         user_disconnected: false,
      }
   }

   #[test]
   fn connects_on_its_own_only_when_idle_and_allowed() {
      let mut device = managed_device();
      assert!(device.wants_aap());
      device.aap_state = AAPState::Failed("Adapter lost");
      assert!(device.wants_aap());

      // Connected, in progress, or a retry is already scheduled
      for state in [
         AAPState::Connected,
         AAPState::Connecting,
         AAPState::WaitingToReconnect,
      ] {
         device.aap_state = state;
         assert!(!device.wants_aap());
      }

      device.aap_state = AAPState::Disconnected;
      device.user_disconnected = true;
      assert!(!device.wants_aap());

      device.user_disconnected = false;
      device.bluetooth_state = BluetoothState::Disconnected;
      assert!(!device.wants_aap());
   }

   #[tokio::test]
   async fn lost_link_cancels_reconnect_and_stays_down() {
      let mut device = managed_device();
      let retry = tokio::spawn(std::future::pending::<()>());
      let task = retry.abort_handle();
      device.retry_handle = Some(retry);
      device.aap_state = AAPState::WaitingToReconnect;
      device.aap_attempt = 3;

      let event_tx: EventSender = Arc::new(NoEvents);
      device.mark_bluetooth_disconnected(&event_tx).await;

      assert_eq!(device.bluetooth_state, BluetoothState::Disconnected);
      assert_eq!(device.aap_state, AAPState::Disconnected);
      assert!(device.retry_handle.is_none() && device.aap_attempt == 0);
      assert!(!device.wants_aap());
      for _ in 0..10 {
         tokio::task::yield_now().await;
      }
      assert!(task.is_finished());
   }

   struct NoEvents;

   impl crate::event::EventBus for NoEvents {
      fn emit(&self, _device: &AirPods, _event: AirPodsEvent) {}
   }

   #[tokio::test]
   async fn stop_aap_cancels_tasks_and_invalidates_attempt() {
      let mut device = managed_device();
      let connection = tokio::spawn(std::future::pending::<()>());
      let retry = tokio::spawn(std::future::pending::<()>());
      let tasks = [connection.abort_handle(), retry.abort_handle()];
      device.aap_handle = Some(connection);
      device.retry_handle = Some(retry);
      device.aap_attempt = 7;

      device.stop_aap();

      assert_eq!(device.aap_attempt, 0);
      assert!(device.aap_handle.is_none() && device.retry_handle.is_none());
      for _ in 0..10 {
         tokio::task::yield_now().await;
      }
      assert!(tasks.iter().all(|task| task.is_finished()));
   }
}

#[cfg(test)]
mod adapter_retry_tests {
   use super::*;

   #[test]
   fn recovery_backoff_grows_and_stays_capped() {
      let delays: Vec<_> = (0..8).map(adapter_retry_delay).collect();
      assert_eq!(
         delays,
         [5, 10, 20, 40, 80, 120, 120, 120].map(Duration::from_secs)
      );
      assert_eq!(adapter_retry_delay(u32::MAX), MAX_ADAPTER_RECOVERY_DELAY);
   }

   #[test]
   fn aap_retry_backoff_reaches_its_cap() {
      let jitter = Duration::from_secs(1);
      for (retry_count, secs) in [
         (1, 4),
         (2, 8),
         (3, 16),
         (4, 32),
         (5, 64),
         (6, 120),
         (7, 120),
      ] {
         let delay = calc_retry_delay(retry_count);
         let base = Duration::from_secs(secs);
         assert!(
            delay >= base && delay < base + jitter,
            "retry {retry_count}: {delay:?}"
         );
      }
      assert!(calc_retry_delay(u32::MAX) < MAX_AAP_RETRY_DELAY + jitter);
   }
}

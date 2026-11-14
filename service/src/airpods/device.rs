//! `AirPods` device implementation and state management.
//!
//! This module provides the core `AirPods` type which represents a connected
//! `AirPods` device, manages its state, and handles communication over L2CAP.

use core::fmt;
use std::{
   collections::HashMap,
   sync::{
      Arc, Weak,
      atomic::{AtomicBool, AtomicU64, Ordering},
   },
   time::{Duration, SystemTime, UNIX_EPOCH},
};

use bluer::Address;
use crossbeam::atomic::AtomicCell;
use log::{debug, info, warn};
use serde_json::json;
use smol_str::{SmolStr, ToSmolStr};
use tokio::{
   sync::{RwLock, oneshot},
   task::{JoinHandle, JoinSet},
   time,
};

use crate::{
   airpods::{
      parser,
      protocol::{
         BatteryInfo, Component, EarDetectionStatus, FeatureBitmap, FeatureCmd, FeatureId,
         HDR_ACK_FEATURES, HDR_ACK_HANDSHAKE, HDR_BATTERY_STATE, HDR_EAR_DETECTION, HDR_METADATA,
         HDR_NOISE_CTL, NoiseControlMode, PKT_HANDSHAKE, PKT_REQUEST_NOTIFY, PKT_SET_FEATURES,
         build_control_packet,
      },
      recognition::NoiseCapabilities,
   },
   battery_study::{BatteryStudy, BatteryTracker},
   bluetooth::l2cap::{self, L2CapReceiver, L2CapSender, Packet},
   error::{AirPodsError, Result},
   event::{AirPodsEvent, EventSender},
};

/// Internal state for an active L2CAP connection.
#[derive(Debug)]
struct ConnectionState {
   /// Distinguishes this connection from earlier and later ones of the same device
   id: u64,
   sender: l2cap::L2CapSender,
   jset: JoinSet<()>,
}

impl Drop for ConnectionState {
   fn drop(&mut self) {
      self.jset.abort_all();
   }
}

/// Internal shared state for an `AirPods` device.
#[derive(Debug, Default)]
struct AirPodsInner {
   address: Address,
   address_str: SmolStr,
   name: SmolStr,
   battery: AtomicCell<Option<BatteryInfo>>,
   is_connected: AtomicBool,
   connected_since: AtomicCell<Option<u64>>,
   ear_detection: AtomicCell<Option<EarDetectionStatus>>,
   /// `[primary pod, secondary pod]` bytes of the last ear detection report, to map them
   /// again when a battery report shows that the other earbud is primary
   ear_detection_pods: AtomicCell<Option<[u8; 2]>>,
   /// The right earbud is the primary one, as last seen in a battery report
   right_is_primary: AtomicBool,
   noise_mode: AtomicCell<Option<NoiseControlMode>>,
   /// Battery time remaining in minutes, recalculated on each battery report
   battery_ttl: AtomicCell<Option<u32>>,
   product_id: Option<u32>,
   features: FeatureBitmap,
   features_present: FeatureBitmap,
   conn: RwLock<Option<ConnectionState>>,
   next_conn_id: AtomicU64,
   battery_tracker: parking_lot::Mutex<BatteryTracker>,
   /// Battery study writes running on the blocking pool, so shutdown can wait for them
   battery_writes: parking_lot::Mutex<Vec<JoinHandle<()>>>,
}

/// Represents a connected `AirPods` device.
///
/// This type is cheaply cloneable and thread-safe.
#[derive(Clone)]
pub struct AirPods(Arc<AirPodsInner>);

/// Weak reference to an `AirPods` device.
#[derive(Debug, Clone)]
pub struct WeakAirPods(Weak<AirPodsInner>);

impl WeakAirPods {
   pub fn new(airpods: &AirPods) -> Self {
      Self(Arc::downgrade(&airpods.0))
   }

   pub fn upgrade(&self) -> Option<AirPods> {
      self.0.upgrade().map(AirPods)
   }
}

impl fmt::Debug for AirPods {
   fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
      fmt::Debug::fmt(&self.0, f)
   }
}

/// Represents the result of an update operation on device state.
#[derive(Debug, Clone, Copy)]
pub enum UpdateOp<T> {
   /// No change occurred
   Noop,
   /// A new value was inserted (None -> Some)
   Inserted,
   /// A value was deleted (Some -> None)
   Deleted(T),
   /// An existing value was updated
   Updated(T),
}

impl<T: PartialEq> UpdateOp<T> {
   fn apply_atomic(dst: &AtomicCell<Option<T>>, new: Option<T>) -> Self
   where
      T: Copy,
   {
      Self::new(dst.swap(new), new)
   }

   fn new(prev: Option<T>, new: Option<T>) -> Self {
      match (prev, new) {
         (Some(p), Some(n)) if p == n => Self::Noop,
         (None, Some(_)) => Self::Inserted,
         (Some(p), None) => Self::Deleted(p),
         (Some(_), Some(n)) => Self::Updated(n),
         (None, None) => Self::Noop,
      }
   }

   const fn is_updated(&self) -> bool {
      matches!(self, Self::Inserted | Self::Updated(_))
   }
}

impl AirPods {
   /// Creates a new `AirPods` device instance.
   pub fn new(
      address: Address,
      name: String,
      product_id: Option<u32>,
      battery_study: Option<BatteryStudy>,
   ) -> Self {
      Self(Arc::new(AirPodsInner {
         address,
         address_str: address.to_smolstr(),
         product_id,
         name: name.into(),
         battery_tracker: parking_lot::Mutex::new(BatteryTracker::new(battery_study)),
         ..Default::default()
      }))
   }

   /// Gets the address of the Airpod.
   pub fn address(&self) -> Address {
      self.0.address
   }

   /// Gets the address string of the Airpod.
   pub fn address_str(&self) -> &SmolStr {
      &self.0.address_str
   }

   /// Gets the name of the Airpod.
   pub fn name(&self) -> &SmolStr {
      &self.0.name
   }

   /// Gets the battery information of the Airpod.
   pub fn battery_info(&self) -> Option<BatteryInfo> {
      self.0.battery.load()
   }

   /// Replaces the battery information of the Airpod.
   pub fn update_battery_info(
      &self,
      battery: impl Into<Option<BatteryInfo>>,
   ) -> UpdateOp<BatteryInfo> {
      UpdateOp::apply_atomic(&self.0.battery, battery.into())
   }

   /// Checks if the Airpod is connected.
   pub fn is_connected(&self) -> bool {
      self.0.is_connected.load(Ordering::Relaxed)
   }

   /// Gets the ear detection status of the Airpod.
   pub fn ear_detection(&self) -> Option<EarDetectionStatus> {
      self.0.ear_detection.load()
   }

   /// Sets the ear detection status of the Airpod and returns the previous one.
   pub fn update_ear_detection(&self, status: EarDetectionStatus) -> Option<EarDetectionStatus> {
      self.0.ear_detection.swap(Some(status))
   }

   /// Gets the noise control mode of the Airpod.
   pub fn noise_mode(&self) -> Option<NoiseControlMode> {
      self.0.noise_mode.load()
   }

   /// Sets the noise control mode of the Airpod.
   pub fn update_noise_mode(
      &self,
      mode: impl Into<Option<NoiseControlMode>>,
   ) -> UpdateOp<NoiseControlMode> {
      UpdateOp::apply_atomic(&self.0.noise_mode, mode.into())
   }

   /// Converts the device state to a JSON representation.
   pub fn to_json(&self) -> serde_json::Value {
      let capabilities = NoiseCapabilities::from_product_id(self.0.product_id);
      let mut info = json!({
          "address": self.address_str().as_str(),
          "name": self.name().as_str(),
          "connected": self.is_connected(),
          "capabilities": {
             "noise_control": capabilities.noise_control,
             "adaptive_audio": capabilities.adaptive_audio,
          },
      });

      if let Some(connected_since) = self.0.connected_since.load() {
         info["connected_since"] = json!(connected_since);
      }

      if let Some(battery) = self.battery_info() {
         info["battery"] = battery.to_json();
      }

      // Add battery TTL estimate
      info["battery_ttl_estimate"] = match self.battery_ttl_estimate() {
         Some(minutes) => json!(minutes),
         None => json!(null),
      };

      if let Some(mode) = self.noise_mode() {
         info["noise_mode"] = json!(mode.to_str());
      }

      if let Some(ear) = self.ear_detection() {
         info["ear_detection"] = ear.to_json();
      }

      let features_dict: HashMap<_, _> = self
         .features()
         .into_iter()
         .map(|(k, v)| (k.to_str(), v))
         .collect();
      info["features"] = json!(features_dict);
      info
   }

   pub fn feature_enabled(&self, feature: FeatureId) -> bool {
      self.0.features.get(feature)
   }

   pub fn features(&self) -> Vec<(FeatureId, bool)> {
      self
         .0
         .features_present
         .iter()
         .map(|feat| (feat, self.feature_enabled(feat)))
         .collect()
   }

   pub fn set_feature_enabled(&self, feature: FeatureId, enabled: bool) -> bool {
      self.0.features_present.set(feature, true);
      self.0.features.set(feature, enabled)
   }

   /// Establishes an L2CAP connection to the `AirPods` device.
   ///
   /// Returns a join handle that resolves when the connection is closed.
   pub async fn connect(&self, event_tx: &EventSender) -> Result<JoinHandle<Option<AirPodsError>>> {
      info!("Connecting to AirPods at {}", self.address());
      let mut conn = self.0.conn.write().await;
      if conn.take().is_some() {
         // Replaced a connection that was still open; its packet processor is ignored from
         // here on, because the connection ids no longer match
         self.mark_disconnected();
      }
      let id = self.0.next_conn_id.fetch_add(1, Ordering::Relaxed);

      // Create L2CAP connection
      let mut jset = JoinSet::new();

      // Perform handshake
      let (receiver, sender) = self.start_connection(&mut jset).await?;

      // Start packet processor with direct access to fields
      let jhandle = self.start_packet_processor(id, receiver, event_tx.clone());

      // Store connection state
      *conn = Some(ConnectionState { id, sender, jset });
      self.0.connected_since.store(
         SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .ok()
            .map(|duration| duration.as_secs()),
      );
      self.0.is_connected.store(true, Ordering::Relaxed);

      // Initialize battery study session
      let study = self.0.battery_tracker.lock().study().cloned();
      if let Some(study) = study {
         let (address, name) = (self.address(), self.name().clone());
         self.spawn_battery_write(move || study.init_session(address, &name));
      }

      info!("Successfully connected to {}", self.address());
      Ok(jhandle)
   }

   /// Closes the connection and clears the reported state.
   ///
   /// Returns whether a connection was open. The caller reports the disconnect then:
   /// the packet processor of a closed connection stays silent.
   pub async fn disconnect(&self) -> bool {
      let mut conn = self.0.conn.write().await;
      let was_open = conn.take().is_some();
      self.mark_disconnected();
      was_open
   }

   /// Called by the packet processor of connection `conn_id` when that connection ended.
   ///
   /// Does nothing if the connection was already closed by [`Self::disconnect`] or replaced
   /// by a newer one, so a late processor cannot tear down a connection it does not own.
   async fn notify_disconnected(&self, conn_id: u64, event_tx: &EventSender) {
      let mut conn = self.0.conn.write().await;
      if conn.as_ref().is_none_or(|conn| conn.id != conn_id) {
         debug!(
            "{}: connection {conn_id} was already closed or replaced",
            self.address()
         );
         return;
      }
      conn.take();
      self.mark_disconnected();
      drop(conn);
      event_tx.emit(self, AirPodsEvent::DeviceDisconnected);
   }

   /// Updates the device state after its connection was dropped. Call with `conn` locked.
   fn mark_disconnected(&self) {
      // Save battery study data collected over the connection (reads the noise mode)
      self.end_battery_session();

      // Several paths close a connection; only the first one logs it
      let was_connected = self.0.is_connected.swap(false, Ordering::Relaxed);
      self.0.connected_since.store(None);

      // What the device reported is only valid for the connection that delivered it. The
      // next connection starts without it and waits for fresh reports.
      self.0.battery.store(None);
      self.0.noise_mode.store(None);
      self.0.ear_detection.store(None);
      self.0.ear_detection_pods.store(None);
      self.0.battery_ttl.store(None);
      if was_connected {
         info!("Disconnected from {}", self.address());
      }
   }

   async fn start_connection(
      &self,
      jset: &mut JoinSet<()>,
   ) -> Result<(L2CapReceiver, L2CapSender)> {
      async fn wait_for_ack<T>(tx: &mut oneshot::Receiver<T>) -> Result<T> {
         time::timeout(Duration::from_secs(5), tx)
            .await
            .map_err(|_| AirPodsError::RequestTimeout)?
            .map_err(|_| AirPodsError::ConnectionClosed)
      }

      let (hs_ack_tx, mut hs_ack_rx) = oneshot::channel();
      let (feat_ack_tx, mut feat_ack_rx) = oneshot::channel();

      let hooks = l2cap::Hooks::new()
         .prefix_once(HDR_ACK_HANDSHAKE, |_| {
            let _ = hs_ack_tx.send(());
         })
         .prefix_once(HDR_ACK_FEATURES, |_| {
            let _ = feat_ack_tx.send(());
         });

      let (receiver, sender) = l2cap::connect(jset, hooks, self.address(), None).await?;
      debug!("Starting handshake sequence...");

      // Send handshake
      if let Err(e) = sender.send(PKT_HANDSHAKE).await {
         warn!("Failed to send handshake: {e:?}");
         return Err(e);
      } else if let Err(e) = wait_for_ack(&mut hs_ack_rx).await {
         warn!("No handshake acknowledgment received ({e:?}), continuing anyway...");
      } else {
         debug!("Handshake acknowledged");
      }

      // Send features
      if let Err(e) = sender.send(PKT_SET_FEATURES).await {
         warn!("Failed to send features: {e:?}");
         return Err(e);
      } else if let Err(e) = wait_for_ack(&mut feat_ack_rx).await {
         warn!("No features acknowledgment received ({e:?}), continuing anyway...");
      } else {
         debug!("Features acknowledged");
      }

      // Request notifications
      if let Err(e) = sender.send(PKT_REQUEST_NOTIFY).await {
         warn!("Failed to send notification request: {e:?}");
         return Err(e);
      }

      // Schedule retry for notifications with battery status check
      let weak = WeakAirPods::new(self);
      let mac = self.address();
      info!("{mac}: Handshake sequence completed");
      jset.spawn({
            let sender = sender.clone();
            async move {
                const RETRY_SCHEDULE: &[Duration] = &[
                    Duration::from_secs(2),
                    Duration::from_secs(3),
                    Duration::from_secs(5),
                    Duration::from_secs(10),
                ];

                time::sleep(Duration::from_secs(1)).await;
                for (i, delay) in RETRY_SCHEDULE.iter().enumerate() {
                    if let Some(this) = weak.upgrade()
                        && this.battery_info().is_some() {
                            info!("{mac}: Battery status established after {i} retries!");
                            return;
                        }
                    warn!(
                        "{mac}: [Retry {i}] No battery status received after notification request, retrying in {delay:?}..."
                    );
                    let _ = sender.send(PKT_REQUEST_NOTIFY).await;
                    time::sleep(*delay).await;
                }
            }
        });
      Ok((receiver, sender))
   }

   fn start_packet_processor(
      &self,
      conn_id: u64,
      mut rx: l2cap::L2CapReceiver,
      event_tx: EventSender,
   ) -> JoinHandle<Option<AirPodsError>> {
      let addr = self.address();
      let weak = WeakAirPods::new(self);
      tokio::spawn(async move {
         let mut err = None;
         loop {
            match rx.recv().await {
               Ok(packet) => {
                  if let Some(this) = weak.upgrade() {
                     this.process_packet(addr, packet, &event_tx);
                  } else {
                     warn!("{addr}: Airpod instance was dropped");
                     break;
                  }
               },
               Err(e) => {
                  if let Some(this) = weak.upgrade() {
                     this.notify_disconnected(conn_id, &event_tx).await;
                  } else {
                     warn!("{addr}: Connection closed: {e:?}");
                  }
                  err = Some(e);
                  break;
               },
            }
         }
         err
      })
   }

   pub async fn set_noise_control(&self, mode: NoiseControlMode) -> Result<()> {
      let conn = self.0.conn.read().await;
      if let Some(conn) = conn.as_ref() {
         let packet = build_control_packet(0x0D, (mode as u32).to_le_bytes());
         conn.sender.send(&packet).await?;
         self.0.noise_mode.store(Some(mode));
         Ok(())
      } else {
         Err(AirPodsError::DeviceNotConnected)
      }
   }

   pub async fn passthrough(&self, packet: &[u8]) -> Result<()> {
      let conn = self.0.conn.read().await;
      if let Some(conn) = conn.as_ref() {
         conn.sender.send(packet).await?;
         Ok(())
      } else {
         Err(AirPodsError::DeviceNotConnected)
      }
   }

   pub async fn set_feature(&self, feature: FeatureId, enabled: bool) -> Result<()> {
      let conn = self.0.conn.read().await;
      if let Some(conn) = conn.as_ref() {
         let packet = if enabled {
            FeatureCmd::Enable.build(feature.id())
         } else {
            FeatureCmd::Disable.build(feature.id())
         };
         conn.sender.send(&packet).await?;
         self.set_feature_enabled(feature, enabled);
         Ok(())
      } else {
         Err(AirPodsError::DeviceNotConnected)
      }
   }

   /// The earbud the device currently uses as primary; left until a battery report says.
   fn primary_pod(&self) -> Component {
      if self.0.right_is_primary.load(Ordering::Relaxed) {
         Component::Right
      } else {
         Component::Left
      }
   }

   /// Records which earbud is primary, as seen in a battery report.
   ///
   /// Ear detection reports name the primary and secondary earbud, not left and right. If
   /// the primary changed, or the last ear report came before the first battery report,
   /// that report was mapped to the wrong sides: map it again.
   fn set_primary_pod(&self, address: Address, primary: Component, event_tx: &EventSender) {
      let right_is_primary = primary == Component::Right;
      let changed = self
         .0
         .right_is_primary
         .swap(right_is_primary, Ordering::Relaxed)
         != right_is_primary;
      if changed && let Some(pods) = self.0.ear_detection_pods.load() {
         debug!("{address}: primary earbud is now {primary}, mapping ear detection again");
         let status = parser::ear_detection_from_pods(pods, primary);
         self.apply_ear_detection(address, status, event_tx);
      }
   }

   /// Stores an ear detection status and reports it if it changed.
   fn apply_ear_detection(
      &self,
      address: Address,
      status: EarDetectionStatus,
      event_tx: &EventSender,
   ) {
      debug!(
         "Ear detection updated for {}: L:{} R:{} (in case L:{} R:{})",
         address,
         status.is_left_in_ear(),
         status.is_right_in_ear(),
         status.is_left_in_case(),
         status.is_right_in_case()
      );
      let previous = self.update_ear_detection(status);
      if previous != Some(status) {
         event_tx.emit(self, AirPodsEvent::EarDetectionChanged { status, previous });
      }
   }

   fn process_packet(&self, address: Address, packet: Packet, event_tx: &EventSender) {
      // Battery status
      if packet.starts_with(HDR_BATTERY_STATE) {
         match parser::parse_battery_status(&packet) {
            Ok(battery) => {
               if let Some(primary) = parser::primary_pod(&packet) {
                  self.set_primary_pod(address, primary, event_tx);
               }
               debug!(
                  "Battery updated for {}: L:{}% R:{}% C:{}%",
                  address, battery.left.level, battery.right.level, battery.case.level
               );

               // Send event if battery changed
               if self.update_battery_info(battery).is_updated() {
                  // split_ref yields the headphone battery for AirPods Max
                  let (l, r) = battery.split_ref();
                  self.0.battery_tracker.lock().record_battery_drop(*l, *r);
                  self.refresh_battery_ttl(&battery);
                  event_tx.emit(self, AirPodsEvent::BatteryUpdated(battery));
               }
            },
            Err(e) => warn!("Failed to parse battery: {e}"),
         }
      }
      // Noise control mode
      else if packet.starts_with(HDR_NOISE_CTL) {
         match parser::parse_noise_mode(&packet) {
            Ok(mode) => {
               debug!("Noise mode updated for {address}: {mode}");
               if self.update_noise_mode(mode).is_updated() {
                  event_tx.emit(self, AirPodsEvent::NoiseControlChanged(mode));
               }
            },
            Err(e) => warn!("Failed to parse noise mode: {e}"),
         }
      }
      // Ear detection
      else if packet.starts_with(HDR_EAR_DETECTION) {
         match parser::parse_ear_detection(&packet, self.primary_pod()) {
            Ok(status) => {
               // Parsing checked the length
               self
                  .0
                  .ear_detection_pods
                  .store(Some([packet[6], packet[7]]));
               self.apply_ear_detection(address, status, event_tx);
            },
            Err(e) => warn!("Failed to parse ear detection: {e}"),
         }
      }
      // Metadata packets (device details, not used)
      else if packet.starts_with(HDR_METADATA) {
         debug!("Received metadata from {address} ({} bytes)", packet.len());
      }
      // Other packets
      else if packet.starts_with(HDR_ACK_HANDSHAKE) {
         debug!("Received handshake ACK from {address}");
      } else if packet.starts_with(HDR_ACK_FEATURES) {
         debug!("Received features ACK from {address}");
      } else if let Some((cmd, op)) = FeatureCmd::parse(&packet) {
         debug!("Received feature command from {address}: {cmd} {op:?}");
         if matches!(op, FeatureCmd::Enable | FeatureCmd::Disable) {
            self.set_feature_enabled(cmd, matches!(op, FeatureCmd::Enable));
         }
      } else {
         let data = if packet.len() < 16 {
            hex::encode(&packet)
         } else {
            format!(
               "{}..{}",
               hex::encode(&packet[..8]),
               hex::encode(&packet[8..])
            )
         };

         debug!(
            "Unknown packet from {} | {} bytes => {}",
            address,
            packet.len(),
            data
         );
      }
   }

   /// Battery time remaining in minutes, as of the last battery report.
   pub fn battery_ttl_estimate(&self) -> Option<u32> {
      self.0.battery_ttl.load()
   }

   /// Recalculates the battery time remaining from a new battery report.
   ///
   /// Each call moves the smoothed estimate one step, so it must run once per report,
   /// never on reads.
   fn refresh_battery_ttl(&self, battery: &BatteryInfo) {
      const EARBUDS_DEFAULT_DRAIN_RATE: f64 = 16.9; // %/hr
      const HEADPHONE_DEFAULT_DRAIN_RATE: f64 = 5.0; // %/hr, ~20 h rated listening

      let mode = self.noise_mode();

      // Get estimate from battery tracker, or use default
      let ttl = self
         .0
         .battery_tracker
         .lock()
         .estimate_ttl(battery, mode, self.address())
         .or_else(|| {
            // Nothing is draining while charging: the tracker has no estimate on purpose
            let (left, right) = battery.split_ref();
            if left.is_charging() || right.is_charging() {
               return None;
            }

            // Default drain rate until local/historical data is available
            let default_rate = if battery.headphone.is_available() {
               HEADPHONE_DEFAULT_DRAIN_RATE
            } else {
               EARBUDS_DEFAULT_DRAIN_RATE
            };
            let min_level = f64::from(left.level.min(right.level));
            let minutes = (min_level / default_rate * 60.0) as u32;
            (minutes > 0).then_some(minutes)
         });
      self.0.battery_ttl.store(ttl);
   }

   /// Saves the battery study data of the connection that ended and forgets it.
   ///
   /// The history is cleared at once, so a reconnect starts afresh; the database write
   /// waits for the disk, so it runs on the blocking pool instead of holding up the
   /// caller, which is often the manager.
   fn end_battery_session(&self) {
      let mode = self.noise_mode().unwrap_or_default();
      let pending = self
         .0
         .battery_tracker
         .lock()
         .take_session(self.address(), mode);
      if let Some(pending) = pending {
         self.spawn_battery_write(move || {
            pending.write();
         });
      }
   }

   /// Runs a battery study write on the blocking pool and keeps track of it.
   fn spawn_battery_write(&self, write: impl FnOnce() + Send + 'static) {
      let mut writes = self.0.battery_writes.lock();
      writes.retain(|write| !write.is_finished());
      writes.push(tokio::task::spawn_blocking(write));
   }

   /// Waits for the battery study writes started so far, such as the save of the session
   /// that a disconnect ended.
   pub async fn flush_battery_writes(&self) {
      let writes = std::mem::take(&mut *self.0.battery_writes.lock());
      for write in writes {
         let _ = write.await;
      }
   }

   /// Checks if enough time has passed and samples collected to warrant a periodic save.
   /// Returns true if data should be saved.
   fn should_save_battery_study(&self, interval_minutes: u32) -> bool {
      if let Some(battery) = self.battery_info() {
         self
            .0
            .battery_tracker
            .lock()
            .should_save(interval_minutes, &battery)
      } else {
         false
      }
   }

   /// Performs a periodic save of battery study data if conditions are met.
   fn periodic_battery_save(&self) {
      let mode = self.noise_mode().unwrap_or_default();
      self
         .0
         .battery_tracker
         .lock()
         .save_to_study(self.address(), mode);
   }

   /// Performs all periodic tasks for the device.
   pub fn tick(&self) {
      if self.is_connected() {
         if self.should_save_battery_study(5) {
            debug!("Performing periodic battery save for {}", self.address());
            self.periodic_battery_save();
         } else {
            debug!("Battery save check for {} returned false", self.address());
         }
      } else {
         debug!("Device {} not connected, skipping tick", self.address());
      }
   }
}

#[cfg(test)]
mod tests {
   use super::*;
   use crate::{
      airpods::protocol::{BatteryStatus, PodPlacement},
      event::EventBus,
      media_control,
   };

   #[tokio::test]
   async fn connected_since_is_serialized_and_cleared_on_disconnect() {
      let device = AirPods::new(Address::default(), "test".into(), None, None);
      assert!(device.to_json().get("connected_since").is_none());

      device.0.connected_since.store(Some(123));
      device.0.is_connected.store(true, Ordering::Relaxed);
      assert_eq!(device.to_json()["connected_since"], 123);

      device.disconnect().await;
      assert!(device.to_json().get("connected_since").is_none());
   }

   #[derive(Default)]
   struct RecordedEvents(parking_lot::Mutex<Vec<AirPodsEvent>>);

   impl EventBus for RecordedEvents {
      fn emit(&self, _device: &AirPods, event: AirPodsEvent) {
         self.0.lock().push(event);
      }
   }

   async fn set_connection(device: &AirPods, id: u64) {
      *device.0.conn.write().await = Some(ConnectionState {
         id,
         sender: L2CapSender::disconnected(),
         jset: JoinSet::new(),
      });
      device.0.is_connected.store(true, Ordering::Relaxed);
   }

   #[tokio::test]
   async fn reported_state_is_cleared_on_disconnect() {
      let device = AirPods::new(Address::default(), "test".into(), None, None);
      let mut battery = BatteryInfo::new();
      battery.left.status = BatteryStatus::Normal;
      device.update_battery_info(battery);
      device.update_noise_mode(NoiseControlMode::Active);
      device.update_ear_detection(EarDetectionStatus::new(
         PodPlacement::InEar,
         PodPlacement::InEar,
      ));

      device.disconnect().await;

      // A reconnect must wait for fresh reports instead of reusing these
      assert!(device.battery_info().is_none());
      assert!(device.noise_mode().is_none());
      assert!(device.ear_detection().is_none());
      let json = device.to_json();
      for key in ["battery", "noise_mode", "ear_detection"] {
         assert!(json.get(key).is_none(), "{key} survived the disconnect");
      }
   }

   /// Battery report with one `(component, level, status)` entry per component.
   fn battery_packet(entries: &[(Component, u8, BatteryStatus)]) -> Packet {
      let mut packet = Packet::from_slice(HDR_BATTERY_STATE);
      packet.push(entries.len() as u8);
      for &(component, level, status) in entries {
         packet.extend([component as u8, 0x01, level, status as u8, 0x01]);
      }
      packet
   }

   #[tokio::test]
   async fn battery_estimate_changes_on_reports_not_on_reads() {
      let device = AirPods::new(Address::default(), "test".into(), None, None);
      let event_tx: EventSender = Arc::new(RecordedEvents::default());
      let report_with_status = |left: u8, left_status: BatteryStatus, right: u8| {
         let packet = battery_packet(&[
            (Component::Left, left, left_status),
            (Component::Right, right, BatteryStatus::Discharging),
         ]);
         device.process_packet(device.address(), packet, &event_tx);
      };
      let report =
         |left: u8, right: u8| report_with_status(left, BatteryStatus::Discharging, right);
      assert_eq!(device.battery_ttl_estimate(), None);

      report(80, 85);
      let estimate = device.battery_ttl_estimate();
      assert!(estimate.is_some());
      for _ in 0..5 {
         assert_eq!(device.to_json()["battery_ttl_estimate"], json!(estimate));
      }

      report(70, 85);
      assert!(device.battery_ttl_estimate() < estimate);

      // A charging bud has no time left to report, not even from the default rate
      report_with_status(70, BatteryStatus::Charging, 85);
      assert_eq!(device.battery_ttl_estimate(), None);
      assert_eq!(device.to_json()["battery_ttl_estimate"], json!(null));

      // The estimate returns once nothing is charging
      report(90, 85);
      assert!(device.battery_ttl_estimate().is_some());

      device.disconnect().await;
      assert_eq!(device.battery_ttl_estimate(), None);
   }

   #[tokio::test]
   async fn first_ear_detection_report_of_a_connection_is_flagged() {
      let device = AirPods::new(Address::default(), "test".into(), None, None);
      let events = Arc::new(RecordedEvents::default());
      let event_tx: EventSender = events.clone();
      let report = |left_out: u8, right_out: u8| {
         let mut packet = Packet::from_slice(HDR_EAR_DETECTION);
         packet.extend([left_out, right_out]);
         device.process_packet(device.address(), packet, &event_tx);
      };
      let first_reports = || -> Vec<bool> {
         events
            .0
            .lock()
            .drain(..)
            .map(|event| match event {
               AirPodsEvent::EarDetectionChanged { previous, .. } => previous.is_none(),
               other => panic!("unexpected event {other:?}"),
            })
            .collect()
      };

      // Connected with both buds out, then both are put in
      report(1, 1);
      report(0, 0);
      assert_eq!(first_reports(), [true, false]);

      // The same state reported after a reconnect is a first report again
      device.disconnect().await;
      report(0, 0);
      assert_eq!(first_reports(), [true]);
   }

   #[tokio::test]
   async fn ear_detection_follows_a_later_primary_change() {
      let device = AirPods::new(Address::default(), "test".into(), None, None);
      let events = Arc::new(RecordedEvents::default());
      let event_tx: EventSender = events.clone();
      let ear = |primary: u8, secondary: u8| {
         let mut packet = Packet::from_slice(HDR_EAR_DETECTION);
         packet.extend([primary, secondary]);
         device.process_packet(device.address(), packet, &event_tx);
      };
      let battery_with_primary = |primary: Component, secondary: Component| {
         let packet = battery_packet(&[
            (primary, 80, BatteryStatus::Discharging),
            (secondary, 80, BatteryStatus::Discharging),
         ]);
         device.process_packet(device.address(), packet, &event_tx);
      };
      let ear_events = || -> Vec<(Option<EarDetectionStatus>, EarDetectionStatus)> {
         events
            .0
            .lock()
            .drain(..)
            .filter_map(|event| match event {
               AirPodsEvent::EarDetectionChanged { status, previous } => Some((previous, status)),
               _ => None,
            })
            .collect()
      };

      // Primary out of ear, secondary in, before any battery report: left is assumed primary
      ear(0x01, 0x00);
      let status = device.ear_detection().unwrap();
      assert!(!status.is_left_in_ear() && status.is_right_in_ear());

      // The battery report shows the right earbud is primary: the sides are swapped
      battery_with_primary(Component::Right, Component::Left);
      let status = device.ear_detection().unwrap();
      assert!(status.is_left_in_ear() && !status.is_right_in_ear());

      // Reported once, with the same number of buds in an ear, so playback is left alone
      let reports = ear_events();
      assert_eq!(reports.len(), 2);
      let (previous, status) = reports[1];
      assert_eq!(previous.unwrap().in_ear_count(), status.in_ear_count());
      assert_eq!(media_control::playback_action(previous, status), None);

      // An unchanged primary maps nothing again
      battery_with_primary(Component::Right, Component::Left);
      assert!(ear_events().is_empty());
   }

   #[tokio::test]
   async fn disconnect_does_not_wait_for_the_battery_study() {
      let (study, _dir) = BatteryStudy::open_temp();
      study
         .get_or_create_study(Address::default(), "test".into())
         .unwrap();
      let device = AirPods::new(Address::default(), "test".into(), None, Some(study.clone()));
      device.0.battery_tracker.lock().record_test_drops(6);
      set_connection(&device, 1).await;

      // Another writer holds the database, as a slow disk would
      let writer = study.hold_write_lock(Duration::from_millis(500));
      let started = std::time::Instant::now();
      assert!(device.disconnect().await);
      let waited = started.elapsed();
      assert!(
         waited < Duration::from_millis(250),
         "disconnect waited {waited:?}"
      );
      writer.join().unwrap();

      // The session is still saved, once the database is free
      device.flush_battery_writes().await;
      assert!(
         study
            .get_drain_rate(Address::default(), NoiseControlMode::Off)
            .unwrap()
            .is_some()
      );
   }

   #[tokio::test]
   async fn late_packet_processor_cannot_close_newer_connection() {
      let device = AirPods::new(Address::default(), "test".into(), None, None);
      let events = Arc::new(RecordedEvents::default());
      let event_tx: EventSender = events.clone();

      // Connection 2 replaced connection 1, whose packet processor ends afterwards
      set_connection(&device, 2).await;
      device.notify_disconnected(1, &event_tx).await;
      assert!(device.is_connected());
      assert!(device.0.conn.read().await.is_some());
      assert!(events.0.lock().is_empty());

      // The current connection ends on its own
      device.notify_disconnected(2, &event_tx).await;
      assert!(!device.is_connected());
      assert!(device.0.conn.read().await.is_none());
      assert!(matches!(
         events.0.lock().as_slice(),
         [AirPodsEvent::DeviceDisconnected]
      ));
   }

   #[tokio::test]
   async fn packet_processor_stays_silent_after_disconnect() {
      let device = AirPods::new(Address::default(), "test".into(), None, None);
      let events = Arc::new(RecordedEvents::default());
      let event_tx: EventSender = events.clone();

      set_connection(&device, 1).await;
      assert!(device.disconnect().await);
      assert!(!device.is_connected());

      // Nothing left to close
      assert!(!device.disconnect().await);

      // The caller of disconnect() reports it; the processor must not report it again
      device.notify_disconnected(1, &event_tx).await;
      assert!(events.0.lock().is_empty());
   }
}

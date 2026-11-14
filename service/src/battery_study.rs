//! Persistent battery study database using heed (LMDB).
//!
//! This module provides storage and analysis of battery drain patterns
//! per `AirPods` device for immediate battery estimates upon connection
//! and continuous accuracy improvement.

use std::{
   borrow::{Borrow, Cow},
   path::{Path, PathBuf},
   sync::{Arc, LazyLock},
   time::{Duration, Instant, SystemTime},
};

use bluer::Address;
use heed::{Database, Env, EnvOpenOptions, types::SerdeBincode};
use log::{debug, info, warn};
use parking_lot::Mutex;
use serde::{Deserialize, Serialize};
use smol_str::SmolStr;
use strum::IntoEnumIterator;
use thiserror::Error;

use crate::{
   airpods::protocol::{BatteryInfo, BatteryState, NoiseControlMap, NoiseControlMode},
   error::Result,
   ringbuf::Ring,
};

/// Errors that can occur in battery study operations.
#[derive(Error, Debug)]
pub enum Error {
   #[error("Failed to create battery study directory: {0}")]
   CreateDirectory(#[from] std::io::Error),

   #[error("Failed to open heed environment: {0}")]
   OpenEnvironment(heed::Error),

   #[error("Database transaction error: {0}")]
   Transaction(heed::Error),

   #[error("Database operation error: {0}")]
   DatabaseOperation(heed::Error),

   #[error("Could not find local data directory")]
   DataDirectoryNotFound,

   #[error("Device study not found")]
   StudyNotFound,
}

/// Ring buffer for tracking battery history.
const BATTERY_HISTORY_SIZE: usize = 32;
/// Minimum number of not yet saved samples to save a battery study
const MIN_SAMPLES_TO_SAVE: usize = 3;
/// Upper bound on the weight of stored drain statistics, in samples (1 sample = 1% drop).
///
/// Keeps new sessions relevant as the battery ages, and bounds the sample counts that
/// earlier versions inflated by saving the same samples on every tick.
const MAX_STUDY_SAMPLES: u32 = 200;
/// Rise in level, in points, that can only mean the battery was charged.
///
/// Smaller rises are treated as measurement wobble and ignored.
const RECHARGE_MIN_RISE: u8 = 3;
/// Highest believable drain rate, in percent per hour (a full charge lasting 2 hours).
///
/// Faster rates come from history that spans a gap the clock did not see, such as a
/// suspend, and are ignored rather than used or stored.
const MAX_PLAUSIBLE_DRAIN_RATE: f64 = 50.0;

static BASE_TIME: LazyLock<Instant> = LazyLock::new(Instant::now);

#[derive(Debug, Clone, Copy, Default)]
struct SecondsSinceInit(u32);

impl From<Instant> for SecondsSinceInit {
   fn from(instant: Instant) -> Self {
      Self::new(instant)
   }
}

impl From<SecondsSinceInit> for Instant {
   fn from(seconds: SecondsSinceInit) -> Self {
      seconds.instant()
   }
}

impl SecondsSinceInit {
   fn new(t: Instant) -> Self {
      Self(
         t.checked_duration_since(*BASE_TIME)
            .map_or(0, |d| d.as_secs() as u32),
      )
   }
   const fn seconds_since(self, rhs: Self) -> u32 {
      self.0.saturating_sub(rhs.0)
   }
   fn instant(self) -> Instant {
      *BASE_TIME + Duration::from_secs(u64::from(self.0))
   }
}

#[derive(Default, Debug, Clone, Copy)]
struct BatteryHistory {
   samples: Ring<(SecondsSinceInit, u8), BATTERY_HISTORY_SIZE>, // (seconds since init, level)
   /// Number of newest samples that were not saved to the study database yet
   unsaved: usize,
}

impl BatteryHistory {
   fn push(&mut self, timestamp: Instant, level: u8) {
      self.samples.push((timestamp.into(), level));
      self.unsaved = (self.unsaved + 1).min(BATTERY_HISTORY_SIZE);
   }

   fn iter(&self) -> impl ExactSizeIterator<Item = (SecondsSinceInit, u8)> + Clone + '_ {
      self.samples.iter().map(|&(t, l)| (t, l))
   }

   const fn len(&self) -> usize {
      self.samples.len()
   }

   const fn is_empty(&self) -> bool {
      self.samples.is_empty()
   }

   const fn clear(&mut self) {
      self.samples.clear();
      self.unsaved = 0;
   }

   fn last_level(&self) -> Option<u8> {
      if self.is_empty() {
         None
      } else {
         self.samples.last().map(|&(_, l)| l)
      }
   }

   fn oldest_timestamp(&self) -> Option<Instant> {
      self.iter().next().map(|(t, _)| t.into())
   }

   fn record_battery_drop(&mut self, level: u8, timestamp: Instant) {
      // Not charging, record battery level
      match self.last_level() {
         Some(last_level) if level >= last_level.saturating_add(RECHARGE_MIN_RISE) => {
            // Charged without a charging report reaching us, typically while disconnected.
            // The old samples belong to an earlier discharge, so start over from here.
            debug!("Battery rose from {last_level} to {level}, restarting battery history");
            self.clear();
         },
         Some(last_level) if level >= last_level => return,
         Some(last_level) => {
            let elapsed = timestamp.duration_since(*BASE_TIME).as_secs_f64();
            debug!(
               "Battery dropped from {last_level} to {level} (sample #{}, elapsed: {:.1}s)",
               self.len() + 1,
               elapsed
            );
         },
         None => debug!("Recording initial battery level: {level} (first sample)"),
      }
      self.push(timestamp, level);
   }

   /// Calculates battery drain rate from the samples. Returns `(rate, alpha)`
   /// where rate is percent per hour and alpha is for exponential moving average smoothing.
   fn calculate_drain_rate(
      &self,
      min_samples: usize,
      max_age: Option<Instant>,
   ) -> Option<(f64, f64)> {
      if self.len() < min_samples {
         return None;
      }

      let samples: heapless::Vec<_, BATTERY_HISTORY_SIZE> = self
         .iter()
         .filter(|(timestamp, _)| max_age.is_none_or(|s| timestamp.instant() >= s))
         .collect();
      if samples.len() < min_samples {
         None
      } else {
         let rate = calculate_slope(&samples)?;
         if rate > MAX_PLAUSIBLE_DRAIN_RATE {
            debug!("Ignoring implausible drain rate of {rate:.1}%/hr");
            return None;
         }
         let alpha = if samples.len() >= 10 { 0.3 } else { 0.1 };
         Some((rate, alpha))
      }
   }
}

struct KeyCodec;

impl<'a> heed::BytesEncode<'a> for KeyCodec {
   type EItem = Address;
   fn bytes_encode(item: &'a Self::EItem) -> Result<Cow<'a, [u8]>, heed::BoxedError> {
      Ok(Cow::Borrowed(&item.0))
   }
}

impl<'a> heed::BytesDecode<'a> for KeyCodec {
   type DItem = Address;
   fn bytes_decode(bytes: &'a [u8]) -> Result<Self::DItem, heed::BoxedError> {
      let Ok(s) = bytes.try_into() else {
         return Err(heed::BoxedError::from(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "Invalid address",
         )));
      };
      Ok(Address(s))
   }
}

fn unix_now() -> u64 {
   SystemTime::UNIX_EPOCH.elapsed().unwrap().as_secs()
}

/// Database layout for battery study data
#[derive(Debug)]
struct Db {
   env: Env,
   /// MAC address -> `DeviceStudy`
   devices: Database<KeyCodec, SerdeBincode<DeviceStudy>>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DeviceStudy {
   pub device_name: SmolStr,
   pub last_updated: u64, // Unix timestamp
   pub total_sessions: u32,
   pub total_samples: u32,
   /// Noise mode -> drain statistics
   pub drain_rates: NoiseControlMap<DrainRateStats>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DrainRateStats {
   pub rate: f64,         // Percent per hour
   pub variance: f64,     // Statistical variance for confidence
   pub samples: u32,      // Total samples
   pub last_updated: u64, // Unix timestamp
}

/// Thread-safe wrapper for battery study database
#[derive(Clone, Debug)]
pub struct BatteryStudy {
   db: Arc<Db>,
}

impl BatteryStudy {
   /// Open or create the battery study database
   pub fn open() -> Result<Self> {
      Self::open_at(&Self::db_path()?)
   }

   /// Open or create the battery study database at `path`
   fn open_at(path: &Path) -> Result<Self> {
      std::fs::create_dir_all(path)?;

      let env = unsafe {
         EnvOpenOptions::new()
            .map_size(10 * 1024 * 1024) // 10MB should be plenty
            .max_dbs(1)
            .open(path)
            .map_err(Error::OpenEnvironment)?
      };

      let mut wtxn = env.write_txn().map_err(Error::Transaction)?;

      let devices = env
         .create_database(&mut wtxn, Some("devices"))
         .map_err(Error::DatabaseOperation)?;

      wtxn.commit().map_err(Error::Transaction)?;

      Ok(Self {
         db: Arc::new(Db { env, devices }),
      })
   }

   fn db_path() -> Result<PathBuf> {
      // Check for override environment variable first
      if let Ok(path) = std::env::var("AIRPODS_BATTERY_DB_PATH") {
         return Ok(PathBuf::from(path));
      }

      // ~/.local/share/kairpods/battery_study.db
      let base = dirs::data_local_dir().ok_or(Error::DataDirectoryNotFound)?;
      Ok(base.join("kairpods").join("battery_study.db"))
   }

   /// Get or create a battery study for a device
   pub fn get_or_create_study(
      &self,
      address: Address,
      device_name: SmolStr,
   ) -> Result<DeviceStudy> {
      let rtxn = self.db.env.read_txn().map_err(Error::Transaction)?;

      if let Some(study) = self
         .db
         .devices
         .get(&rtxn, &address)
         .map_err(Error::DatabaseOperation)?
      {
         Ok(study)
      } else {
         let study = DeviceStudy {
            device_name,
            last_updated: unix_now(),
            total_sessions: 0,
            total_samples: 0,
            drain_rates: NoiseControlMap::default(),
         };

         // Create in a write transaction
         drop(rtxn);
         let mut wtxn = self.db.env.write_txn().map_err(Error::Transaction)?;

         self
            .db
            .devices
            .put(&mut wtxn, &address, &study)
            .map_err(Error::DatabaseOperation)?;

         wtxn.commit().map_err(Error::Transaction)?;

         Ok(study)
      }
   }

   /// Update drain rate statistics using Welford's online algorithm
   pub fn update_drain_rate(
      &self,
      address: Address,
      mode: NoiseControlMode,
      new_rate: f64,
      samples: u32,
   ) -> Result<()> {
      let mut wtxn = self.db.env.write_txn().map_err(Error::Transaction)?;

      let mut study = self
         .db
         .devices
         .get(&wtxn, &address)
         .map_err(Error::DatabaseOperation)?
         .ok_or(Error::StudyNotFound)?;

      let stats = study
         .drain_rates
         .get_or_insert_with(mode, || DrainRateStats {
            rate: new_rate,
            variance: 0.0,
            samples: 0,
            last_updated: 0,
         });

      // Cap the weight of the stored history, so new data keeps moving the estimate
      stats.samples = stats.samples.min(MAX_STUDY_SAMPLES);

      // Update with Welford's online algorithm for mean and variance
      let k = f64::from(samples);
      let n = f64::from(stats.samples);
      let delta = new_rate - stats.rate;
      stats.rate += delta * k / (n + k);

      if stats.samples > 0 {
         let delta2 = new_rate - stats.rate;
         stats.variance = stats.variance.mul_add(n, delta * delta2 * k) / (n + k);
      }

      stats.samples += samples;
      stats.last_updated = unix_now();

      study.total_samples += samples;
      study.last_updated = unix_now();

      self
         .db
         .devices
         .put(&mut wtxn, &address, &study)
         .map_err(Error::DatabaseOperation)?;

      wtxn.commit().map_err(Error::Transaction)?;

      Ok(())
   }

   /// Get drain rate with confidence interval
   pub fn get_drain_rate(
      &self,
      address: Address,
      mode: NoiseControlMode,
   ) -> Result<Option<(f64, f64)>> {
      let rtxn = self.db.env.read_txn().map_err(Error::Transaction)?;

      let Some(study) = self
         .db
         .devices
         .get(&rtxn, &address)
         .map_err(Error::DatabaseOperation)?
      else {
         return Ok(None);
      };

      if let Some(stats) = study.drain_rates.get(mode) {
         // Stored by earlier versions from history that spanned a suspend
         if stats.rate > MAX_PLAUSIBLE_DRAIN_RATE {
            return Ok(None);
         }

         // Calculate 95% confidence interval. A variance of exactly zero means the rate
         // comes from a single save: there is no spread to measure, so nothing is known
         // about how reliable it is.
         let samples = stats.samples.min(MAX_STUDY_SAMPLES);
         let confidence = if samples > 1 && stats.variance > 0.0 {
            1.96 * (stats.variance / f64::from(samples)).sqrt()
         } else {
            f64::INFINITY
         };

         Ok(Some((stats.rate, confidence)))
      } else {
         Ok(None)
      }
   }

   /// Increment session count for a device
   pub fn increment_session_count(&self, address: Address) -> Result<()> {
      let mut wtxn = self.db.env.write_txn().map_err(Error::Transaction)?;

      if let Some(mut study) = self
         .db
         .devices
         .get(&wtxn, &address)
         .map_err(Error::DatabaseOperation)?
      {
         study.total_sessions += 1;
         study.last_updated = unix_now();

         self
            .db
            .devices
            .put(&mut wtxn, &address, &study)
            .map_err(Error::DatabaseOperation)?;

         wtxn.commit().map_err(Error::Transaction)?;
      }

      Ok(())
   }
}

/// Battery tracker that manages real-time battery monitoring and integrates with long-term study.
#[derive(Debug, Default)]
pub struct BatteryTracker {
   left_history: BatteryHistory,
   right_history: BatteryHistory,
   last_ttl_estimate: Option<u32>,
   /// When drain data was last written to the study database
   last_save: Option<Instant>,
   study: Option<BatteryStudy>,
   // Cache for historical drain rates to reduce DB queries
   historical_cache: Mutex<NoiseControlMap<(f64, f64, Instant)>>, // (rate, confidence, last_updated)
}

impl BatteryTracker {
   /// Creates a new battery tracker with optional long-term study integration.
   pub fn new(study: Option<BatteryStudy>) -> Self {
      Self {
         study,
         ..Default::default()
      }
   }

   /// Initializes a new battery study session for a device.
   pub fn init_session(&self, address: Address, device_name: &SmolStr) {
      if let Some(study) = &self.study {
         debug!("Initializing battery study session for {address} ({device_name})");
         if let Err(e) = study.increment_session_count(address) {
            debug!("Failed to increment session count: {e}");
         }
         match study.get_or_create_study(address, device_name.clone()) {
            Ok(device_study) => {
               debug!(
                  "Battery study for {}: {} sessions, {} samples, {} modes tracked",
                  address,
                  device_study.total_sessions,
                  device_study.total_samples,
                  device_study.drain_rates.len()
               );
            },
            Err(e) => {
               debug!("Failed to get/create battery study: {e}");
            },
         }
      } else {
         debug!("No battery study available for session initialization");
      }
   }

   /// Records battery levels for both buds, tracking drops for drain rate calculation.
   pub fn record_battery_drop(&mut self, l: BatteryState, r: BatteryState) {
      let now = Instant::now();

      [
         ("left", l, &mut self.left_history),
         ("right", r, &mut self.right_history),
      ]
      .into_iter()
      .filter(|(_, state, _)| state.is_available())
      .for_each(|(name, state, history)| {
         if state.is_charging() && history.last_level().is_some() {
            debug!("{name} bud started charging, clearing battery history");
            history.clear();
         } else if !state.is_charging() {
            history.record_battery_drop(state.level, now);
         }
      });
   }

   /// Estimates battery time-to-live, optionally trying multiple noise modes if none specified.
   pub fn estimate_ttl(
      &mut self,
      battery_info: &BatteryInfo,
      noise_mode: Option<NoiseControlMode>,
      address: Address,
   ) -> Option<u32> {
      let prev_estimate = self.last_ttl_estimate;

      // Don't estimate if either bud is charging
      let (left, right) = battery_info.split_ref();
      if left.is_charging() || right.is_charging() {
         if prev_estimate.is_some() {
            debug!("Battery TTL estimation unavailable: AirPods are charging");
            self.last_ttl_estimate = None;
         }
         return None;
      }

      // Don't estimate if either bud is disconnected
      if !left.is_available() || !right.is_available() {
         if prev_estimate.is_some() {
            debug!("Battery TTL estimation unavailable: One or both buds disconnected");
            self.last_ttl_estimate = None;
         }
         return None;
      }

      // Get local drain rate and sample count
      let (local_rate, local_sample_count) =
         if let Some((rate, alpha, count)) = self.calculate_local_drain_rate() {
            (Some((rate, alpha)), count)
         } else {
            (None, 0)
         };

      // Try to get historical rate, falling back through modes if needed
      let mut historical_rate = None;
      let mut used_mode = None;
      for mode in noise_mode.iter().copied().chain(NoiseControlMode::iter()) {
         if let Some(rate) = self.get_historical_rate_cached(address, mode) {
            historical_rate = Some(rate);
            used_mode = Some(mode);
            break;
         }
      }

      debug!(
         "Battery TTL calculation - local: {:?} (samples: {}), historical: {:?}, mode: {:?} (used: {:?})",
         local_rate.map(|(r, _)| r),
         local_sample_count,
         historical_rate.map(|(r, _)| r),
         noise_mode,
         used_mode
      );

      // Combine local and historical rates
      let (drain_rate, alpha) = if let Some((rate, alpha)) =
         Self::combine_drain_rates(local_rate, historical_rate, local_sample_count)
      {
         (rate, alpha)
      } else {
         if prev_estimate.is_some() {
            debug!("Battery TTL estimation unavailable: No drain rate available");
            self.last_ttl_estimate = None;
         }
         return None;
      };

      // Guard against zero or near-zero drain rate
      if drain_rate <= f64::EPSILON {
         if prev_estimate.is_some() {
            debug!("Battery TTL estimation unavailable: Drain rate is effectively zero");
            self.last_ttl_estimate = None;
         }
         return None;
      }

      // Use the minimum battery level for conservative estimate
      let min_level = f64::from(left.level.min(right.level));

      // Calculate hours remaining
      let hours_remaining = min_level / drain_rate;

      // Convert to minutes
      let new_minutes = (hours_remaining * 60.0) as u32;

      if new_minutes > 0 && new_minutes < 24 * 60 {
         // Apply hysteresis to avoid jumpy estimates
         let smoothed_minutes = if let Some(last_estimate) = prev_estimate {
            let smoothed =
               f64::from(new_minutes).mul_add(alpha, f64::from(last_estimate) * (1.0 - alpha));
            smoothed.round() as u32
         } else {
            info!("Battery TTL estimation now available: {new_minutes} minutes remaining");
            new_minutes
         };

         // Cache the smoothed estimate
         self.last_ttl_estimate = Some(smoothed_minutes);
         Some(smoothed_minutes)
      } else {
         if prev_estimate.is_some() {
            debug!(
               "Battery TTL estimation unavailable: Unreasonable estimate ({new_minutes} minutes)"
            );
            self.last_ttl_estimate = None;
         }
         None
      }
   }

   /// Calculates drain rate from local battery history.
   /// Returns (`drain_rate`, alpha, `sample_count`)
   fn calculate_local_drain_rate(&self) -> Option<(f64, f64, usize)> {
      const MIN_SAMPLES: usize = 4;
      const MAX_AGE_HOURS: f64 = 2.0;

      let now = Instant::now();
      let max_age = now
         .checked_sub(Duration::from_secs_f64(MAX_AGE_HOURS * 3600.0))
         .unwrap();

      // Try to get drain rate from left or right history
      if let Some((rate, alpha)) = self
         .left_history
         .calculate_drain_rate(MIN_SAMPLES, Some(max_age))
      {
         Some((rate, alpha, self.left_history.len()))
      } else if let Some((rate, alpha)) = self
         .right_history
         .calculate_drain_rate(MIN_SAMPLES, Some(max_age))
      {
         Some((rate, alpha, self.right_history.len()))
      } else {
         None
      }
   }

   /// Gets historical drain rate with caching to reduce DB queries.
   fn get_historical_rate_cached(
      &self,
      address: Address,
      mode: NoiseControlMode,
   ) -> Option<(f64, f64)> {
      const CACHE_DURATION: Duration = Duration::from_secs(300); // 5 minutes

      // Check cache first
      {
         let cache = self.historical_cache.lock();
         if let Some(&(rate, confidence, last_updated)) = cache.get(mode)
            && last_updated.elapsed() < CACHE_DURATION
         {
            return Some((rate, confidence));
         }
      }

      // Cache miss or expired, query from study
      if let Some(ref study) = self.study {
         match study.get_drain_rate(address, mode) {
            Ok(Some((rate, confidence))) => {
               debug!(
                  "Found historical drain rate for {address} mode {mode}: {rate:.1}%/hr (confidence: ±{confidence:.1})"
               );
               // Update cache
               self
                  .historical_cache
                  .lock()
                  .insert(mode, (rate, confidence, Instant::now()));
               return Some((rate, confidence));
            },
            Ok(None) => {
               debug!("No historical drain rate found for {address} mode {mode}");
            },
            Err(e) => {
               debug!("Error getting historical drain rate: {e}");
            },
         }
      } else {
         debug!("No battery study available");
      }

      None
   }

   /// Combines local and historical drain rates using weighted average based on confidence.
   fn combine_drain_rates(
      local_rate: Option<(f64, f64)>, // (rate, alpha from local calculation)
      historical_rate: Option<(f64, f64)>, // (rate, confidence)
      local_sample_count: usize,
   ) -> Option<(f64, f64)> {
      match (local_rate, historical_rate) {
         // Both available - weighted combination
         (Some((local_r, _)), Some((hist_r, hist_conf))) => {
            // Calculate weight based on local sample count and historical confidence
            let local_weight = if local_sample_count < 4 {
               0.0f64 // Not enough local samples, use 100% historical
            } else if local_sample_count <= 10 {
               0.7f64 // 4-10 samples: 70% local, 30% historical
            } else {
               0.9f64 // >10 samples: 90% local, 10% historical
            };

            // Adjust weight based on historical confidence
            // High confidence (low value) in historical data = use more historical
            let adjusted_weight = if hist_conf < 1.0 {
               local_weight * 0.8 // Very high confidence in historical
            } else if hist_conf < 2.0 {
               local_weight // Good confidence
            } else {
               (1.0 - local_weight).mul_add(0.5, local_weight) // Low confidence, use more local
            };

            let combined_rate = local_r.mul_add(adjusted_weight, hist_r * (1.0 - adjusted_weight));
            let alpha = adjusted_weight.mul_add(0.2, 0.3); // Higher alpha for more smoothing when using more local data

            debug!(
               "Combined drain rate: {:.1}%/hr (local: {:.1}%/hr * {:.0}%, historical: {:.1}%/hr * {:.0}%)",
               combined_rate,
               local_r,
               adjusted_weight * 100.0,
               hist_r,
               (1.0 - adjusted_weight) * 100.0
            );

            Some((combined_rate, alpha))
         },
         // Only local available
         (Some(local), None) => {
            debug!("Using local drain rate only (no historical data)");
            Some(local)
         },
         // Only historical available
         (None, Some((hist_r, hist_conf))) => {
            // Always use historical data, even with low confidence
            // Low confidence is better than no estimate
            if hist_conf < 5.0 {
               debug!("Using historical drain rate: {hist_r:.1}%/hr (confidence: ±{hist_conf:.1})");
               Some((hist_r, 0.5)) // Higher alpha for pure historical
            } else {
               debug!(
                  "Using historical drain rate with low confidence: {hist_r:.1}%/hr (±{hist_conf:.1})"
               );
               Some((hist_r, 0.7)) // Even higher alpha for low confidence data
            }
         },
         // Neither available
         (None, None) => None,
      }
   }

   /// Number of samples recorded since the last save, for the bud with the most new data.
   fn unsaved_samples(&self) -> usize {
      self.left_history.unsaved.max(self.right_history.unsaved)
   }

   /// Checks if enough time has passed and new samples collected to warrant a periodic save.
   pub fn should_save(&self, interval_minutes: u32, battery_info: &BatteryInfo) -> bool {
      // Check if we have enough samples that were not saved yet
      let sample_count = self.unsaved_samples();
      if sample_count < MIN_SAMPLES_TO_SAVE {
         debug!(
            "should_save: Not enough new samples yet (have {sample_count}, need {MIN_SAMPLES_TO_SAVE})"
         );
         return false; // Not enough new samples yet
      }

      // Check if neither bud is charging
      let (left, right) = battery_info.split_ref();
      if left.is_charging() || right.is_charging() {
         debug!(
            "should_save: AirPods are charging (left: {}, right: {})",
            left.is_charging(),
            right.is_charging()
         );
         return false; // Don't save while charging
      }

      // Check time since the last save, or since the oldest sample if that is more recent
      // (nothing saved yet, or history restarted after charging)
      let oldest_time = match (
         self.left_history.oldest_timestamp(),
         self.right_history.oldest_timestamp(),
      ) {
         (Some(l), Some(r)) => l.min(r),
         (Some(t), None) | (None, Some(t)) => t,
         (None, None) => {
            debug!("should_save: No timestamp samples available");
            return false; // No samples
         },
      };

      let since = self
         .last_save
         .map_or(oldest_time, |saved| saved.max(oldest_time));
      let elapsed = Instant::now().duration_since(since);
      let required_duration = Duration::from_secs(u64::from(interval_minutes * 60));
      let should_save = elapsed >= required_duration;

      debug!(
         "should_save: Elapsed: {:.1}s, Required: {:.1}s, Will save: {}",
         elapsed.as_secs_f64(),
         required_duration.as_secs_f64(),
         should_save
      );

      should_save
   }

   /// Saves aggregated battery drain data to the study database.
   ///
   /// Each sample is counted once: the drain rate is weighted by the samples recorded
   /// since the last save, and nothing is written when there are none.
   pub fn save_to_study(&mut self, address: Address, noise_mode: NoiseControlMode) {
      let new_samples = self.unsaved_samples();
      if new_samples > 0
         && let Some(study) = &self.study
         // Calculate drain rate from current session
         && let Some((drain_rate, _alpha, sample_count)) = self.calculate_local_drain_rate()
         && sample_count >= 4
      {
         self.last_save = Some(Instant::now());

         // Update drain rate statistics in the database
         match study.update_drain_rate(address, noise_mode, drain_rate, new_samples as u32) {
            Ok(()) => {
               debug!(
                  "Saved battery drain rate of {drain_rate:.1}%/hr for mode {noise_mode} with {new_samples} new samples"
               );

               // Clear cache for this mode to force refresh
               self.historical_cache.lock().remove(noise_mode);

               self.left_history.unsaved = 0;
               self.right_history.unsaved = 0;
            },
            Err(e) => warn!("Failed to save battery drain rate for {address}: {e}"),
         }
      }
   }

   /// Saves the drain data of a connection that ended and starts the next one afresh.
   ///
   /// Samples from different connections must not be combined: the time in between,
   /// such as a suspend, is not visible to the monotonic clock, so the levels lost in
   /// that time would look like a sudden drain.
   pub fn end_session(&mut self, address: Address, noise_mode: NoiseControlMode) {
      self.save_to_study(address, noise_mode);
      self.left_history.clear();
      self.right_history.clear();
      self.last_ttl_estimate = None;
      self.last_save = None;
   }
}

// Helper function to calculate linear regression slope
fn calculate_slope<I>(samples: I) -> Option<f64>
where
   I: IntoIterator<Item: Borrow<(SecondsSinceInit, u8)>>,
   I::IntoIter: ExactSizeIterator,
{
   let samples = samples.into_iter();
   let len = samples.len();
   if len < 2 {
      return None;
   }

   let n = len as f64;
   let mut sum_x = 0.0;
   let mut sum_y = 0.0;
   let mut sum_xy = 0.0;
   let mut sum_xx = 0.0;
   let mut base_time = None;

   for v in samples {
      let (timestamp, level) = v.borrow();

      let since = if let Some(base_time) = base_time {
         f64::from(timestamp.seconds_since(base_time)) / 3600.0
      } else {
         base_time = Some(*timestamp);
         0.0
      };

      let x = since;
      let y = f64::from(*level);

      sum_x += x;
      sum_y += y;
      sum_xy += x * y;
      sum_xx += x * x;
   }

   let denominator = n.mul_add(sum_xx, -(sum_x * sum_x));
   if denominator.abs() < f64::EPSILON {
      return None;
   }

   // Slope represents battery change per hour (negative for drain)
   let slope = n.mul_add(sum_xy, -(sum_x * sum_y)) / denominator;

   // Convert to positive drain rate
   if slope < 0.0 {
      Some(-slope)
   } else {
      None // Battery not draining
   }
}

#[cfg(test)]
mod tests {
   use crate::airpods::protocol::{BatteryState, BatteryStatus};

   use super::*;

   use tempfile::TempDir;

   const TEST_ADDRESS: Address = Address([0xAA, 0xBB, 0xCC, 0xDD, 0xEE, 0xFF]);

   fn create_test_db() -> Result<(BatteryStudy, TempDir)> {
      let temp_dir = TempDir::new().unwrap();
      let manager = BatteryStudy::open_at(temp_dir.path())?;
      Ok((manager, temp_dir))
   }

   fn mock_state(level: u8, is_charging: bool) -> BatteryState {
      if is_charging {
         BatteryState {
            level,
            status: BatteryStatus::Charging,
         }
      } else {
         BatteryState {
            level,
            status: BatteryStatus::Normal,
         }
      }
   }

   #[test]
   fn test_create_and_get_study() -> Result<()> {
      let (manager, _dir) = create_test_db()?;

      let study = manager.get_or_create_study(TEST_ADDRESS, SmolStr::new_static("Test AirPods"))?;
      assert_eq!(study.device_name, "Test AirPods");
      assert_eq!(study.total_sessions, 0);
      assert_eq!(study.total_samples, 0);

      Ok(())
   }

   #[test]
   fn test_update_drain_rate() -> Result<()> {
      let (manager, _dir) = create_test_db()?;

      manager.get_or_create_study(TEST_ADDRESS, SmolStr::new_static("Test AirPods"))?;
      manager.update_drain_rate(TEST_ADDRESS, NoiseControlMode::Active, 12.5, 10)?;

      let (rate, confidence) = manager
         .get_drain_rate(TEST_ADDRESS, NoiseControlMode::Active)?
         .unwrap();
      assert!((rate - 12.5).abs() < 0.001);
      // A single save says nothing about the spread
      assert_eq!(confidence, f64::INFINITY);

      // Add another sample
      manager.update_drain_rate(TEST_ADDRESS, NoiseControlMode::Active, 11.5, 10)?;
      let (rate, confidence) = manager
         .get_drain_rate(TEST_ADDRESS, NoiseControlMode::Active)?
         .unwrap();
      assert!((rate - 12.0).abs() < 0.001); // Average of 12.5 and 11.5
      assert!(confidence < f64::INFINITY); // Now we have variance

      Ok(())
   }

   #[test]
   fn test_battery_history_ring_buffer() {
      let mut history = BatteryHistory::default();
      let base_time = Instant::now();

      // Test initial state
      assert_eq!(history.len(), 0);
      assert!(history.last_level().is_none());

      // Add some samples
      for i in 0..5 {
         history.push(base_time + Duration::from_secs(i * 60), 100 - i as u8);
      }

      assert_eq!(history.len(), 5);
      assert_eq!(history.last_level(), Some(96));

      // Test iterator
      let samples: Vec<_> = history.iter().collect();
      assert_eq!(samples.len(), 5);
      assert_eq!(samples[0].1, 100);
      assert_eq!(samples[4].1, 96);
   }

   #[test]
   fn test_battery_history_wraparound() {
      let mut history = BatteryHistory::default();
      let base_time = Instant::now();

      // Fill beyond capacity
      for i in 0..80 {
         history.push(base_time + Duration::from_secs(i * 60), 100 - i as u8);
      }

      assert_eq!(history.len(), BATTERY_HISTORY_SIZE);

      // Check that we have the most recent samples
      let samples: Vec<_> = history.iter().collect();
      assert_eq!(samples.len(), BATTERY_HISTORY_SIZE);

      // The oldest sample should be from index 48 (80 - 32)
      assert_eq!(samples[0].1, 52);
   }

   #[test]
   fn test_battery_tracker_ttl_when_charging() {
      let mut tracker = BatteryTracker::new(None);

      // Set battery with one bud charging
      let battery = BatteryInfo {
         left: BatteryState {
            level: 50,
            status: BatteryStatus::Charging,
         },
         right: BatteryState {
            level: 60,
            status: BatteryStatus::Normal,
         },
         case: BatteryState {
            level: 80,
            status: BatteryStatus::Normal,
         },
         headphone: BatteryState::new(),
      };

      // Should return None when charging
      assert!(
         tracker
            .estimate_ttl(&battery, Some(NoiseControlMode::Off), TEST_ADDRESS)
            .is_none()
      );
   }

   #[test]
   fn test_battery_tracker_clears_on_charging() {
      let mut tracker = BatteryTracker::new(None);

      // Add some battery history
      for i in 0..5 {
         let level = (100 - i * 2) as u8;
         tracker.record_battery_drop(mock_state(level, false), mock_state(level, false));
      }

      assert!(!tracker.left_history.is_empty());
      assert!(!tracker.right_history.is_empty());

      // Now simulate charging
      tracker.record_battery_drop(mock_state(90, true), mock_state(90, false));

      // Left history should be cleared, right should remain
      assert_eq!(tracker.left_history.len(), 0);
      assert!(!tracker.right_history.is_empty());
   }

   #[test]
   fn test_recharge_without_charging_report_restarts_history() {
      let mut tracker = BatteryTracker::new(None);
      let record = |tracker: &mut BatteryTracker, left: u8, right: u8| {
         tracker.record_battery_drop(mock_state(left, false), mock_state(right, false));
      };

      for level in [80, 79, 78, 77, 76] {
         record(&mut tracker, level, level);
      }
      assert_eq!(tracker.left_history.len(), 5);

      // A rise of a point or two is wobble, not a recharge
      record(&mut tracker, 78, 77);
      assert_eq!(tracker.left_history.len(), 5);
      assert_eq!(tracker.left_history.last_level(), Some(76));
      assert_eq!(tracker.right_history.len(), 5);

      // Left bud was charged while disconnected: no charging report was ever seen
      record(&mut tracker, 100, 76);
      assert_eq!(tracker.left_history.len(), 1);
      assert_eq!(tracker.left_history.last_level(), Some(100));
      assert_eq!(tracker.right_history.len(), 5);

      // Drain is recorded again right away, not only below the old level
      record(&mut tracker, 99, 76);
      assert_eq!(tracker.left_history.len(), 2);
      assert_eq!(tracker.left_history.last_level(), Some(99));
   }

   #[test]
   fn test_battery_tracker_insufficient_data() {
      let mut tracker = BatteryTracker::new(None);

      // Add only 3 samples (less than MIN_SAMPLES of 4)
      for i in 0..3 {
         let level = (100 - i) as u8;
         tracker.record_battery_drop(mock_state(level, false), mock_state(level, false));
      }

      let battery = BatteryInfo {
         left: BatteryState {
            level: 97,
            status: BatteryStatus::Normal,
         },
         right: BatteryState {
            level: 97,
            status: BatteryStatus::Normal,
         },
         case: BatteryState {
            level: 80,
            status: BatteryStatus::Normal,
         },
         headphone: BatteryState::new(),
      };

      // Should return None with insufficient data
      assert!(
         tracker
            .estimate_ttl(&battery, Some(NoiseControlMode::Off), TEST_ADDRESS)
            .is_none()
      );
   }

   #[test]
   fn test_battery_tracker_integration_with_study() {
      let (study, _dir) = create_test_db().unwrap();
      let mut tracker = BatteryTracker::new(Some(study));

      // Simulate some battery history
      for i in 0..10 {
         let level = (100 - i * 2) as u8;
         tracker.record_battery_drop(mock_state(level, false), mock_state(level, false));
      }

      // Save to study
      tracker.save_to_study(TEST_ADDRESS, NoiseControlMode::Active);

      // Verify it was saved by checking the cache was cleared
      assert!(tracker.historical_cache.lock().is_empty());
   }

   #[test]
   fn test_should_save() {
      let mut tracker = BatteryTracker::new(None);

      let battery = BatteryInfo {
         left: BatteryState {
            level: 80,
            status: BatteryStatus::Normal,
         },
         right: BatteryState {
            level: 80,
            status: BatteryStatus::Normal,
         },
         case: BatteryState {
            level: 80,
            status: BatteryStatus::Normal,
         },
         headphone: BatteryState::new(),
      };

      // Should be false with no samples
      assert!(!tracker.should_save(30, &battery));

      // Add 10 samples
      for i in 0..10 {
         let level = (100 - i) as u8;
         tracker.record_battery_drop(mock_state(level, false), mock_state(level, false));
      }

      // Note: In real usage, time would have passed between samples
      // The test will likely still return false because not enough time has elapsed
      // This is expected behavior
   }

   #[test]
   fn test_save_counts_each_sample_once() -> Result<()> {
      let (study, _dir) = create_test_db()?;
      study.get_or_create_study(TEST_ADDRESS, SmolStr::new_static("Test AirPods"))?;
      let mut tracker = BatteryTracker::new(Some(study.clone()));

      let saved_samples = || -> Result<Option<u32>> {
         let device =
            study.get_or_create_study(TEST_ADDRESS, SmolStr::new_static("Test AirPods"))?;
         Ok(device
            .drain_rates
            .get(NoiseControlMode::Off)
            .map(|stats| stats.samples))
      };

      // One 1% drop every 3.5 minutes (~17%/hr); interval 0 stands in for elapsed time
      let base_time = Instant::now();
      let record = |tracker: &mut BatteryTracker, i: u64| {
         let timestamp = base_time + Duration::from_secs(i * 210);
         tracker.left_history.push(timestamp, 100 - i as u8);
         tracker.right_history.push(timestamp, 100 - i as u8);
      };
      let battery = BatteryInfo {
         left: mock_state(90, false),
         right: mock_state(90, false),
         case: BatteryState::new(),
         headphone: BatteryState::new(),
      };

      for i in 0..6 {
         record(&mut tracker, i);
      }
      assert!(tracker.should_save(0, &battery));
      tracker.save_to_study(TEST_ADDRESS, NoiseControlMode::Off);
      assert_eq!(saved_samples()?, Some(6));

      // Later ticks without new samples must not save the retained samples again
      assert!(!tracker.should_save(0, &battery));
      tracker.save_to_study(TEST_ADDRESS, NoiseControlMode::Off);
      assert_eq!(saved_samples()?, Some(6));

      // Only the new samples are added
      for i in 6..9 {
         record(&mut tracker, i);
      }
      assert!(tracker.should_save(0, &battery));
      tracker.save_to_study(TEST_ADDRESS, NoiseControlMode::Off);
      assert_eq!(saved_samples()?, Some(9));

      Ok(())
   }

   /// Records one 1% drop every 3.5 minutes (~17.1 %/hr) on both buds, from 100%.
   fn record_drops(tracker: &mut BatteryTracker, base_time: Instant, range: std::ops::Range<u64>) {
      for i in range {
         let timestamp = base_time + Duration::from_secs(i * 210);
         tracker.left_history.push(timestamp, 100 - i as u8);
         tracker.right_history.push(timestamp, 100 - i as u8);
      }
   }

   #[test]
   fn test_save_keeps_history_for_the_drain_rate() -> Result<()> {
      let (study, _dir) = create_test_db()?;
      study.get_or_create_study(TEST_ADDRESS, SmolStr::new_static("Test AirPods"))?;
      let mut tracker = BatteryTracker::new(Some(study));
      let base_time = Instant::now();

      record_drops(&mut tracker, base_time, 0..8);
      tracker.save_to_study(TEST_ADDRESS, NoiseControlMode::Off);
      assert_eq!(tracker.unsaved_samples(), 0);

      // Every sample stays for the local drain rate
      assert_eq!(tracker.left_history.len(), 8);
      assert_eq!(tracker.right_history.len(), 8);

      // Samples before and after the save form one history: the rate uses all 12 and
      // gets the weighting for 10 or more samples
      record_drops(&mut tracker, base_time, 8..12);
      let (rate, alpha, sample_count) = tracker.calculate_local_drain_rate().unwrap();
      assert_eq!(sample_count, 12);
      assert!((alpha - 0.3).abs() < f64::EPSILON);
      assert!((rate - 3600.0 / 210.0).abs() < 0.01, "rate {rate}");
      Ok(())
   }

   #[test]
   fn test_history_without_saves_is_bounded_by_the_ring() {
      let mut tracker = BatteryTracker::new(None);
      record_drops(&mut tracker, Instant::now(), 0..40);

      // The oldest samples are dropped, the newest kept
      assert_eq!(tracker.left_history.len(), BATTERY_HISTORY_SIZE);
      assert_eq!(tracker.left_history.last_level(), Some(61));
      assert_eq!(tracker.unsaved_samples(), BATTERY_HISTORY_SIZE);
   }

   #[test]
   fn test_stored_history_weight_is_capped() -> Result<()> {
      let (study, _dir) = create_test_db()?;
      study.get_or_create_study(TEST_ADDRESS, SmolStr::new_static("Test AirPods"))?;

      // Sample count inflated far beyond the cap, as written by earlier versions
      study.update_drain_rate(TEST_ADDRESS, NoiseControlMode::Active, 10.0, 1_000_000)?;
      let (_, confidence) = study
         .get_drain_rate(TEST_ADDRESS, NoiseControlMode::Active)?
         .unwrap();
      assert_eq!(confidence, f64::INFINITY); // Single update, no variance yet

      // New data with the weight of the cap moves the mean halfway
      study.update_drain_rate(
         TEST_ADDRESS,
         NoiseControlMode::Active,
         20.0,
         MAX_STUDY_SAMPLES,
      )?;
      let (rate, confidence) = study
         .get_drain_rate(TEST_ADDRESS, NoiseControlMode::Active)?
         .unwrap();
      assert!((rate - 15.0).abs() < 0.001);
      assert!(confidence > 0.1); // Not collapsed by the inflated count

      Ok(())
   }

   #[test]
   fn test_session_end_saves_and_forgets_history() -> Result<()> {
      let (study, _dir) = create_test_db()?;
      study.get_or_create_study(TEST_ADDRESS, SmolStr::new_static("Test AirPods"))?;
      let mut tracker = BatteryTracker::new(Some(study.clone()));

      // One 1% drop every 3.5 minutes (~17%/hr)
      let base_time = Instant::now();
      for i in 0..6 {
         let timestamp = base_time + Duration::from_secs(i * 210);
         tracker.left_history.push(timestamp, 80 - i as u8);
         tracker.right_history.push(timestamp, 80 - i as u8);
      }
      tracker.last_ttl_estimate = Some(200);

      tracker.end_session(TEST_ADDRESS, NoiseControlMode::Off);

      assert!(
         study
            .get_drain_rate(TEST_ADDRESS, NoiseControlMode::Off)?
            .is_some()
      );
      assert!(tracker.left_history.is_empty() && tracker.right_history.is_empty());
      assert!(tracker.last_ttl_estimate.is_none() && tracker.last_save.is_none());

      // After a suspend the next connection reports a much lower level: it starts a new
      // history instead of extending the old one into a sudden drop
      tracker.record_battery_drop(mock_state(60, false), mock_state(60, false));
      assert_eq!(tracker.left_history.len(), 1);
      Ok(())
   }

   #[test]
   fn test_implausible_drain_rate_is_ignored() -> Result<()> {
      // Steady drain, then a 20-point drop the clock saw as 60 seconds (a suspend)
      let mut history = BatteryHistory::default();
      let base_time = Instant::now();
      for i in 0..5 {
         history.push(base_time + Duration::from_secs(i * 210), 80 - i as u8);
      }
      assert!(history.calculate_drain_rate(4, None).is_some());
      history.push(base_time + Duration::from_secs(900), 60);
      assert!(history.calculate_drain_rate(4, None).is_none());

      // Inflated rates stored by earlier versions are not used either
      let (study, _dir) = create_test_db()?;
      study.get_or_create_study(TEST_ADDRESS, SmolStr::new_static("Test AirPods"))?;
      study.update_drain_rate(TEST_ADDRESS, NoiseControlMode::Off, 68.6, 10)?;
      assert!(
         study
            .get_drain_rate(TEST_ADDRESS, NoiseControlMode::Off)?
            .is_none()
      );
      Ok(())
   }

   #[test]
   fn test_single_save_does_not_outweigh_local_samples() {
      let local = Some((20.0, 0.3));
      let combine = |hist_conf: f64| {
         BatteryTracker::combine_drain_rates(local, Some((10.0, hist_conf)), 6)
            .unwrap()
            .0
      };
      // Unknown confidence leans on the live samples more than a measured, tight one
      assert!(combine(f64::INFINITY) > combine(0.5));
   }

   #[test]
   fn test_battery_tracker_records_headphone() {
      let mut tracker = BatteryTracker::new(None);

      // AirPods Max: only the headphone battery is available
      for i in 0..5 {
         let battery = BatteryInfo {
            left: BatteryState::new(),
            right: BatteryState::new(),
            case: BatteryState::new(),
            headphone: mock_state((100 - i) as u8, false),
         };
         let (l, r) = battery.split_ref();
         tracker.record_battery_drop(*l, *r);
      }

      // Headphone level feeds the drain history
      assert!(!tracker.left_history.is_empty());
      assert!(!tracker.right_history.is_empty());
   }
}

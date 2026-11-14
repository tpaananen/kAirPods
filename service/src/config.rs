//! Configuration management for the `AirPods` service.
//!
//! This module handles loading and saving configuration from disk,
//! including known devices and connection parameters.

use std::{env, fs, path::PathBuf};

use serde::{Deserialize, Serialize};
use smol_str::SmolStr;

use crate::error::{AirPodsError, Result};

/// Main configuration structure for the service.
#[derive(Serialize, Deserialize, Clone)]
pub struct Config {
   #[serde(default)]
   pub known_devices: Vec<KnownDevice>,

   #[serde(default = "default_export_bluez_battery")]
   pub export_bluez_battery: bool,

   #[serde(default)]
   pub log_filter: Option<SmolStr>,
}

/// Represents a known `AirPods` device.
#[derive(Serialize, Deserialize, Clone)]
pub struct KnownDevice {
   pub address: String,
   pub name: String,
}

const fn default_export_bluez_battery() -> bool {
   true
}

impl Default for Config {
   fn default() -> Self {
      Self {
         known_devices: vec![],
         export_bluez_battery: default_export_bluez_battery(),
         log_filter: None,
      }
   }
}

impl Config {
   /// Loads configuration from disk or creates default if not exists.
   pub fn load() -> Result<Self> {
      let config_path = Self::config_path()?;

      if config_path.exists() {
         let contents = fs::read_to_string(&config_path)?;
         Ok(toml::from_str(&contents)?)
      } else {
         // Create default config
         let config = Self::default();
         config.save()?;
         Ok(config)
      }
   }

   /// Saves the current configuration to disk.
   pub fn save(&self) -> Result<()> {
      let config_path = Self::config_path()?;

      // Ensure directory exists
      if let Some(parent) = config_path.parent() {
         fs::create_dir_all(parent)?;
      }

      let contents = toml::to_string_pretty(self)?;
      fs::write(&config_path, contents)?;

      Ok(())
   }

   fn config_path() -> Result<PathBuf> {
      // Check for override environment variable first
      if let Ok(path) = env::var("AIRPODS_CONFIG_PATH") {
         return Ok(PathBuf::from(path));
      }

      // Use dirs crate to get the proper config directory
      Ok(dirs::config_dir()
         .ok_or(AirPodsError::ConfigDirNotFound)?
         .join("kairpods")
         .join("config.toml"))
   }

   /// Checks if the given address is a known device and returns its name.
   pub fn is_known_device(&self, address: &str) -> Option<&str> {
      self
         .known_devices
         .iter()
         .find(|d| d.address == address)
         .map(|d| d.name.as_str())
   }
}

#[cfg(test)]
mod tests {
   use super::*;

   #[test]
   fn keys_removed_from_the_config_are_ignored_in_existing_files() {
      // Written by earlier versions into every generated config.toml
      let config: Config = toml::from_str(
         "known_devices = []\npoll_interval = 30\nconnection_retry_count = 10\n\
          reconnect_delay_sec = 10\nnotification_retries = 3\nexport_bluez_battery = false\n",
      )
      .unwrap();
      assert!(!config.export_bluez_battery);
      assert!(config.known_devices.is_empty());
   }
}

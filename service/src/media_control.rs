//! Media control module for sending play/pause commands via MPRIS.
//!
//! This module provides functionality to control media playback using the
//! MPRIS (Media Player Remote Interfacing Specification) D-Bus interface.

use std::{
   sync::atomic::{AtomicBool, Ordering},
   time::Duration,
};

use log::{debug, warn};
use parking_lot::Mutex;
use tokio::{sync::mpsc, time};
use zbus::Connection;

use crate::airpods::protocol::EarDetectionStatus;

/// Maximum time to wait for a reply from the bus or a media player.
///
/// zbus has no timeout of its own, so a frozen player would otherwise never answer.
const CALL_TIMEOUT: Duration = Duration::from_secs(2);

type BoxError = Box<dyn std::error::Error + Send + Sync>;

static ENABLED: AtomicBool = AtomicBool::new(false);

/// Tracks which players we paused (so we can resume all of them)
static PAUSED_PLAYERS: Mutex<Vec<String>> = Mutex::new(Vec::new());

pub fn set_enabled(enabled: bool) {
   ENABLED.store(enabled, Ordering::Relaxed);
   debug!("Auto play/pause set to {enabled}");
}

pub fn is_enabled() -> bool {
   ENABLED.load(Ordering::Relaxed)
}

enum Command {
   Play,
   Pause,
}

/// What an ear detection change does to playback.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PlaybackAction {
   Play,
   Pause,
}

/// Decides the playback change for an ear detection report.
///
/// Follows the number of earbuds in an ear: taking one out pauses, putting one in
/// resumes. Moves between hand and case do not change that number and are ignored, so
/// one-bud listening is not paused by the other bud going into the case. The first report
/// of a connection (`previous` is `None`) leaves playback alone: connecting changes
/// nothing in the ears.
pub fn playback_action(
   previous: Option<EarDetectionStatus>,
   status: EarDetectionStatus,
) -> Option<PlaybackAction> {
   let before = previous?.in_ear_count();
   let now = status.in_ear_count();
   match now.cmp(&before) {
      std::cmp::Ordering::Less => Some(PlaybackAction::Pause),
      std::cmp::Ordering::Greater => Some(PlaybackAction::Play),
      std::cmp::Ordering::Equal => None,
   }
}

/// Pauses and resumes media players on a task of its own.
///
/// Requests return immediately and run in the order they were made, so a slow or frozen
/// player cannot hold up the caller.
pub struct MediaControl {
   tx: mpsc::UnboundedSender<Command>,
}

impl MediaControl {
   /// Starts the media control task, which talks to the players over `connection`.
   pub fn spawn(connection: Connection) -> Self {
      let (tx, mut rx) = mpsc::unbounded_channel();
      tokio::spawn(async move {
         while let Some(command) = rx.recv().await {
            match command {
               Command::Play => send_play(&connection).await,
               Command::Pause => send_pause(&connection).await,
            }
         }
      });
      Self { tx }
   }

   /// Resumes the players paused by the last [`Self::pause`].
   pub fn play(&self) {
      let _ = self.tx.send(Command::Play);
   }

   /// Pauses all playing media players.
   pub fn pause(&self) {
      let _ = self.tx.send(Command::Pause);
   }

   /// Requests the playback change decided by [`playback_action`].
   pub fn apply(&self, action: PlaybackAction) {
      match action {
         PlaybackAction::Play => self.play(),
         PlaybackAction::Pause => self.pause(),
      }
   }
}

/// Sends a play command to all players we previously paused.
/// Only plays if we previously paused the media.
async fn send_play(connection: &Connection) {
   if !is_enabled() {
      return;
   }

   // Get all players we paused
   let paused_players = PAUSED_PLAYERS.lock().clone();

   if paused_players.is_empty() {
      debug!("No media was paused by us, skipping play command");
      return;
   }

   debug!(
      "Resuming {} previously paused player(s): {:?}",
      paused_players.len(),
      paused_players
   );

   // Resume all paused players
   let mut successful = 0;

   for player_name in &paused_players {
      match send_mpris_command_to_player(connection, "Play", player_name).await {
         Ok(()) => {
            debug!("Successfully resumed player: {player_name}");
            successful += 1;
         },
         Err(e) => {
            warn!("Failed to resume player {player_name}: {e}");
         },
      }
   }

   debug!(
      "Resumed {}/{} players successfully",
      successful,
      paused_players.len()
   );

   // Clear the stored players since we've resumed them all
   PAUSED_PLAYERS.lock().clear();
}

/// Sends a pause command to all playing media players via MPRIS.
/// Stores all players that were paused (only if they were playing).
async fn send_pause(connection: &Connection) {
   if !is_enabled() {
      return;
   }

   // Find all playing players and pause them all
   let dbus_proxy = match zbus::fdo::DBusProxy::new(connection).await {
      Ok(proxy) => proxy,
      Err(e) => {
         warn!("Failed to create D-Bus proxy: {e}");
         return;
      },
   };

   let names = match time::timeout(CALL_TIMEOUT, dbus_proxy.list_names()).await {
      Ok(Ok(names)) => names,
      Ok(Err(e)) => {
         warn!("Failed to list D-Bus names: {e}");
         return;
      },
      Err(_) => {
         warn!("Timed out listing D-Bus names");
         return;
      },
   };

   // Find all MPRIS media players (excluding KDE Connect, which is for remote control)
   let mpris_services: Vec<_> = names
      .iter()
      .filter(|name| {
         let name_str = name.as_str();
         name_str.starts_with("org.mpris.MediaPlayer2.")
            && !name_str.contains("kdeconnect")
            && !name_str.contains("KDEConnect")
      })
      .collect();

   if mpris_services.is_empty() {
      debug!("No MPRIS media players found");
      return;
   }

   debug!(
      "Found {} MPRIS player(s), checking which are playing",
      mpris_services.len()
   );

   let mut paused_players = Vec::new();

   // Check each player and pause all that are playing
   for service_name in &mpris_services {
      // Check if this player is playing
      if let Ok(was_playing) = is_player_playing(connection, service_name.as_str()).await {
         if was_playing {
            debug!("Player {service_name} is playing, pausing it");
            // Pause this player
            match send_mpris_command_to_player(connection, "Pause", service_name.as_str()).await {
               Ok(()) => {
                  debug!("Successfully paused player: {service_name}");
                  paused_players.push(service_name.as_str().to_string());
               },
               Err(e) => {
                  warn!("Failed to pause player {service_name}: {e}");
               },
            }
         } else {
            debug!("Player {service_name} is not playing, skipping");
         }
      } else {
         debug!("Could not check playback status for player {service_name}, skipping");
      }
   }

   if paused_players.is_empty() {
      debug!("No playing players found to pause");
   } else {
      debug!(
         "Paused {} player(s), storing for resume: {:?}",
         paused_players.len(),
         paused_players
      );
      // Store all paused players
      *PAUSED_PLAYERS.lock() = paused_players;
   }
}

/// Checks if a specific player is currently playing.
async fn is_player_playing(connection: &Connection, service_name: &str) -> Result<bool, BoxError> {
   let path = zbus::zvariant::ObjectPath::from_str_unchecked("/org/mpris/MediaPlayer2");
   let interface = "org.mpris.MediaPlayer2.Player";
   let property = "PlaybackStatus";

   let reply = time::timeout(
      CALL_TIMEOUT,
      connection.call_method(
         Some(service_name),
         &path,
         Some("org.freedesktop.DBus.Properties"),
         "Get",
         &(interface, property),
      ),
   )
   .await??;

   let body = reply.body();
   let variant: zbus::zvariant::Value = body.deserialize()?;
   let status = match variant {
      zbus::zvariant::Value::Str(s) => s.to_string(),
      _ => {
         if let Ok(s) = String::try_from(variant) {
            s
         } else {
            return Ok(false);
         }
      },
   };

   Ok(status == "Playing")
}

/// Sends a command to a specific player by service name.
async fn send_mpris_command_to_player(
   connection: &Connection,
   method: &str,
   service_name: &str,
) -> Result<(), BoxError> {
   let path = zbus::zvariant::ObjectPath::from_str_unchecked("/org/mpris/MediaPlayer2");
   let interface = "org.mpris.MediaPlayer2.Player";

   debug!("Sending {method} command to specific player: {service_name}");

   time::timeout(
      CALL_TIMEOUT,
      connection.call_method(Some(service_name), &path, Some(interface), method, &()),
   )
   .await??;

   Ok(())
}

#[cfg(test)]
mod tests {
   use super::*;
   use crate::airpods::protocol::PodPlacement::{InCase, InEar, OutOfEar};

   fn status(
      left: crate::airpods::protocol::PodPlacement,
      right: crate::airpods::protocol::PodPlacement,
   ) -> EarDetectionStatus {
      EarDetectionStatus::new(left, right)
   }

   #[test]
   fn first_report_leaves_playback_alone() {
      assert_eq!(playback_action(None, status(InEar, InEar)), None);
      assert_eq!(playback_action(None, status(OutOfEar, InCase)), None);
   }

   #[test]
   fn taking_a_bud_out_pauses() {
      let both = status(InEar, InEar);
      assert_eq!(
         playback_action(Some(both), status(OutOfEar, InEar)),
         Some(PlaybackAction::Pause)
      );
      let one = status(InEar, OutOfEar);
      assert_eq!(
         playback_action(Some(one), status(OutOfEar, OutOfEar)),
         Some(PlaybackAction::Pause)
      );
   }

   #[test]
   fn putting_a_bud_in_resumes() {
      // One-bud listening: the bud comes back
      assert_eq!(
         playback_action(Some(status(OutOfEar, InCase)), status(InEar, InCase)),
         Some(PlaybackAction::Play)
      );
      assert_eq!(
         playback_action(Some(status(InEar, OutOfEar)), status(InEar, InEar)),
         Some(PlaybackAction::Play)
      );
   }

   #[test]
   fn moving_the_other_bud_to_the_case_is_ignored() {
      assert_eq!(
         playback_action(Some(status(InEar, OutOfEar)), status(InEar, InCase)),
         None
      );
      assert_eq!(
         playback_action(Some(status(InEar, InCase)), status(InEar, OutOfEar)),
         None
      );
   }
}

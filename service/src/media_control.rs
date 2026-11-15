//! Media control module for sending play/pause commands via MPRIS.
//!
//! This module provides functionality to control media playback using the
//! MPRIS (Media Player Remote Interfacing Specification) D-Bus interface.

use std::{
   sync::atomic::{AtomicBool, Ordering},
   time::Duration,
};

use futures::future::join_all;
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

/// Prefix of the bus names of MPRIS media players
const MPRIS_PREFIX: &str = "org.mpris.MediaPlayer2.";

/// The calls media control makes: to the players on the bus in the service, to fakes in
/// tests. They have no timeout of their own; callers add [`CALL_TIMEOUT`].
trait Players {
   /// Names on the bus.
   async fn names(&self) -> Result<Vec<String>, BoxError>;
   /// Whether the player is playing.
   async fn is_playing(&self, player: &str) -> Result<bool, BoxError>;
   /// Calls an MPRIS player method, such as `Play` or `Pause`.
   async fn command(&self, player: &str, method: &str) -> Result<(), BoxError>;
}

/// Runs a player call, giving up after [`CALL_TIMEOUT`].
async fn timed<T>(call: impl Future<Output = Result<T, BoxError>>) -> Result<T, BoxError> {
   time::timeout(CALL_TIMEOUT, call).await?
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
   let successful = resume_players(connection, &paused_players).await;
   debug!(
      "Resumed {}/{} players successfully",
      successful,
      paused_players.len()
   );

   // Clear the stored players since we've resumed them all
   PAUSED_PLAYERS.lock().clear();
}

/// Resumes the players, all at once: a frozen one does not hold up the others. Returns
/// how many did.
async fn resume_players(players: &impl Players, paused: &[String]) -> usize {
   let resumes = paused.iter().map(|player_name| async move {
      match timed(players.command(player_name, "Play")).await {
         Ok(()) => {
            debug!("Successfully resumed player: {player_name}");
            true
         },
         Err(e) => {
            warn!("Failed to resume player {player_name}: {e}");
            false
         },
      }
   });
   join_all(resumes).await.into_iter().filter(|&ok| ok).count()
}

/// Sends a pause command to all playing media players via MPRIS.
/// Stores all players that were paused (only if they were playing).
async fn send_pause(connection: &Connection) {
   if !is_enabled() {
      return;
   }

   let paused_players = pause_playing(connection).await;
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

/// Pauses every playing media player. Returns the ones it paused, in bus order.
async fn pause_playing(players: &impl Players) -> Vec<String> {
   let names = match timed(players.names()).await {
      Ok(names) => names,
      Err(e) => {
         warn!("Failed to list D-Bus names: {e}");
         return Vec::new();
      },
   };

   // Find all MPRIS media players (excluding KDE Connect, which is for remote control)
   let mpris_services: Vec<_> = names
      .into_iter()
      .filter(|name| {
         name.starts_with(MPRIS_PREFIX)
            && !name.contains("kdeconnect")
            && !name.contains("KDEConnect")
      })
      .collect();

   if mpris_services.is_empty() {
      debug!("No MPRIS media players found");
      return Vec::new();
   }

   debug!(
      "Found {} MPRIS player(s), checking which are playing",
      mpris_services.len()
   );

   // All at once: a frozen player does not hold up pausing the others
   let pauses = mpris_services
      .into_iter()
      .map(|service_name| pause_if_playing(players, service_name));
   join_all(pauses).await.into_iter().flatten().collect()
}

/// Pauses the player if it is playing. Returns its name if it paused it.
async fn pause_if_playing(players: &impl Players, service_name: String) -> Option<String> {
   match timed(players.is_playing(&service_name)).await {
      Ok(true) => {},
      Ok(false) => {
         debug!("Player {service_name} is not playing, skipping");
         return None;
      },
      Err(e) => {
         debug!("Could not check playback status for player {service_name}, skipping: {e}");
         return None;
      },
   }
   debug!("Player {service_name} is playing, pausing it");
   match timed(players.command(&service_name, "Pause")).await {
      Ok(()) => {
         debug!("Successfully paused player: {service_name}");
         Some(service_name)
      },
      Err(e) => {
         warn!("Failed to pause player {service_name}: {e}");
         None
      },
   }
}

impl Players for Connection {
   async fn names(&self) -> Result<Vec<String>, BoxError> {
      let reply = self
         .call_method(
            Some("org.freedesktop.DBus"),
            "/org/freedesktop/DBus",
            Some("org.freedesktop.DBus"),
            "ListNames",
            &(),
         )
         .await?;
      Ok(reply.body().deserialize()?)
   }

   async fn is_playing(&self, player: &str) -> Result<bool, BoxError> {
      let reply = self
         .call_method(
            Some(player),
            "/org/mpris/MediaPlayer2",
            Some("org.freedesktop.DBus.Properties"),
            "Get",
            &("org.mpris.MediaPlayer2.Player", "PlaybackStatus"),
         )
         .await?;
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

   async fn command(&self, player: &str, method: &str) -> Result<(), BoxError> {
      debug!("Sending {method} command to specific player: {player}");
      self
         .call_method(
            Some(player),
            "/org/mpris/MediaPlayer2",
            Some("org.mpris.MediaPlayer2.Player"),
            method,
            &(),
         )
         .await?;
      Ok(())
   }
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

   /// Players on a fake bus; those named `frozen` never answer.
   #[derive(Default)]
   struct FakePlayers {
      names: Vec<String>,
      /// Commands as they were made: player, method, time since the start
      commands: Mutex<Vec<(String, String, Duration)>>,
      start: Option<time::Instant>,
   }

   impl FakePlayers {
      fn new(names: &[&str]) -> Self {
         Self {
            names: names.iter().map(|&n| n.to_owned()).collect(),
            commands: Mutex::default(),
            start: Some(time::Instant::now()),
         }
      }

      async fn answer(player: &str) {
         if player.contains("frozen") {
            std::future::pending::<()>().await;
         }
      }
   }

   impl Players for FakePlayers {
      async fn names(&self) -> Result<Vec<String>, BoxError> {
         Ok(self.names.clone())
      }

      async fn is_playing(&self, player: &str) -> Result<bool, BoxError> {
         Self::answer(player).await;
         Ok(true)
      }

      async fn command(&self, player: &str, method: &str) -> Result<(), BoxError> {
         Self::answer(player).await;
         let at = self.start.unwrap().elapsed();
         self
            .commands
            .lock()
            .push((player.to_owned(), method.to_owned(), at));
         Ok(())
      }
   }

   const FROZEN: &str = "org.mpris.MediaPlayer2.frozen";
   const PLAYER: &str = "org.mpris.MediaPlayer2.player";

   #[tokio::test(start_paused = true)]
   async fn a_frozen_player_does_not_delay_pausing_the_others() {
      let players = FakePlayers::new(&[FROZEN, PLAYER, "org.kde.StatusNotifierWatcher"]);
      let started = time::Instant::now();

      let paused = pause_playing(&players).await;

      assert_eq!(paused, [PLAYER]);
      assert_eq!(
         players.commands.lock().as_slice(),
         [(PLAYER.to_owned(), "Pause".to_owned(), Duration::ZERO)]
      );
      // Given up on the frozen one after one timeout, not one per call
      assert_eq!(started.elapsed(), CALL_TIMEOUT);
   }

   #[tokio::test(start_paused = true)]
   async fn a_frozen_player_does_not_delay_resuming_the_others() {
      let players = FakePlayers::new(&[]);
      let started = time::Instant::now();

      let resumed = resume_players(&players, &[FROZEN.to_owned(), PLAYER.to_owned()]).await;

      assert_eq!(resumed, 1);
      assert_eq!(
         players.commands.lock().as_slice(),
         [(PLAYER.to_owned(), "Play".to_owned(), Duration::ZERO)]
      );
      assert_eq!(started.elapsed(), CALL_TIMEOUT);
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

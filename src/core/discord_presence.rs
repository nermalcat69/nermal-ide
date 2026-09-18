//! Discord Rich Presence — shows what nermal is doing on the user's Discord
//! profile: "Editing in Nermal" or "Cooking in Nermal" (picked in Settings),
//! plus the file currently open.
//!
//! Speaks Discord's local IPC directly (a Unix socket on macOS/Linux, a named
//! pipe on Windows) via the `discord-rich-presence` crate, which implements
//! both behind the same API — nothing here is platform-specific, and the
//! background thread below runs the same on every OS.
//!
//! Best-effort throughout: Discord not being installed, not running, or
//! rejecting the connection never surfaces to the user or affects anything
//! else nermal does. Every failure just means the next [`sync`] tries again.

use std::sync::Mutex;
use std::sync::mpsc::{Sender, channel};

use discord_rich_presence::activity::{Activity, Timestamps};
use discord_rich_presence::{DiscordIpc, DiscordIpcClient};

use crate::core::config::DiscordPresenceActivity;

/// Registered at <https://discord.com/developers/applications>. Nermal ships
/// with no application of its own here — Discord Rich Presence only works
/// once this is replaced with a real application id, created (free, a few
/// clicks) under whichever Discord account should own nermal's presence.
const CLIENT_ID: &str = "0000000000000000000000";

enum Command {
    Show {
        activity: DiscordPresenceActivity,
        file_name: Option<String>,
    },
    Disable,
}

static SENDER: Mutex<Option<Sender<Command>>> = Mutex::new(None);

/// What was last asked for, so repeated identical frames do not each cost an
/// IPC round trip — `sync` is cheap to call from every render precisely
/// because most calls stop here.
static LAST: Mutex<Option<(bool, DiscordPresenceActivity, Option<String>)>> = Mutex::new(None);

fn sender() -> Sender<Command> {
    let mut guard = SENDER.lock().unwrap_or_else(|p| p.into_inner());
    if let Some(tx) = guard.as_ref() {
        return tx.clone();
    }
    let (tx, rx) = channel();
    *guard = Some(tx.clone());
    let spawned = std::thread::Builder::new()
        .name("nermal-discord-presence".to_string())
        .spawn(move || run(rx));
    if let Err(e) = spawned {
        log::warn!("discord presence: could not start its own thread: {e}");
    }
    tx
}

/// Tells Discord what nermal is doing right now, if the feature is on.
/// Cheap enough to call on every render — see `LAST`.
pub fn sync(enabled: bool, activity: DiscordPresenceActivity, file_name: Option<String>) {
    let mut last = LAST.lock().unwrap_or_else(|p| p.into_inner());
    let this_call = (enabled, activity, file_name.clone());
    if *last == Some(this_call.clone()) {
        return;
    }
    *last = Some(this_call);
    drop(last);
    let command = if enabled {
        Command::Show {
            activity,
            file_name,
        }
    } else {
        // No thread ever started means nothing was ever shown, so there is
        // nothing to clear — starting one just to tell it to disable would
        // be the one case where turning the feature off costs a connection
        // to Discord it never needed to make.
        if SENDER.lock().unwrap_or_else(|p| p.into_inner()).is_none() {
            return;
        }
        Command::Disable
    };
    let _ = sender().send(command);
}

fn run(rx: std::sync::mpsc::Receiver<Command>) {
    let started_at = std::time::SystemTime::now()
        .duration_since(std::time::SystemTime::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0);
    let mut client: Option<DiscordIpcClient> = None;
    for command in rx {
        match command {
            Command::Disable => {
                if let Some(mut c) = client.take() {
                    let _ = c.clear_activity();
                    let _ = c.close();
                }
            }
            Command::Show {
                activity,
                file_name,
            } => {
                if client.is_none() {
                    client = connect();
                    if client.is_none() {
                        continue;
                    }
                }
                let mut payload = Activity::new()
                    .state(activity_label(activity))
                    .timestamps(Timestamps::new().start(started_at));
                if let Some(name) = file_name.as_deref() {
                    payload = payload.details(name);
                }
                // A connection Discord has since dropped (it quit, or the
                // user signed out) fails here rather than at the next
                // connect — reconnecting once before giving up on this
                // update is what an editor left open for hours needs to
                // pick Discord back up without a restart.
                let sent = client
                    .as_mut()
                    .map(|c| c.set_activity(payload.clone()))
                    .unwrap();
                if sent.is_err() {
                    client = connect();
                    if let Some(c) = client.as_mut() {
                        let _ = c.set_activity(payload);
                    }
                }
            }
        }
    }
}

fn connect() -> Option<DiscordIpcClient> {
    let mut client = DiscordIpcClient::new(CLIENT_ID);
    client
        .connect()
        .inspect_err(|e| log::debug!("discord presence: connect failed: {e}"))
        .ok()?;
    Some(client)
}

/// The Settings toggle's two choices, spelled the way Discord shows them.
fn activity_label(activity: DiscordPresenceActivity) -> &'static str {
    match activity {
        DiscordPresenceActivity::Editing => "Editing in Nermal",
        DiscordPresenceActivity::Cooking => "Cooking in Nermal",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn each_activity_names_nermal_by_the_verb_settings_picked() {
        assert_eq!(
            activity_label(DiscordPresenceActivity::Editing),
            "Editing in Nermal"
        );
        assert_eq!(
            activity_label(DiscordPresenceActivity::Cooking),
            "Cooking in Nermal"
        );
    }
}

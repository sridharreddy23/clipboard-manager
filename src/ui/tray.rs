//! Optional `StatusNotifierItem` tray icon (§26).
//!
//! The menu is: Open Clipboard History, Pause/Resume, Clear History, Settings,
//! About, Quit.
//!
//! # Optional means optional
//!
//! §26 is explicit that the tray must not be mandatory, and that matters more
//! on Linux than it sounds: GNOME has shipped without a system tray since 3.26,
//! so on the single most common desktop this will simply not appear unless the
//! user has the AppIndicator extension. Everything the menu offers is therefore
//! reachable another way — the shortcut, the panel's own footer, and the CLI —
//! and a failure to register is logged at debug and otherwise ignored.
//!
//! # Threading
//!
//! `ksni` runs its own D-Bus service on a background thread, so menu callbacks
//! fire *off* the GTK main thread. Every one of them therefore does nothing but
//! post a [`TrayCommand`] down a channel; the daemon picks it up on the main
//! loop. Touching a GTK widget from the ksni thread would be a crash waiting
//! for a user to click a menu item.

use ksni::menu::{MenuItem, StandardItem};

/// What a menu item asks the daemon to do.
///
/// Mirrors the subset of [`crate::ipc::Request`] the tray exposes, as a
/// separate type because the tray is in-process and need not go through the
/// socket.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TrayCommand {
    Open,
    Pause,
    Resume,
    Clear,
    Settings,
    About,
    Quit,
}

/// The tray item. Cloned into the ksni thread, so it holds only a channel.
#[derive(Debug, Clone)]
pub struct Tray {
    commands: async_channel::Sender<TrayCommand>,
    paused: bool,
}

impl Tray {
    fn send(&self, command: TrayCommand) {
        // `try_send` rather than blocking: this runs on the ksni thread and
        // must never stall it. An unbounded channel only refuses when the
        // daemon is already shutting down.
        if self.commands.try_send(command).is_err() {
            tracing::debug!(?command, "tray command dropped; the daemon is shutting down");
        }
    }
}

impl ksni::Tray for Tray {
    fn id(&self) -> String {
        "clipboard-manager".into()
    }

    fn title(&self) -> String {
        "Clipboard Manager".into()
    }

    fn icon_name(&self) -> String {
        // From the standard icon theme, so there is nothing to install for the
        // tray to show something sensible.
        "edit-paste".into()
    }

    fn tool_tip(&self) -> ksni::ToolTip {
        ksni::ToolTip {
            icon_name: "edit-paste".into(),
            title: "Clipboard Manager".into(),
            description: if self.paused {
                "Monitoring is paused".into()
            } else {
                "Recording clipboard history".into()
            },
            icon_pixmap: Vec::new(),
        }
    }

    /// Left-clicking the icon opens the panel, which is what every other
    /// clipboard manager does and therefore what users will try first.
    fn activate(&mut self, _x: i32, _y: i32) {
        self.send(TrayCommand::Open);
    }

    fn menu(&self) -> Vec<MenuItem<Self>> {
        vec![
            StandardItem {
                label: "Open Clipboard History".into(),
                icon_name: "edit-paste".into(),
                activate: Box::new(|tray: &mut Self| tray.send(TrayCommand::Open)),
                ..Default::default()
            }
            .into(),
            MenuItem::Separator,
            // One item that changes label, rather than two with one greyed
            // out: the state is then readable from the menu itself.
            if self.paused {
                StandardItem {
                    label: "Resume Monitoring".into(),
                    icon_name: "media-playback-start".into(),
                    activate: Box::new(|tray: &mut Self| tray.send(TrayCommand::Resume)),
                    ..Default::default()
                }
                .into()
            } else {
                StandardItem {
                    label: "Pause Monitoring".into(),
                    icon_name: "media-playback-pause".into(),
                    activate: Box::new(|tray: &mut Self| tray.send(TrayCommand::Pause)),
                    ..Default::default()
                }
                .into()
            },
            StandardItem {
                label: "Clear History…".into(),
                icon_name: "edit-clear-all".into(),
                activate: Box::new(|tray: &mut Self| tray.send(TrayCommand::Clear)),
                ..Default::default()
            }
            .into(),
            MenuItem::Separator,
            StandardItem {
                label: "Settings…".into(),
                icon_name: "preferences-system".into(),
                activate: Box::new(|tray: &mut Self| tray.send(TrayCommand::Settings)),
                ..Default::default()
            }
            .into(),
            StandardItem {
                label: "About".into(),
                icon_name: "help-about".into(),
                activate: Box::new(|tray: &mut Self| tray.send(TrayCommand::About)),
                ..Default::default()
            }
            .into(),
            MenuItem::Separator,
            StandardItem {
                label: "Quit".into(),
                icon_name: "application-exit".into(),
                activate: Box::new(|tray: &mut Self| tray.send(TrayCommand::Quit)),
                ..Default::default()
            }
            .into(),
        ]
    }
}

/// A running tray, kept alive by holding the handle.
pub struct TrayHandle {
    handle: ksni::Handle<Tray>,
    commands: async_channel::Receiver<TrayCommand>,
}

impl TrayHandle {
    /// Menu activations, to be drained on the GTK main loop.
    pub fn commands(&self) -> async_channel::Receiver<TrayCommand> {
        self.commands.clone()
    }

    /// Update the Pause/Resume label after the state changes elsewhere — from
    /// the CLI, or the Privacy tab.
    pub fn set_paused(&self, paused: bool) {
        self.handle.update(move |tray: &mut Tray| tray.paused = paused);
    }

    pub fn shutdown(&self) {
        self.handle.shutdown();
    }
}

/// Try to show a tray icon.
///
/// `Ok(None)` means there is no tray on this desktop, which is an ordinary
/// outcome and not an error — see the module docs.
pub fn start(paused: bool) -> Option<TrayHandle> {
    let (tx, rx) = async_channel::unbounded();
    let service = ksni::TrayService::new(Tray { commands: tx, paused });
    let handle = service.handle();

    // Spawns its own thread and connects to the session bus. It does not fail
    // loudly when no StatusNotifierHost exists; the item simply goes unwatched,
    // which is exactly the GNOME-without-the-extension case.
    service.spawn();

    tracing::info!("tray icon registered (it will only appear if the desktop has a tray)");
    Some(TrayHandle { handle, commands: rx })
}

#[cfg(test)]
mod tests {
    use super::*;
    // Anonymous import: the trait's methods are what we need, and naming it
    // would collide with our own `Tray` struct.
    use ksni::Tray as _;

    #[test]
    fn the_menu_covers_every_action_the_specification_lists() {
        // §26 enumerates these; a missing one is a silently absent feature.
        let (tx, _rx) = async_channel::unbounded();
        let tray = Tray { commands: tx, paused: false };

        let labels: Vec<String> = tray
            .menu()
            .iter()
            .filter_map(|item| match item {
                MenuItem::Standard(standard) => Some(standard.label.clone()),
                _ => None,
            })
            .collect();

        for expected in ["Open Clipboard History", "Clear History…", "Settings…", "About", "Quit"] {
            assert!(labels.iter().any(|l| l == expected), "{expected:?} missing from {labels:?}");
        }
    }

    #[test]
    fn the_pause_item_reflects_the_current_state() {
        let (tx, _rx) = async_channel::unbounded();

        let running = Tray { commands: tx.clone(), paused: false };
        let labels = standard_labels(&running);
        assert!(labels.iter().any(|l| l == "Pause Monitoring"), "{labels:?}");
        assert!(!labels.iter().any(|l| l == "Resume Monitoring"), "{labels:?}");

        let paused = Tray { commands: tx, paused: true };
        let labels = standard_labels(&paused);
        assert!(labels.iter().any(|l| l == "Resume Monitoring"), "{labels:?}");
        assert!(!labels.iter().any(|l| l == "Pause Monitoring"), "{labels:?}");
    }

    #[test]
    fn a_menu_click_posts_a_command_rather_than_acting_inline() {
        // The ksni callbacks run off the GTK thread, so anything other than a
        // channel send would be a latent crash.
        let (tx, rx) = async_channel::unbounded();
        let tray = Tray { commands: tx, paused: false };
        tray.send(TrayCommand::Open);
        assert_eq!(rx.try_recv().unwrap(), TrayCommand::Open);
    }

    #[test]
    fn sending_after_shutdown_is_harmless() {
        // The daemon drops the receiver first on the way out; the tray thread
        // may still be processing a click.
        let (tx, rx) = async_channel::unbounded();
        let tray = Tray { commands: tx, paused: false };
        drop(rx);
        tray.send(TrayCommand::Quit);
    }

    #[test]
    fn the_tooltip_states_whether_monitoring_is_paused() {
        let (tx, _rx) = async_channel::unbounded();
        let paused = Tray { commands: tx.clone(), paused: true };
        assert!(paused.tool_tip().description.contains("paused"));
        let running = Tray { commands: tx, paused: false };
        assert!(running.tool_tip().description.contains("Recording"));
    }

    fn standard_labels(tray: &Tray) -> Vec<String> {
        tray.menu()
            .iter()
            .filter_map(|item| match item {
                MenuItem::Standard(standard) => Some(standard.label.clone()),
                _ => None,
            })
            .collect()
    }
}

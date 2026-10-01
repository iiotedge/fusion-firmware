// src/matter/commissioning.rs
//
// Keeps the node ADDABLE once its last controller is gone.
//
// rs-matter opens a commissioning window only when asked, never on its own, and
// `run()` asks at boot when the node has no controller. Left at that, a node
// whose last controller removes it (Apple Home "Remove Accessory", `chip-tool
// pairing unpair`, a commissioning that fails and whose fail-safe expires)
// answers nobody and is not discoverable, so adding it again would need a
// service restart. The reference SDK reopens the window in exactly this
// situation; this does the same.
//
// Only on the "had a controller, now has none" transition. A node that was never
// commissioned, or whose window simply timed out, stays closed: reopening on a
// timer would leave an uncommissioned node pairable by anyone on the LAN for ever.
//
// A window an ADMINISTRATOR opened ("turn on pairing mode", the share-to-another-
// ecosystem step) is not left to run out when the last controller goes: it carries a
// one-off passcode that only the departed controller knew, so it would keep the node
// unaddable with the code a person has for up to its whole timeout (Apple opens
// 3-minute windows). It is replaced by the node's own window straight away.

use std::time::Duration;

use async_io::Timer;
use rs_matter::error::Error;
use tracing::warn;

/// How often the fabric table is looked at. Removal is a human action, so half a
/// second is prompt, and the look is a lock and a pointer compare.
const POLL: Duration = Duration::from_millis(500);

/// Which commissioning window, if any, the node has open.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Window {
    Closed,
    /// The node's own window (what it opens at boot / after the last controller): the
    /// setup code printed in the log and shown by `--matter-qr` works.
    Device,
    /// Opened by an administrator over CASE: a one-off passcode, not the setup code.
    Admin,
}

/// What the watcher must do now.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Step {
    Nothing,
    /// No window: open the node's own.
    Open,
    /// An orphaned administrator window is in the way: close it, then open the node's own.
    ReplaceAdminWindow,
}

/// When the window must be (re)opened. Pure, so it is tested without a node.
pub(crate) struct Reopen {
    /// A fabric was there the last time we looked.
    had_fabrics: bool,
    /// The last fabric went away and the node's own window is not open yet.
    pending: bool,
}

impl Reopen {
    pub(crate) fn new(has_fabrics: bool) -> Self {
        Self {
            had_fabrics: has_fabrics,
            pending: false,
        }
    }

    /// Feed what the node looks like now.
    ///
    /// A fabric that returns before we got to it cancels the reopening, and so does
    /// the node's own window being open already (a window that merely times out later
    /// is not reopened: that is the "never on a timer" rule).
    pub(crate) fn poll(&mut self, has_fabrics: bool, window: Window) -> Step {
        if self.had_fabrics && !has_fabrics {
            self.pending = true;
        }
        self.had_fabrics = has_fabrics;
        if has_fabrics || window == Window::Device {
            self.pending = false;
        }
        match (self.pending, window) {
            (true, Window::Closed) => Step::Open,
            (true, Window::Admin) => Step::ReplaceAdminWindow,
            _ => Step::Nothing,
        }
    }

    /// The node's own window was opened; nothing is owed until the next removal.
    pub(crate) fn opened(&mut self) {
        self.pending = false;
    }
}

/// Runs for the life of the node, reopening the commissioning window each time
/// the last controller goes away.
///
/// Takes closures rather than the node so it does not drag in the interaction
/// model's many generic parameters: `has_fabrics` and `window` read the node,
/// `close_window` and `open_window` change it, and `on_reopened` tells the
/// operator (the log with the pairing code).
pub(crate) async fn reopen_after_last_fabric_removed(
    has_fabrics: impl Fn() -> bool,
    window: impl Fn() -> Window,
    close_window: impl Fn() -> Result<bool, Error>,
    open_window: impl Fn() -> Result<(), Error>,
    on_reopened: impl Fn(),
) -> Result<(), Error> {
    let mut watch = Reopen::new(has_fabrics());
    loop {
        Timer::after(POLL).await;
        let step = watch.poll(has_fabrics(), window());
        if step == Step::Nothing {
            continue;
        }
        if step == Step::ReplaceAdminWindow {
            if let Err(e) = close_window() {
                // Stays owed: the next look tries again.
                warn!("Matter: could not close the administrator's commissioning window: {e:?}");
                continue;
            }
        }
        match open_window() {
            Ok(()) => {
                watch.opened();
                on_reopened();
            }
            // Stays owed: the next look tries again.
            Err(e) => warn!("Matter: could not reopen the commissioning window: {e:?}"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use Step::{Nothing, Open, ReplaceAdminWindow};
    use Window::{Admin, Closed, Device};

    #[test]
    fn a_node_that_was_never_commissioned_is_left_alone() {
        let mut watch = Reopen::new(false);
        for _ in 0..10 {
            // Whatever window it has, or has not, at the moment.
            for w in [Closed, Device, Admin] {
                assert_eq!(watch.poll(false, w), Nothing);
            }
        }
    }

    #[test]
    fn removing_the_last_controller_reopens_the_window_once() {
        let mut watch = Reopen::new(true);
        assert_eq!(watch.poll(true, Closed), Nothing, "commissioned and well");
        assert_eq!(watch.poll(false, Closed), Open, "the controller was removed");
        watch.opened();
        assert_eq!(watch.poll(false, Device), Nothing, "our window is now open");
        // The window times out with nobody added: stay closed, do not loop.
        assert_eq!(watch.poll(false, Closed), Nothing, "a timed-out window is not reopened");
    }

    #[test]
    fn a_failed_attempt_is_retried_on_the_next_look() {
        let mut watch = Reopen::new(true);
        assert_eq!(watch.poll(false, Closed), Open);
        // `opened()` is only called when opening worked.
        assert_eq!(watch.poll(false, Closed), Open, "still owed");
        assert_eq!(watch.poll(false, Closed), Open, "and still");
        watch.opened();
        assert_eq!(watch.poll(false, Closed), Nothing);
    }

    #[test]
    fn an_orphaned_administrator_window_is_replaced_not_waited_out() {
        // "Turn on pairing mode" opens a window with a one-off passcode; if the last
        // controller leaves meanwhile, that window is useless with the code a person
        // has, and would keep the node unaddable until it timed out.
        let mut watch = Reopen::new(true);
        assert_eq!(watch.poll(false, Admin), ReplaceAdminWindow);
        assert_eq!(watch.poll(false, Admin), ReplaceAdminWindow, "retried until it went through");
        watch.opened();
        assert_eq!(watch.poll(false, Device), Nothing);
    }

    #[test]
    fn an_administrator_window_while_a_controller_remains_is_none_of_our_business() {
        let mut watch = Reopen::new(true);
        for _ in 0..5 {
            assert_eq!(watch.poll(true, Admin), Nothing, "sharing to a second ecosystem");
        }
    }

    #[test]
    fn our_own_window_already_open_cancels_the_reopening() {
        let mut watch = Reopen::new(true);
        assert_eq!(watch.poll(false, Device), Nothing, "already addable");
        assert_eq!(watch.poll(false, Closed), Nothing, "and when it times out we do not reopen it");
    }

    #[test]
    fn a_controller_that_returns_first_cancels_the_reopening() {
        let mut watch = Reopen::new(true);
        assert_eq!(watch.poll(false, Admin), ReplaceAdminWindow);
        assert_eq!(watch.poll(true, Closed), Nothing, "a fabric came back");
        assert_eq!(watch.poll(true, Closed), Nothing);
    }

    #[test]
    fn every_removal_is_handled_not_just_the_first() {
        let mut watch = Reopen::new(false);
        for _ in 0..3 {
            assert_eq!(watch.poll(true, Closed), Nothing, "commissioned");
            assert_eq!(watch.poll(false, Closed), Open, "removed");
            watch.opened();
            assert_eq!(watch.poll(false, Device), Nothing);
        }
    }

    #[test]
    fn a_fail_safe_expiry_during_commissioning_counts_like_a_removal() {
        // AddNOC creates the fabric well before CommissioningComplete; if the
        // commissioner vanishes, the fail-safe deletes it again.
        let mut watch = Reopen::new(false);
        assert_eq!(watch.poll(true, Device), Nothing, "AddNOC: fabric present, window still open");
        assert_eq!(watch.poll(false, Closed), Open, "fail-safe expired: bare again, window shut");
    }
}

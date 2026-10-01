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

use std::time::Duration;

use async_io::Timer;
use rs_matter::error::Error;
use tracing::warn;

/// How often the fabric table is looked at. Removal is a human action, so half a
/// second is prompt, and the look is a lock and a pointer compare.
const POLL: Duration = Duration::from_millis(500);

/// When the window must be (re)opened. Pure, so it is tested without a node.
pub(crate) struct Reopen {
    /// A fabric was there the last time we looked.
    had_fabrics: bool,
    /// The last fabric went away and the window has not been reopened yet.
    pending: bool,
}

impl Reopen {
    pub(crate) fn new(has_fabrics: bool) -> Self {
        Self {
            had_fabrics: has_fabrics,
            pending: false,
        }
    }

    /// Feed what the node looks like now. `true`: open the window now.
    ///
    /// A window somebody else already holds open is left alone, and the reopening
    /// is only owed again once that window closes; a fabric that returns before
    /// we got to it cancels it.
    pub(crate) fn poll(&mut self, has_fabrics: bool, window_open: bool) -> bool {
        if self.had_fabrics && !has_fabrics {
            self.pending = true;
        }
        self.had_fabrics = has_fabrics;
        if has_fabrics {
            self.pending = false;
        }
        self.pending && !window_open
    }

    /// The window was opened; nothing is owed until the next removal.
    pub(crate) fn opened(&mut self) {
        self.pending = false;
    }
}

/// Runs for the life of the node, reopening the commissioning window each time
/// the last controller goes away.
///
/// Takes closures rather than the node so it does not drag in the interaction
/// model's many generic parameters: `has_fabrics` and `window_open` read the
/// node, `open_window` asks it for a fresh window, and `on_reopened` tells the
/// operator (the log with the pairing code).
pub(crate) async fn reopen_after_last_fabric_removed(
    has_fabrics: impl Fn() -> bool,
    window_open: impl Fn() -> bool,
    open_window: impl Fn() -> Result<(), Error>,
    on_reopened: impl Fn(),
) -> Result<(), Error> {
    let mut watch = Reopen::new(has_fabrics());
    loop {
        Timer::after(POLL).await;
        if watch.poll(has_fabrics(), window_open()) {
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
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_node_that_was_never_commissioned_is_left_alone() {
        let mut watch = Reopen::new(false);
        for _ in 0..10 {
            // Whether or not its boot-time window is still open.
            assert!(!watch.poll(false, false));
            assert!(!watch.poll(false, true));
        }
    }

    #[test]
    fn removing_the_last_controller_reopens_the_window_once() {
        let mut watch = Reopen::new(true);
        assert!(!watch.poll(true, false), "commissioned and well");
        assert!(watch.poll(false, false), "the controller was removed");
        watch.opened();
        assert!(!watch.poll(false, true), "window now open");
        // The window times out with nobody added: stay closed, do not loop.
        assert!(!watch.poll(false, false), "a timed-out window is not reopened");
    }

    #[test]
    fn a_failed_attempt_is_retried_on_the_next_look() {
        let mut watch = Reopen::new(true);
        assert!(watch.poll(false, false));
        // `opened()` is only called when opening worked.
        assert!(watch.poll(false, false), "still owed");
        assert!(watch.poll(false, false), "and still");
        watch.opened();
        assert!(!watch.poll(false, false));
    }

    #[test]
    fn a_window_somebody_else_holds_is_waited_out_not_fought() {
        let mut watch = Reopen::new(true);
        assert!(!watch.poll(false, true), "already open: nothing to do yet");
        assert!(!watch.poll(false, true));
        assert!(watch.poll(false, false), "it closed with the node still bare: now open ours");
    }

    #[test]
    fn a_controller_that_returns_first_cancels_the_reopening() {
        let mut watch = Reopen::new(true);
        assert!(!watch.poll(false, true));
        assert!(!watch.poll(true, false), "a fabric came back");
        assert!(!watch.poll(true, false));
    }

    #[test]
    fn every_removal_is_handled_not_just_the_first() {
        let mut watch = Reopen::new(false);
        for _ in 0..3 {
            assert!(!watch.poll(true, false), "commissioned");
            assert!(watch.poll(false, false), "removed");
            watch.opened();
            assert!(!watch.poll(false, true));
        }
    }

    #[test]
    fn a_fail_safe_expiry_during_commissioning_counts_like_a_removal() {
        // AddNOC creates the fabric well before CommissioningComplete; if the
        // commissioner vanishes, the fail-safe deletes it again.
        let mut watch = Reopen::new(false);
        assert!(!watch.poll(true, true), "AddNOC: fabric present, window still open");
        assert!(watch.poll(false, false), "fail-safe expired: bare again, window shut");
    }
}

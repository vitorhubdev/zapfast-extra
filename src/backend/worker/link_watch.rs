//! Link watchdog ported from upstream ZapFast (crmne/zapfast, MIT) for the Vespera fork.
//!
//! Notice a link that is still open but no longer carries anything.
//!
//! After a suspend, or a network change underneath it, a TCP connection can
//! stay established while nothing arrives, and a write into it can wait
//! forever for buffer space. whatsapp-rust's keepalive sends its ping through
//! that same write, and its answer deadline only starts once the write has
//! finished, so a wedged write silences the keepalive and its dead-socket
//! watchdog alike: no error, no disconnect, no reconnect (issue #177).
//!
//! The worker's tick asks this watch two questions and reconnects through
//! the library when either says so:
//!
//! - Did the machine sleep since the last tick? The wall clock keeps running
//!   through a suspend, while the monotonic clock behind [`Instant`] stops on
//!   Linux (`CLOCK_MONOTONIC`) and macOS (`CLOCK_UPTIME_RAW`). On Windows,
//!   `Instant` counts the suspend too, so this question never fires there and
//!   the next one fires on the first tick after any sleep longer than its
//!   limit.
//! - Has nothing at all arrived for longer than a working link allows? The
//!   library pings an idle link every 15 to 30 seconds, so its answers alone
//!   keep the received-frame counter moving.

use std::time::{Duration, Instant, SystemTime};

/// Wall-clock time that must pass beyond the monotonic clock between two ticks
/// before the machine counts as having slept. Ticks are five seconds apart; a
/// busy worker delays both clocks alike, so only a suspend opens this gap.
pub(super) const SLEEP_GAP: Duration = Duration::from_secs(30);

/// Longest a connected link may go without receiving a single frame. A
/// working one answers a keepalive at least every minute or so.
pub(super) const SILENCE_LIMIT: Duration = Duration::from_secs(120);

#[derive(Debug, PartialEq, Eq)]
pub(super) enum Verdict {
    Healthy,
    /// The machine slept at least this long since the last tick.
    Slept(Duration),
    /// Nothing arrived for this long.
    Silent(Duration),
}

#[derive(Default)]
pub(super) struct LinkWatch {
    /// The two clocks at the previous tick.
    last_tick: Option<(Instant, SystemTime)>,
    /// The received-frame count, and when it was first seen at that value.
    frames: Option<(u64, Instant)>,
}

impl LinkWatch {
    /// Checks the link at a tick. `frames_received` is the library's
    /// cumulative received-frame count while the link is connected, `None`
    /// otherwise; a verdict other than `Healthy` asks for a reconnect and
    /// restarts the watch.
    pub fn check(
        &mut self,
        now: Instant,
        wall: SystemTime,
        frames_received: Option<u64>,
    ) -> Verdict {
        let slept = self
            .last_tick
            .replace((now, wall))
            .and_then(|(then, then_wall)| {
                let awake = now.saturating_duration_since(then);
                let passed = wall.duration_since(then_wall).unwrap_or_default();
                passed.checked_sub(awake).filter(|gap| *gap >= SLEEP_GAP)
            });
        let Some(frames) = frames_received else {
            self.frames = None;
            return Verdict::Healthy;
        };
        if let Some(gap) = slept {
            self.frames = Some((frames, now));
            return Verdict::Slept(gap);
        }
        match self.frames {
            Some((seen, since)) if seen == frames => {
                let quiet = now.saturating_duration_since(since);
                if quiet >= SILENCE_LIMIT {
                    self.frames = Some((frames, now));
                    return Verdict::Silent(quiet);
                }
            }
            _ => self.frames = Some((frames, now)),
        }
        Verdict::Healthy
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const TICK: Duration = Duration::from_secs(5);

    /// Two clocks that advance together unless the test suspends the machine.
    struct Clocks {
        now: Instant,
        wall: SystemTime,
    }

    impl Clocks {
        fn new() -> Self {
            Self {
                now: Instant::now(),
                wall: SystemTime::UNIX_EPOCH + Duration::from_secs(1_800_000_000),
            }
        }

        fn awake(&mut self, by: Duration) {
            self.now += by;
            self.wall += by;
        }

        /// A suspend as Linux and macOS see it: only the wall clock moves.
        fn suspend(&mut self, by: Duration) {
            self.wall += by;
        }

        fn check(&self, watch: &mut LinkWatch, frames: Option<u64>) -> Verdict {
            watch.check(self.now, self.wall, frames)
        }
    }

    #[test]
    fn a_link_that_keeps_receiving_stays_healthy() {
        let mut clocks = Clocks::new();
        let mut watch = LinkWatch::default();
        for frames in 0..100 {
            assert_eq!(clocks.check(&mut watch, Some(frames)), Verdict::Healthy);
            clocks.awake(TICK);
        }
    }

    #[test]
    fn a_link_that_receives_nothing_is_reconnected_once_per_limit() {
        let mut clocks = Clocks::new();
        let mut watch = LinkWatch::default();
        assert_eq!(clocks.check(&mut watch, Some(7)), Verdict::Healthy);
        let mut waited = Duration::ZERO;
        while waited + TICK < SILENCE_LIMIT {
            clocks.awake(TICK);
            waited += TICK;
            assert_eq!(clocks.check(&mut watch, Some(7)), Verdict::Healthy);
        }
        clocks.awake(TICK);
        assert_eq!(
            clocks.check(&mut watch, Some(7)),
            Verdict::Silent(SILENCE_LIMIT)
        );
        // The reconnect gets a full limit of its own before the next verdict.
        clocks.awake(TICK);
        assert_eq!(clocks.check(&mut watch, Some(7)), Verdict::Healthy);
    }

    #[test]
    fn silence_is_only_counted_while_connected() {
        let mut clocks = Clocks::new();
        let mut watch = LinkWatch::default();
        assert_eq!(clocks.check(&mut watch, Some(3)), Verdict::Healthy);
        clocks.awake(SILENCE_LIMIT);
        assert_eq!(clocks.check(&mut watch, None), Verdict::Healthy);
        // Connecting again starts the count afresh, even at the same total.
        clocks.awake(TICK);
        assert_eq!(clocks.check(&mut watch, Some(3)), Verdict::Healthy);
        clocks.awake(SILENCE_LIMIT - TICK);
        assert_eq!(clocks.check(&mut watch, Some(3)), Verdict::Healthy);
    }

    #[test]
    fn a_suspend_is_noticed_on_the_first_tick_after_resume() {
        let mut clocks = Clocks::new();
        let mut watch = LinkWatch::default();
        assert_eq!(clocks.check(&mut watch, Some(1)), Verdict::Healthy);
        clocks.awake(TICK);
        clocks.suspend(Duration::from_secs(1_560));
        assert_eq!(
            clocks.check(&mut watch, Some(1)),
            Verdict::Slept(Duration::from_secs(1_560))
        );
        clocks.awake(TICK);
        assert_eq!(clocks.check(&mut watch, Some(1)), Verdict::Healthy);
    }

    #[test]
    fn a_short_suspend_or_a_small_clock_step_is_not_a_sleep() {
        let mut clocks = Clocks::new();
        let mut watch = LinkWatch::default();
        assert_eq!(clocks.check(&mut watch, Some(1)), Verdict::Healthy);
        clocks.awake(TICK);
        clocks.suspend(SLEEP_GAP - Duration::from_secs(1));
        assert_eq!(clocks.check(&mut watch, Some(2)), Verdict::Healthy);
        // The wall clock stepping back never reads as a sleep.
        clocks.awake(TICK);
        clocks.wall -= Duration::from_secs(3_600);
        assert_eq!(clocks.check(&mut watch, Some(3)), Verdict::Healthy);
    }

    #[test]
    fn a_slow_worker_is_not_a_sleep() {
        let mut clocks = Clocks::new();
        let mut watch = LinkWatch::default();
        assert_eq!(clocks.check(&mut watch, Some(1)), Verdict::Healthy);
        clocks.awake(Duration::from_secs(300));
        assert_eq!(clocks.check(&mut watch, Some(2)), Verdict::Healthy);
    }

    #[test]
    fn a_sleep_while_disconnected_leaves_the_library_to_reconnect() {
        let mut clocks = Clocks::new();
        let mut watch = LinkWatch::default();
        assert_eq!(clocks.check(&mut watch, None), Verdict::Healthy);
        clocks.suspend(Duration::from_secs(600));
        assert_eq!(clocks.check(&mut watch, None), Verdict::Healthy);
        clocks.awake(TICK);
        assert_eq!(clocks.check(&mut watch, Some(1)), Verdict::Healthy);
    }

    #[test]
    fn a_suspend_counted_by_the_monotonic_clock_is_caught_as_silence() {
        // Windows: `Instant` keeps counting through the suspend.
        let mut clocks = Clocks::new();
        let mut watch = LinkWatch::default();
        assert_eq!(clocks.check(&mut watch, Some(1)), Verdict::Healthy);
        clocks.awake(Duration::from_secs(1_560));
        assert_eq!(
            clocks.check(&mut watch, Some(1)),
            Verdict::Silent(Duration::from_secs(1_560))
        );
    }
}

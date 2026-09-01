//! Testable state and timing for the macOS HID tap safety watchdogs.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU8, AtomicU64, Ordering};
use std::time::{Duration, Instant};

pub(super) const CALLBACK_STUCK_BUDGET: Duration = Duration::from_millis(200);
pub(super) const TAP_SHUTDOWN_BUDGET: Duration = Duration::from_millis(1_500);
/// Poll cadence of the two watchdog threads, kept next to the budgets they
/// enforce because the scheduling-gap threshold below is derived from it.
pub(super) const CALLBACK_WATCHDOG_POLL_INTERVAL: Duration = Duration::from_millis(20);
pub(super) const LIFECYCLE_WATCHDOG_POLL_INTERVAL: Duration = Duration::from_millis(100);
/// A poll arriving this many intervals late means the watchdog thread itself
/// was off-CPU, not that the thread it observes stalled.
const SCHEDULING_GAP_POLLS: u32 = 5;
/// How many re-arms the hook grants inside [`REARM_WINDOW`] before it gives
/// the tap up.
pub(super) const REARM_LIMIT: u32 = 10;
/// The rolling window [`REARM_LIMIT`] applies to. Re-arming happens at most
/// once per run-loop slice, so a spent budget means the OS has been disabling
/// the tap for seconds on end.
pub(super) const REARM_WINDOW: Duration = Duration::from_secs(10);

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
#[repr(u8)]
pub(super) enum TapPhase {
    /// The tap thread has not begun CoreGraphics tap creation, so no tap can exist.
    Starting,
    /// The tap thread is creating or activating a tap that may already exist.
    Arming,
    Armed,
    TapStopped,
    ThreadExited,
}

impl TapPhase {
    const fn decode(value: u8) -> Self {
        if value > Self::ThreadExited as u8 {
            // An unknown byte cannot prove that the HID tap was destroyed.
            // Treat it as hazardous so both watchdogs remain armed and the
            // lifecycle timeout can fail safe instead of panicking or disarming.
            return Self::Armed;
        }
        match value {
            0 => Self::Starting,
            1 => Self::Arming,
            2 => Self::Armed,
            3 => Self::TapStopped,
            _ => Self::ThreadExited,
        }
    }
}

/// Whether the tap callback is idle or the monotonic millisecond when it entered.
///
/// Zero is idle; [`WatchdogSignals::now_millis`] reserves nonzero values for
/// entries. One Release store publishes each whole state, so an Acquire load
/// cannot observe "entered" without its matching timestamp as it could with
/// separate flag and timestamp atomics.
#[derive(Debug, Default)]
pub(super) struct CallbackActivity(AtomicU64);

impl CallbackActivity {
    pub fn enter(&self, entered_at_ms: u64) {
        debug_assert_ne!(entered_at_ms, 0);
        self.0.store(entered_at_ms, Ordering::Release);
    }

    pub fn exit(&self) {
        self.0.store(0, Ordering::Release);
    }

    pub fn entered_at_ms(&self) -> Option<u64> {
        match self.0.load(Ordering::Acquire) {
            0 => None,
            entered_at_ms => Some(entered_at_ms),
        }
    }
}

/// Atomics shared by the tap, stopper, and watchdog threads.
///
/// A stop request is a separate latch from `phase`: it must never imply that
/// the active HID tap has actually been detached.
#[derive(Debug)]
pub(super) struct WatchdogSignals {
    // `Instant` uses CLOCK_UPTIME_RAW on macOS: monotonic, and paused while
    // the system sleeps so resume cannot consume either watchdog budget.
    origin: Instant,
    phase: AtomicU8,
    stop_requested: AtomicBool,
    tap_progress_at_ms: AtomicU64,
}

impl Default for WatchdogSignals {
    fn default() -> Self {
        Self {
            origin: Instant::now(),
            phase: AtomicU8::new(TapPhase::Starting as u8),
            stop_requested: AtomicBool::new(false),
            tap_progress_at_ms: AtomicU64::new(0),
        }
    }
}

impl WatchdogSignals {
    pub fn now(&self) -> Duration {
        self.origin.elapsed()
    }

    pub fn now_millis(&self) -> u64 {
        u64::try_from(self.now().as_millis())
            .unwrap_or(u64::MAX - 1)
            .saturating_add(1)
    }

    pub fn phase(&self) -> TapPhase {
        TapPhase::decode(self.phase.load(Ordering::Acquire))
    }

    pub fn set_phase(&self, phase: TapPhase) {
        self.phase.store(phase as u8, Ordering::Release);
    }

    pub fn request_stop(&self) {
        self.stop_requested.store(true, Ordering::Release);
    }

    pub fn stop_requested(&self) -> bool {
        self.stop_requested.load(Ordering::Acquire)
    }

    pub fn mark_tap_progress(&self) {
        self.tap_progress_at_ms
            .store(self.now_millis(), Ordering::Release);
    }

    pub fn tap_progress_at(&self) -> Duration {
        Duration::from_millis(
            self.tap_progress_at_ms
                .load(Ordering::Acquire)
                .saturating_sub(1),
        )
    }

    pub fn thread_exit_guard(self: &Arc<Self>) -> TapThreadExitGuard {
        TapThreadExitGuard(Arc::clone(self))
    }
}

pub(super) struct TapThreadExitGuard(Arc<WatchdogSignals>);

impl Drop for TapThreadExitGuard {
    fn drop(&mut self) {
        self.0.set_phase(TapPhase::ThreadExited);
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct LifecycleObservation {
    pub phase: TapPhase,
    pub stop_requested: bool,
    pub tap_progress_at: Duration,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum LifecycleExitReason {
    TapThreadStalled,
    StopTimedOut,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum LifecycleDecision {
    Continue,
    Complete,
    Exit {
        reason: LifecycleExitReason,
        elapsed: Duration,
    },
}

/// Uptime that passed while the *watchdog* was not running is not evidence
/// against the thread it watches.
///
/// The clock behind `now` pauses only once the system is fully asleep. Across
/// sleep entry, every wake — dark wakes included, which no app-level
/// notification reports — and a debugger pause, it keeps counting for seconds
/// while no user thread is scheduled. A watchdog that then gets the CPU before
/// the tap thread reads a stale progress stamp and mistakes the gap for a
/// wedge: that is how a closed-lid dark wake exited an otherwise healthy agent
/// after 2.6 s of "stall". Accountability therefore restarts at the first
/// poll after a gap; a thread that is genuinely wedged still exits one full
/// budget later.
#[derive(Debug)]
pub(super) struct SchedulingGaps {
    gap: Duration,
    last_poll_at: Option<Duration>,
    accountable_from: Duration,
}

impl SchedulingGaps {
    pub fn new(poll_interval: Duration) -> Self {
        Self {
            gap: poll_interval * SCHEDULING_GAP_POLLS,
            last_poll_at: None,
            accountable_from: Duration::ZERO,
        }
    }

    /// Record a poll at `now`; returns the earliest instant the observed
    /// thread can be held accountable from.
    pub fn poll(&mut self, now: Duration) -> Duration {
        if let Some(last) = self.last_poll_at
            && now.saturating_sub(last) > self.gap
        {
            self.accountable_from = now;
        }
        self.last_poll_at = Some(now);
        self.accountable_from
    }
}

#[derive(Debug)]
pub(super) struct LifecycleWatchdog {
    stop_at: Option<Duration>,
    gaps: SchedulingGaps,
}

impl Default for LifecycleWatchdog {
    fn default() -> Self {
        Self {
            stop_at: None,
            gaps: SchedulingGaps::new(LIFECYCLE_WATCHDOG_POLL_INTERVAL),
        }
    }
}

impl LifecycleWatchdog {
    pub fn evaluate(
        &mut self,
        now: Duration,
        observation: LifecycleObservation,
    ) -> LifecycleDecision {
        // Every poll is recorded, whatever the phase: a gap that straddles a
        // phase change must still be seen.
        let accountable_from = self.gaps.poll(now);
        match observation.phase {
            TapPhase::Starting => return LifecycleDecision::Continue,
            TapPhase::ThreadExited => return LifecycleDecision::Complete,
            TapPhase::Arming | TapPhase::Armed | TapPhase::TapStopped => {}
        }

        if observation.stop_requested {
            self.stop_at.get_or_insert(now);
        }
        if observation.phase == TapPhase::TapStopped && !observation.stop_requested {
            return LifecycleDecision::Complete;
        }

        let timeout = if let Some(stopped) = self.stop_at {
            Some((LifecycleExitReason::StopTimedOut, stopped))
        } else if matches!(observation.phase, TapPhase::Arming | TapPhase::Armed) {
            Some((
                LifecycleExitReason::TapThreadStalled,
                observation.tap_progress_at,
            ))
        } else {
            None
        };
        let Some((reason, started)) = timeout else {
            return LifecycleDecision::Continue;
        };
        let elapsed = now.saturating_sub(started.max(accountable_from));
        if elapsed >= TAP_SHUTDOWN_BUDGET {
            LifecycleDecision::Exit { reason, elapsed }
        } else {
            LifecycleDecision::Continue
        }
    }
}

/// Bounded re-arming of a tap the OS has disabled.
///
/// `TapDisabledByUserInput` fires during ordinary heavy input and self-heals,
/// so a burst has to be re-armed or the hook goes deaf. A tap the OS disables
/// again slice after slice is a different animal: nothing is servicing it, and
/// re-enabling it keeps an active HID tap gating events it will never answer
/// for. Give the burst room, then stop fighting and let the tap go.
#[derive(Debug, Default)]
pub(super) struct RearmBudget {
    window_start: Option<Duration>,
    used: u32,
}

impl RearmBudget {
    /// Charge a re-arm at `now`; `false` once this window's budget is spent.
    pub fn allow(&mut self, now: Duration) -> bool {
        match self.window_start {
            Some(start) if now.saturating_sub(start) < REARM_WINDOW => self.used += 1,
            _ => {
                self.window_start = Some(now);
                self.used = 1;
            }
        }
        self.used <= REARM_LIMIT
    }
}

pub(super) fn stuck_callback(now_ms: u64, entered_at_ms: u64) -> Option<Duration> {
    let elapsed = Duration::from_millis(now_ms.saturating_sub(entered_at_ms));
    (elapsed >= CALLBACK_STUCK_BUDGET).then_some(elapsed)
}

/// A callback entry the watchdog has found over budget: the entry timestamp
/// it judged (so the caller can re-sample against it) and how long it has run.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct StuckCallback {
    pub entered_at_ms: u64,
    pub elapsed: Duration,
}

/// The stuck-callback rule with scheduling gaps discounted, in the same
/// millisecond currency as [`CallbackActivity`].
#[derive(Debug)]
pub(super) struct CallbackWatchdog {
    gaps: SchedulingGaps,
}

impl Default for CallbackWatchdog {
    fn default() -> Self {
        Self {
            gaps: SchedulingGaps::new(CALLBACK_WATCHDOG_POLL_INTERVAL),
        }
    }
}

impl CallbackWatchdog {
    /// Record a poll at `now_ms` and judge the callback entry, if any, that
    /// was observed with it.
    pub fn evaluate(&mut self, now_ms: u64, entered_at_ms: Option<u64>) -> Option<StuckCallback> {
        let accountable_from = self.gaps.poll(Duration::from_millis(now_ms));
        let accountable_from_ms = u64::try_from(accountable_from.as_millis()).unwrap_or(u64::MAX);
        let entered_at_ms = entered_at_ms?;
        stuck_callback(now_ms, entered_at_ms.max(accountable_from_ms)).map(|elapsed| {
            StuckCallback {
                entered_at_ms,
                elapsed,
            }
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tap_phase_decoder_is_total_and_fails_safe() {
        assert_eq!(TapPhase::decode(0), TapPhase::Starting);
        assert_eq!(TapPhase::decode(1), TapPhase::Arming);
        assert_eq!(TapPhase::decode(2), TapPhase::Armed);
        assert_eq!(TapPhase::decode(3), TapPhase::TapStopped);
        assert_eq!(TapPhase::decode(4), TapPhase::ThreadExited);
        assert_eq!(TapPhase::decode(5), TapPhase::Armed);
        assert_eq!(TapPhase::decode(u8::MAX), TapPhase::Armed);

        let signals = WatchdogSignals::default();
        signals.phase.store(u8::MAX, Ordering::Relaxed);
        assert_eq!(signals.phase(), TapPhase::Armed);
    }

    #[test]
    fn callback_activity_publishes_one_complete_state() {
        let activity = CallbackActivity::default();
        assert_eq!(activity.entered_at_ms(), None);

        activity.enter(42);
        assert_eq!(activity.entered_at_ms(), Some(42));

        activity.enter(43);
        assert_eq!(activity.entered_at_ms(), Some(43));

        activity.exit();
        assert_eq!(activity.entered_at_ms(), None);
    }

    fn observation(
        phase: TapPhase,
        stop_requested: bool,
        tap_progress_at: Duration,
    ) -> LifecycleObservation {
        LifecycleObservation {
            phase,
            stop_requested,
            tap_progress_at,
        }
    }

    /// Drive the watchdog on its real cadence from `from` up to but excluding
    /// `until`, asserting every poll lets the tap live.
    fn poll_until(
        watchdog: &mut LifecycleWatchdog,
        from: Duration,
        until: Duration,
        observation: LifecycleObservation,
    ) {
        let mut now = from;
        while now < until {
            assert_eq!(
                watchdog.evaluate(now, observation),
                LifecycleDecision::Continue,
                "poll at {now:?}"
            );
            now += LIFECYCLE_WATCHDOG_POLL_INTERVAL;
        }
    }

    #[test]
    fn armed_tap_stall_exits_at_budget_unless_tap_stops() {
        let stalled = observation(TapPhase::Armed, false, Duration::ZERO);
        let mut watchdog = LifecycleWatchdog::default();
        poll_until(&mut watchdog, Duration::ZERO, TAP_SHUTDOWN_BUDGET, stalled);
        assert_eq!(
            watchdog.evaluate(TAP_SHUTDOWN_BUDGET, stalled),
            LifecycleDecision::Exit {
                reason: LifecycleExitReason::TapThreadStalled,
                elapsed: TAP_SHUTDOWN_BUDGET,
            }
        );

        let mut completed = LifecycleWatchdog::default();
        poll_until(
            &mut completed,
            Duration::ZERO,
            Duration::from_millis(500),
            stalled,
        );
        assert_eq!(
            completed.evaluate(
                Duration::from_millis(500),
                observation(TapPhase::TapStopped, false, Duration::ZERO)
            ),
            LifecycleDecision::Complete
        );
    }

    #[test]
    fn tap_creation_or_activation_stall_exits_at_budget() {
        let arming = observation(TapPhase::Arming, false, Duration::ZERO);
        let mut watchdog = LifecycleWatchdog::default();
        poll_until(&mut watchdog, Duration::ZERO, TAP_SHUTDOWN_BUDGET, arming);
        assert_eq!(
            watchdog.evaluate(TAP_SHUTDOWN_BUDGET, arming),
            LifecycleDecision::Exit {
                reason: LifecycleExitReason::TapThreadStalled,
                elapsed: TAP_SHUTDOWN_BUDGET,
            }
        );
    }

    #[test]
    fn stop_requires_thread_exit_even_after_tap_stops() {
        let stopping = observation(TapPhase::Armed, true, Duration::ZERO);
        let tap_stopped = observation(TapPhase::TapStopped, true, Duration::ZERO);
        let mut watchdog = LifecycleWatchdog::default();
        poll_until(
            &mut watchdog,
            Duration::ZERO,
            Duration::from_millis(500),
            stopping,
        );
        poll_until(
            &mut watchdog,
            Duration::from_millis(500),
            TAP_SHUTDOWN_BUDGET,
            tap_stopped,
        );
        assert_eq!(
            watchdog.evaluate(TAP_SHUTDOWN_BUDGET, tap_stopped),
            LifecycleDecision::Exit {
                reason: LifecycleExitReason::StopTimedOut,
                elapsed: TAP_SHUTDOWN_BUDGET,
            }
        );

        let mut completed = LifecycleWatchdog::default();
        poll_until(
            &mut completed,
            Duration::ZERO,
            Duration::from_millis(500),
            stopping,
        );
        assert_eq!(
            completed.evaluate(
                Duration::from_millis(500),
                observation(TapPhase::ThreadExited, true, Duration::ZERO)
            ),
            LifecycleDecision::Complete
        );
    }

    #[test]
    fn a_scheduling_gap_restarts_the_stall_budget() {
        // A healthy tap marks progress every 500 ms slice. The watchdog
        // polls on cadence, then the machine enters sleep: the next poll
        // lands 2.6 s of uptime later (the closed-lid dark wake that exited
        // the agent) with the progress stamp from before the gap.
        let mut watchdog = LifecycleWatchdog::default();
        let before_sleep = observation(TapPhase::Armed, false, Duration::from_millis(400));
        poll_until(
            &mut watchdog,
            Duration::ZERO,
            Duration::from_millis(500),
            before_sleep,
        );
        let resumed_at = Duration::from_millis(500) + Duration::from_millis(2_600);
        assert_eq!(
            watchdog.evaluate(resumed_at, before_sleep),
            LifecycleDecision::Continue
        );
        // Accountability restarts at that poll: a tap thread that then never
        // marks progress still exits exactly one budget later …
        poll_until(
            &mut watchdog,
            resumed_at + LIFECYCLE_WATCHDOG_POLL_INTERVAL,
            resumed_at + TAP_SHUTDOWN_BUDGET,
            before_sleep,
        );
        assert_eq!(
            watchdog.evaluate(resumed_at + TAP_SHUTDOWN_BUDGET, before_sleep),
            LifecycleDecision::Exit {
                reason: LifecycleExitReason::TapThreadStalled,
                elapsed: TAP_SHUTDOWN_BUDGET,
            }
        );

        // … while one that marks progress after the gap is judged from that
        // newer stamp, as before.
        let mut healthy = LifecycleWatchdog::default();
        poll_until(
            &mut healthy,
            Duration::ZERO,
            Duration::from_millis(500),
            before_sleep,
        );
        assert_eq!(
            healthy.evaluate(resumed_at, before_sleep),
            LifecycleDecision::Continue
        );
        let after_wake = observation(
            TapPhase::Armed,
            false,
            resumed_at + Duration::from_millis(50),
        );
        poll_until(
            &mut healthy,
            resumed_at + LIFECYCLE_WATCHDOG_POLL_INTERVAL,
            after_wake.tap_progress_at + TAP_SHUTDOWN_BUDGET,
            after_wake,
        );
        assert_eq!(
            healthy.evaluate(after_wake.tap_progress_at + TAP_SHUTDOWN_BUDGET, after_wake),
            LifecycleDecision::Exit {
                reason: LifecycleExitReason::TapThreadStalled,
                elapsed: TAP_SHUTDOWN_BUDGET,
            }
        );
    }

    #[test]
    fn a_scheduling_gap_during_stop_restarts_the_stop_budget() {
        let stopping = observation(TapPhase::TapStopped, true, Duration::ZERO);
        let mut watchdog = LifecycleWatchdog::default();
        poll_until(
            &mut watchdog,
            Duration::ZERO,
            Duration::from_millis(300),
            stopping,
        );
        let resumed_at = Duration::from_millis(300) + Duration::from_secs(3);
        poll_until(
            &mut watchdog,
            resumed_at,
            resumed_at + TAP_SHUTDOWN_BUDGET,
            stopping,
        );
        assert_eq!(
            watchdog.evaluate(resumed_at + TAP_SHUTDOWN_BUDGET, stopping),
            LifecycleDecision::Exit {
                reason: LifecycleExitReason::StopTimedOut,
                elapsed: TAP_SHUTDOWN_BUDGET,
            }
        );
    }

    #[test]
    fn a_late_poll_within_jitter_is_not_a_gap() {
        // Four intervals late is scheduler jitter; five is a gap.
        let stalled = observation(TapPhase::Armed, false, Duration::ZERO);
        let mut watchdog = LifecycleWatchdog::default();
        assert_eq!(
            watchdog.evaluate(Duration::ZERO, stalled),
            LifecycleDecision::Continue
        );
        let jitter = LIFECYCLE_WATCHDOG_POLL_INTERVAL * SCHEDULING_GAP_POLLS;
        assert_eq!(
            watchdog.evaluate(jitter, stalled),
            LifecycleDecision::Continue
        );
        poll_until(
            &mut watchdog,
            jitter + LIFECYCLE_WATCHDOG_POLL_INTERVAL,
            TAP_SHUTDOWN_BUDGET,
            stalled,
        );
        assert_eq!(
            watchdog.evaluate(TAP_SHUTDOWN_BUDGET, stalled),
            LifecycleDecision::Exit {
                reason: LifecycleExitReason::TapThreadStalled,
                elapsed: TAP_SHUTDOWN_BUDGET,
            }
        );
    }

    #[test]
    fn callback_watchdog_discounts_scheduling_gaps() {
        let poll_ms = u64::try_from(CALLBACK_WATCHDOG_POLL_INTERVAL.as_millis()).unwrap();
        let budget_ms = u64::try_from(CALLBACK_STUCK_BUDGET.as_millis()).unwrap();

        // On cadence, an entry at 1 is stuck exactly at the budget.
        let mut watchdog = CallbackWatchdog::default();
        let mut now = 1;
        while now < 1 + budget_ms {
            assert_eq!(watchdog.evaluate(now, Some(1)), None, "poll at {now}");
            now += poll_ms;
        }
        assert_eq!(
            watchdog.evaluate(1 + budget_ms, Some(1)),
            Some(StuckCallback {
                entered_at_ms: 1,
                elapsed: CALLBACK_STUCK_BUDGET,
            })
        );

        // An entry cut short by sleep is judged from the poll after the gap.
        let mut watchdog = CallbackWatchdog::default();
        assert_eq!(watchdog.evaluate(1, None), None);
        assert_eq!(watchdog.evaluate(1 + poll_ms, Some(5)), None);
        let resumed = 1 + poll_ms + 2_000;
        assert_eq!(watchdog.evaluate(resumed, Some(5)), None);
        let mut now = resumed + poll_ms;
        while now < resumed + budget_ms {
            assert_eq!(watchdog.evaluate(now, Some(5)), None, "poll at {now}");
            now += poll_ms;
        }
        assert_eq!(
            watchdog.evaluate(resumed + budget_ms, Some(5)),
            Some(StuckCallback {
                entered_at_ms: 5,
                elapsed: CALLBACK_STUCK_BUDGET,
            })
        );
    }

    #[test]
    fn starting_and_healthy_states_never_time_out() {
        let mut watchdog = LifecycleWatchdog::default();
        let starting = observation(TapPhase::Starting, false, Duration::ZERO);
        assert_eq!(
            watchdog.evaluate(Duration::ZERO, starting),
            LifecycleDecision::Continue
        );
        assert_eq!(
            watchdog.evaluate(TAP_SHUTDOWN_BUDGET * 2, starting),
            LifecycleDecision::Continue
        );
        assert_eq!(
            watchdog.evaluate(
                TAP_SHUTDOWN_BUDGET * 3,
                observation(TapPhase::Armed, false, Duration::from_secs(4))
            ),
            LifecycleDecision::Continue
        );
        assert_eq!(
            watchdog.evaluate(
                TAP_SHUTDOWN_BUDGET * 4,
                observation(TapPhase::ThreadExited, false, Duration::ZERO)
            ),
            LifecycleDecision::Complete
        );
    }

    #[test]
    fn a_burst_of_re_arms_is_allowed_and_then_bounded() {
        let mut budget = RearmBudget::default();
        for i in 0..REARM_LIMIT {
            assert!(
                budget.allow(Duration::from_millis(u64::from(i) * 500)),
                "re-arm {i} is within the budget"
            );
        }
        assert!(!budget.allow(Duration::from_millis(u64::from(REARM_LIMIT) * 500)));
    }

    #[test]
    fn a_new_window_restores_the_budget() {
        let mut budget = RearmBudget::default();
        for _ in 0..=REARM_LIMIT {
            let _ = budget.allow(Duration::ZERO);
        }
        assert!(!budget.allow(Duration::ZERO));
        // A re-arm past the window opens a fresh one, anchored at that re-arm
        // rather than at the first one ever charged.
        for _ in 0..REARM_LIMIT {
            assert!(budget.allow(REARM_WINDOW));
        }
        let just_inside = REARM_WINDOW + REARM_WINDOW.saturating_sub(Duration::from_millis(1));
        assert!(!budget.allow(just_inside));
    }

    #[test]
    fn callback_timeout_keeps_the_200ms_boundary() {
        assert_eq!(stuck_callback(200, 1), None);
        assert_eq!(stuck_callback(201, 1), Some(CALLBACK_STUCK_BUDGET));
        // A fresh high-frequency event must not inherit an older entry time.
        assert_eq!(stuck_callback(10_000, 9_801), None);
    }
}

//! Which channel to capture next.
//!
//! The decoder budget is one capture at a time and a capture costs seconds,
//! while focus moves in tens of milliseconds. Picking the wrong row is
//! therefore not a small waste: it is the whole rotation parked behind a row
//! the user has already scrolled past. That reasoning is the riskiest part of
//! the preview feature and the least observable on a device — a Fire TV gives
//! you a black rectangle, not a trace — so it is isolated here as a pure
//! function of state. `now` is a parameter rather than a clock read precisely
//! so the awkward cases are a table test that runs anywhere.

use super::ChannelId;
use std::time::{Duration, Instant};

/// What the guide is showing right now.
///
/// The guide virtualizes, so this describes the rendered window rather than
/// the channel list. It is built from the range `ScrollArea::show_rows`
/// computes, so there is one notion of "on screen" rather than two that drift.
#[derive(Debug, Clone, Default)]
pub struct ViewState {
    /// Channel ids currently rendered, in row order.
    pub visible: Vec<ChannelId>,
    /// Index into `visible` of the focused row, if any.
    pub focused: Option<usize>,
}

/// What the scheduler is allowed to know about the frame cache.
///
/// Deliberately narrow. The cache owns textures, an LRU and a capture pipeline;
/// the policy needs none of that to choose a row, and depending on it would
/// drag platform types into core.
pub trait CacheView {
    /// When this channel's cached frame was captured, if it has one.
    fn captured_at(&self, id: &str) -> Option<Instant>;
    /// Whether a capture for this channel is already running.
    fn in_flight(&self, id: &str) -> bool;
    /// Consecutive failures, for back-off. 0 when healthy.
    fn failures(&self, id: &str) -> u32;
}

/// Tuning for [`Scheduler`]. See [`SchedulerConfig::default`] for the budget
/// the defaults encode.
#[derive(Debug, Clone)]
pub struct SchedulerConfig {
    /// Don't recapture anything newer than this.
    pub fresh_ttl: Duration,
    /// Rows either side of focus that count as "next".
    pub prefetch_radius: usize,
    /// Concurrent captures allowed across the visible window.
    pub max_in_flight: usize,
    /// How long focus must hold still before the focused row is targeted.
    pub focus_settle: Duration,
    /// Base back-off after a failed capture; doubles per consecutive failure.
    pub failure_backoff: Duration,
}

impl Default for SchedulerConfig {
    /// Sized for the Fire TV Stick, which is the constraint everywhere else is
    /// comfortably inside.
    ///
    /// Five rows fit its 540 logical points, so a radius of 2 bounds the
    /// working set to the nine channels the user can plausibly reach next.
    /// `max_in_flight` is 1 because the device generally permits a single
    /// concurrent Widevine session, and the focused player already holds one.
    fn default() -> Self {
        Self {
            fresh_ttl: Duration::from_secs(30),
            prefetch_radius: 2,
            max_in_flight: 1,
            // The spec's 300-500ms band; a D-pad repeat is ~100ms, so this
            // clears several repeats without being perceptible on a deliberate
            // single press.
            focus_settle: Duration::from_millis(400),
            failure_backoff: Duration::from_secs(30),
        }
    }
}

/// Ordering key for one candidate row. Field order *is* the priority order:
/// the derived `Ord` compares lexicographically, and lowest wins.
#[derive(Debug, PartialEq, Eq, PartialOrd, Ord)]
struct Priority {
    /// 0 focused, 1 neighbour within the prefetch radius, 2 everything else.
    tier: u8,
    /// `None` (never captured) sorts before every `Some`, which is exactly
    /// "missing beats stale"; among `Some`, ascending is oldest-first.
    captured_at: Option<Instant>,
    /// Rows from focus. Zero outside the neighbour tier, where it carries no
    /// meaning and must not perturb the ordering.
    distance: usize,
    /// Final tie-break, so an equidistant pair above and below focus resolves
    /// to the upper row rather than to hash order.
    index: usize,
}

/// Capture-priority policy. Owns no clock and performs no I/O; the only state
/// it keeps is when focus last moved.
#[derive(Debug, Clone)]
pub struct Scheduler {
    config: SchedulerConfig,
    /// `None` means focus has not moved since the scheduler was built, which
    /// counts as settled — otherwise a guide opened on a focused row would
    /// never capture it.
    last_focus_change: Option<Instant>,
}

impl Scheduler {
    pub fn new(config: SchedulerConfig) -> Self {
        Self {
            config,
            last_focus_change: None,
        }
    }

    /// Call when the focused row changes; starts the settle timer.
    pub fn note_focus_change(&mut self, now: Instant) {
        self.last_focus_change = Some(now);
    }

    /// The channel to capture next, or `None` to stay idle.
    ///
    /// Idle is a real answer, not a failure: burning the decoder on a row that
    /// already has a usable frame costs the focused player the stutter the
    /// whole invariant exists to prevent.
    pub fn next_target(
        &self,
        view: &ViewState,
        cache: &dyn CacheView,
        now: Instant,
    ) -> Option<ChannelId> {
        // Only visible rows are counted. A capture for a channel that has
        // scrolled away is the previous window's business and will drain on
        // its own; blocking on it would leave the guide permanently idle.
        let in_flight = view.visible.iter().filter(|id| cache.in_flight(id)).count();
        if in_flight >= self.config.max_in_flight {
            return None;
        }

        // A focus index past the end of the window is a torn frame between the
        // guide and the scheduler, not a reason to panic. Treat it as no focus.
        let focus = view.focused.filter(|&i| i < view.visible.len());
        let focus_settled = match self.last_focus_change {
            Some(changed_at) => {
                now.saturating_duration_since(changed_at) >= self.config.focus_settle
            }
            None => true,
        };

        let mut best: Option<(Priority, &str)> = None;
        for (index, id) in view.visible.iter().enumerate() {
            if !self.is_candidate(id, cache, now) {
                continue;
            }

            let (tier, distance) = match focus {
                Some(f) if f == index => {
                    // Holding the D-pad moves focus about ten rows per second
                    // while a capture costs seconds. Targeting a focused row
                    // that is still moving means the capture is obsolete before
                    // it lands, every time, during exactly the interaction this
                    // feature exists for. The cache serves the old frame
                    // meanwhile. Note this gates the focused row only: the rest
                    // of the window is still worth prefetching.
                    if !focus_settled {
                        continue;
                    }
                    (0, 0)
                }
                Some(f) => {
                    let distance = f.abs_diff(index);
                    if distance <= self.config.prefetch_radius {
                        (1, distance)
                    } else {
                        (2, 0)
                    }
                }
                None => (2, 0),
            };

            let priority = Priority {
                tier,
                captured_at: cache.captured_at(id),
                distance,
                index,
            };
            if best.as_ref().is_none_or(|(b, _)| priority < *b) {
                best = Some((priority, id));
            }
        }

        best.map(|(_, id)| id.to_string())
    }

    /// Whether this row is worth capturing at all, before priority is
    /// considered.
    fn is_candidate(&self, id: &str, cache: &dyn CacheView, now: Instant) -> bool {
        if cache.in_flight(id) {
            return false;
        }

        let Some(captured_at) = cache.captured_at(id) else {
            // Never captured: nothing to be fresh, and no timestamp to run a
            // back-off from either. A channel that keeps failing before it ever
            // yields a frame therefore stays eligible — `CacheView` exposes no
            // last-attempt time, and inventing one here would be guessing.
            return true;
        };

        let age = now.saturating_duration_since(captured_at);
        if age < self.config.fresh_ttl {
            return false;
        }

        match backoff_window(self.config.failure_backoff, cache.failures(id)) {
            Some(window) => age >= window,
            None => true,
        }
    }
}

/// How long a channel stays benched after `failures` consecutive failures, or
/// `None` while it is healthy.
///
/// Saturates rather than panicking: `Duration` multiplication overflows on the
/// way to a channel's thirtieth failure, and a channel that has failed thirty
/// times is not coming back this session anyway.
fn backoff_window(base: Duration, failures: u32) -> Option<Duration> {
    if failures == 0 {
        return None;
    }
    let factor = 1u32.checked_shl(failures - 1).unwrap_or(u32::MAX);
    Some(base.checked_mul(factor).unwrap_or(Duration::MAX))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::{HashMap, HashSet};

    #[derive(Default)]
    struct FakeCache {
        captured: HashMap<String, Instant>,
        in_flight: HashSet<String>,
        failures: HashMap<String, u32>,
    }

    impl CacheView for FakeCache {
        fn captured_at(&self, id: &str) -> Option<Instant> {
            self.captured.get(id).copied()
        }
        fn in_flight(&self, id: &str) -> bool {
            self.in_flight.contains(id)
        }
        fn failures(&self, id: &str) -> u32 {
            self.failures.get(id).copied().unwrap_or(0)
        }
    }

    /// Distance from the fake clock's origin to `now`. Every age in a case is
    /// measured back from `now` and must stay under this, so the fixtures only
    /// ever add to an `Instant` — subtracting from `Instant::now()` is not
    /// portable and CI runs Windows and macOS too.
    const BASE_MS: u64 = 10_000_000;

    #[derive(Default)]
    struct Case {
        name: &'static str,
        config: SchedulerConfig,
        visible: Vec<&'static str>,
        focused: Option<usize>,
        /// `(channel, milliseconds ago the frame was captured)`.
        captured: Vec<(&'static str, u64)>,
        in_flight: Vec<&'static str>,
        /// `(channel, consecutive failures)`.
        failures: Vec<(&'static str, u32)>,
        /// Milliseconds since focus last moved; `None` means it never has.
        focus_changed_ago_ms: Option<u64>,
        want: Option<&'static str>,
    }

    fn run(cases: Vec<Case>) {
        let t0 = Instant::now();
        let now = t0 + Duration::from_millis(BASE_MS);
        let ago = |ms: u64| t0 + Duration::from_millis(BASE_MS - ms);

        for case in cases {
            let mut cache = FakeCache::default();
            for (id, age_ms) in &case.captured {
                cache.captured.insert((*id).to_string(), ago(*age_ms));
            }
            for id in &case.in_flight {
                cache.in_flight.insert((*id).to_string());
            }
            for (id, n) in &case.failures {
                cache.failures.insert((*id).to_string(), *n);
            }

            let mut scheduler = Scheduler::new(case.config.clone());
            if let Some(age_ms) = case.focus_changed_ago_ms {
                scheduler.note_focus_change(ago(age_ms));
            }

            let view = ViewState {
                visible: case.visible.iter().map(|s| (*s).to_string()).collect(),
                focused: case.focused,
            };

            assert_eq!(
                scheduler.next_target(&view, &cache, now).as_deref(),
                case.want,
                "case `{}`",
                case.name
            );
        }
    }

    const FRESH: u64 = 0;
    const STALE: u64 = 60_000;

    #[test]
    fn tiers_run_focused_then_neighbours_then_the_rest() {
        run(vec![
            Case {
                name: "the focused row wins outright when its frame is missing",
                visible: vec!["a", "b", "c", "d", "e"],
                focused: Some(2),
                want: Some("c"),
                ..Default::default()
            },
            Case {
                name: "a fresh focused row hands the slot to its nearest neighbour",
                visible: vec!["a", "b", "c", "d", "e"],
                focused: Some(2),
                captured: vec![("c", FRESH)],
                want: Some("b"),
                ..Default::default()
            },
            Case {
                name: "equidistant neighbours resolve to the lower index",
                visible: vec!["a", "b", "c", "d", "e"],
                focused: Some(2),
                // Only the equidistant pair is left in the running.
                captured: vec![("a", FRESH), ("c", FRESH), ("e", FRESH)],
                want: Some("b"),
                ..Default::default()
            },
            Case {
                name: "a stale neighbour outranks a missing row beyond the radius",
                visible: vec!["a", "b", "c", "d", "e", "f", "g"],
                focused: Some(3),
                // Every neighbour but "c" is fresh, leaving one stale
                // neighbour against two missing outsiders.
                captured: vec![
                    ("b", FRESH),
                    ("d", FRESH),
                    ("e", FRESH),
                    ("f", FRESH),
                    ("c", STALE),
                ],
                // Tier beats missing-over-stale: "a" has no frame at all but is
                // four rows away, so the user is far likelier to reach "c".
                want: Some("c"),
                ..Default::default()
            },
            Case {
                name: "rows beyond the radius are taken in index order",
                visible: vec!["a", "b", "c", "d", "e", "f", "g"],
                focused: Some(3),
                captured: vec![
                    ("b", FRESH),
                    ("c", FRESH),
                    ("d", FRESH),
                    ("e", FRESH),
                    ("f", FRESH),
                ],
                want: Some("a"),
                ..Default::default()
            },
            Case {
                name: "with no focus every visible row is one tier, in index order",
                visible: vec!["a", "b", "c"],
                focused: None,
                want: Some("a"),
                ..Default::default()
            },
            Case {
                name: "a zero radius leaves every other row in the last tier",
                config: SchedulerConfig {
                    prefetch_radius: 0,
                    ..SchedulerConfig::default()
                },
                visible: vec!["a", "b", "c", "d", "e"],
                focused: Some(2),
                captured: vec![("c", FRESH)],
                // The default radius would have answered "b".
                want: Some("a"),
                ..Default::default()
            },
        ]);
    }

    #[test]
    fn missing_beats_stale_and_stale_goes_oldest_first() {
        run(vec![
            Case {
                name: "a missing frame outranks any stale one",
                visible: vec!["a", "b", "c"],
                focused: None,
                captured: vec![("a", 60_000), ("c", 90_000)],
                want: Some("b"),
                ..Default::default()
            },
            Case {
                name: "among stale frames the oldest goes first",
                visible: vec!["a", "b", "c"],
                focused: None,
                captured: vec![("a", 40_000), ("b", 120_000), ("c", 60_000)],
                want: Some("b"),
                ..Default::default()
            },
            Case {
                name: "a missing far neighbour beats a stale near one",
                visible: vec!["a", "b", "c", "d", "e"],
                focused: Some(2),
                captured: vec![("a", FRESH), ("b", STALE), ("c", FRESH), ("d", FRESH)],
                // "b" is adjacent and "e" two rows out, but missing wins inside
                // a tier: a row with no frame at all is showing a bare logo.
                want: Some("e"),
                ..Default::default()
            },
            Case {
                name: "an older stale far neighbour beats a newer stale near one",
                visible: vec!["a", "b", "c", "d", "e"],
                focused: Some(2),
                captured: vec![
                    ("a", FRESH),
                    ("b", 40_000),
                    ("c", FRESH),
                    ("d", FRESH),
                    ("e", 200_000),
                ],
                want: Some("e"),
                ..Default::default()
            },
            Case {
                name: "identical ages fall back to the lower index",
                visible: vec!["a", "b", "c"],
                focused: None,
                captured: vec![("a", STALE), ("b", STALE), ("c", STALE)],
                want: Some("a"),
                ..Default::default()
            },
        ]);
    }

    #[test]
    fn in_flight_and_freshness_take_rows_out_of_the_running() {
        run(vec![
            Case {
                name: "an in-flight row is never the target",
                config: SchedulerConfig {
                    max_in_flight: 2,
                    ..SchedulerConfig::default()
                },
                visible: vec!["a", "b", "c"],
                focused: None,
                in_flight: vec!["a"],
                want: Some("b"),
                ..Default::default()
            },
            Case {
                name: "a saturated pipeline schedules nothing",
                visible: vec!["a", "b", "c"],
                focused: None,
                in_flight: vec!["a"],
                want: None,
                ..Default::default()
            },
            Case {
                name: "captures for rows that scrolled away do not hold the budget",
                visible: vec!["a", "b"],
                focused: None,
                in_flight: vec!["z"],
                want: Some("a"),
                ..Default::default()
            },
            Case {
                name: "a frame inside the ttl is left alone",
                visible: vec!["a"],
                focused: None,
                captured: vec![("a", 29_999)],
                want: None,
                ..Default::default()
            },
            Case {
                name: "the ttl boundary is inclusive: exactly ttl old is stale",
                visible: vec!["a"],
                focused: None,
                captured: vec![("a", 30_000)],
                want: Some("a"),
                ..Default::default()
            },
            Case {
                name: "an entirely fresh window is idle, not busy",
                visible: vec!["a", "b", "c"],
                focused: Some(1),
                captured: vec![("a", 1_000), ("b", 1_000), ("c", 1_000)],
                want: None,
                ..Default::default()
            },
            Case {
                name: "a zero budget never schedules",
                config: SchedulerConfig {
                    max_in_flight: 0,
                    ..SchedulerConfig::default()
                },
                visible: vec!["a", "b"],
                focused: Some(0),
                want: None,
                ..Default::default()
            },
        ]);
    }

    #[test]
    fn the_settle_debounce_gates_the_focused_row_and_nothing_else() {
        run(vec![
            Case {
                name: "inside the settle window the focused row is passed over",
                visible: vec!["a", "b", "c"],
                focused: Some(1),
                focus_changed_ago_ms: Some(100),
                want: Some("a"),
                ..Default::default()
            },
            Case {
                name: "one millisecond short of the window still passes it over",
                visible: vec!["a", "b", "c"],
                focused: Some(1),
                focus_changed_ago_ms: Some(399),
                want: Some("a"),
                ..Default::default()
            },
            Case {
                name: "the settle boundary is inclusive",
                visible: vec!["a", "b", "c"],
                focused: Some(1),
                focus_changed_ago_ms: Some(400),
                want: Some("b"),
                ..Default::default()
            },
            Case {
                name: "once focus has held still the focused row is targeted",
                visible: vec!["a", "b", "c"],
                focused: Some(1),
                focus_changed_ago_ms: Some(2_000),
                want: Some("b"),
                ..Default::default()
            },
            Case {
                name: "focus that has never moved counts as settled",
                visible: vec!["a", "b", "c"],
                focused: Some(1),
                focus_changed_ago_ms: None,
                want: Some("b"),
                ..Default::default()
            },
            Case {
                name: "an unsettled focus still lets the rest of the window prefetch",
                visible: vec!["a", "b", "c", "d", "e", "f", "g"],
                focused: Some(3),
                // Every neighbour is fresh, so the only candidates left are the
                // gated focused row and a row outside the radius.
                captured: vec![
                    ("b", FRESH),
                    ("c", FRESH),
                    ("e", FRESH),
                    ("f", FRESH),
                    ("g", FRESH),
                ],
                focus_changed_ago_ms: Some(100),
                want: Some("a"),
                ..Default::default()
            },
        ]);
    }

    #[test]
    fn failure_backoff_doubles_and_runs_from_the_last_capture() {
        run(vec![
            Case {
                name: "a stale frame with a healthy channel is eligible",
                visible: vec!["a"],
                focused: None,
                captured: vec![("a", 40_000)],
                failures: vec![("a", 0)],
                want: Some("a"),
                ..Default::default()
            },
            Case {
                name: "one failure benches for the base back-off",
                visible: vec!["a"],
                focused: None,
                captured: vec![("a", 30_000)],
                failures: vec![("a", 1)],
                // Base back-off equals the default ttl, so a single failure adds
                // nothing here; the doubling is what bites.
                want: Some("a"),
                ..Default::default()
            },
            Case {
                name: "two failures bench for twice the base",
                visible: vec!["a"],
                focused: None,
                captured: vec![("a", 40_000)],
                failures: vec![("a", 2)],
                want: None,
                ..Default::default()
            },
            Case {
                name: "and release at exactly twice the base",
                visible: vec!["a"],
                focused: None,
                captured: vec![("a", 60_000)],
                failures: vec![("a", 2)],
                want: Some("a"),
                ..Default::default()
            },
            Case {
                name: "three failures double again",
                visible: vec!["a"],
                focused: None,
                captured: vec![("a", 100_000)],
                failures: vec![("a", 3)],
                want: None,
                ..Default::default()
            },
            Case {
                name: "and release at four times the base",
                visible: vec!["a"],
                focused: None,
                captured: vec![("a", 120_000)],
                failures: vec![("a", 3)],
                want: Some("a"),
                ..Default::default()
            },
            Case {
                name: "a benched row does not block a healthy one",
                visible: vec!["a", "b"],
                focused: None,
                captured: vec![("a", 40_000), ("b", 50_000)],
                failures: vec![("a", 2)],
                want: Some("b"),
                ..Default::default()
            },
            Case {
                name: "an absurd failure count saturates instead of overflowing",
                visible: vec!["a"],
                focused: None,
                captured: vec![("a", 40_000)],
                failures: vec![("a", 40)],
                want: None,
                ..Default::default()
            },
            Case {
                name: "back-off cannot bench a channel that never captured",
                visible: vec!["a"],
                focused: None,
                failures: vec![("a", 5)],
                // Known gap, chosen not overlooked: the back-off is measured
                // from the last capture and `CacheView` offers no other
                // timestamp, so a channel that has never produced a frame has
                // nothing to count from and stays eligible.
                want: Some("a"),
                ..Default::default()
            },
        ]);
    }

    #[test]
    fn degenerate_views_are_answered_rather_than_panicked_on() {
        run(vec![
            Case {
                name: "an empty window schedules nothing",
                visible: vec![],
                focused: None,
                want: None,
                ..Default::default()
            },
            Case {
                name: "an empty window with a focus index schedules nothing",
                visible: vec![],
                focused: Some(0),
                want: None,
                ..Default::default()
            },
            Case {
                name: "a focus index past the end is treated as no focus",
                visible: vec!["a", "b"],
                focused: Some(7),
                want: Some("a"),
                ..Default::default()
            },
            Case {
                name: "a radius wider than the window does not panic",
                config: SchedulerConfig {
                    prefetch_radius: 10,
                    ..SchedulerConfig::default()
                },
                visible: vec!["a", "b", "c"],
                focused: Some(1),
                captured: vec![("b", FRESH)],
                want: Some("a"),
                ..Default::default()
            },
            Case {
                name: "a radius wider than the window still prefers nearer rows",
                config: SchedulerConfig {
                    prefetch_radius: 10,
                    ..SchedulerConfig::default()
                },
                visible: vec!["a", "b", "c", "d", "e"],
                focused: Some(2),
                captured: vec![("c", FRESH), ("b", STALE)],
                // "a", "d" and "e" are all missing, so distance decides and the
                // adjacent row wins even though "a" has the lower index.
                want: Some("d"),
                ..Default::default()
            },
            Case {
                name: "a single row that is also the focused one",
                visible: vec!["a"],
                focused: Some(0),
                want: Some("a"),
                ..Default::default()
            },
        ]);
    }

    #[test]
    fn the_default_budget_is_the_one_the_design_specifies() {
        let c = SchedulerConfig::default();
        assert_eq!(c.fresh_ttl, Duration::from_secs(30));
        assert_eq!(c.prefetch_radius, 2);
        assert_eq!(c.max_in_flight, 1);
        assert_eq!(c.focus_settle, Duration::from_millis(400));
        assert_eq!(c.failure_backoff, Duration::from_secs(30));
    }

    #[test]
    fn each_focus_change_re_arms_the_settle_timer() {
        let t0 = Instant::now();
        let cache = FakeCache::default();
        let view = ViewState {
            visible: vec!["a".into(), "b".into(), "c".into()],
            focused: Some(1),
        };

        let mut scheduler = Scheduler::new(SchedulerConfig::default());
        scheduler.note_focus_change(t0);

        // Settled, so the focused row is the target.
        let settled = t0 + Duration::from_millis(500);
        assert_eq!(
            scheduler.next_target(&view, &cache, settled).as_deref(),
            Some("b")
        );

        // A second press restarts the timer rather than leaving it expired,
        // which is the whole point: a D-pad repeat must keep pushing it out.
        scheduler.note_focus_change(settled);
        assert_eq!(
            scheduler
                .next_target(&view, &cache, settled + Duration::from_millis(100))
                .as_deref(),
            Some("a"),
            "focus moved 100ms ago, so the focused row is gated again"
        );
        assert_eq!(
            scheduler
                .next_target(&view, &cache, settled + Duration::from_millis(400))
                .as_deref(),
            Some("b")
        );
    }
}

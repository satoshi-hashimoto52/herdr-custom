//! Host disk and swap readings for the sidebar footer.
//!
//! Sampling happens on the app's existing deadline loop rather than on a timer
//! of its own, and only while the expanded sidebar can actually show the
//! footer. A collapsed sidebar and the mobile layout both leave
//! [`App::next_system_resource_deadline`] empty, so neither wakes the loop to
//! compute a reading nobody will see.
//!
//! The readings keep separate cadences. CPU and memory move continuously and
//! are sampled every few seconds; swap keeps its own quick beat; free disk
//! space moves in jumps — a build finishing, the Trash emptying — and is
//! re-read on a slower beat that announces itself with a short-lived marker.
//! One platform call answers all of them, so the slower beats cost no extra
//! syscall.

use std::time::{Duration, Instant};

use super::App;
use crate::platform::{CpuTicks, SystemResourceSample};

/// Swap sampling cadence. Slow enough to stay invisible in a CPU profile,
/// quick enough that memory pressure during a build is noticed.
const SWAP_REFRESH_INTERVAL: Duration = Duration::from_secs(4);

/// Free-space sampling cadence.
const DISK_REFRESH_INTERVAL: Duration = Duration::from_secs(30);

/// CPU and memory sampling cadence.
///
/// Both move continuously, so they need a beat a person reads as live without
/// making the number jitter on every frame. Three seconds is also slow enough
/// that the two kernel counters behind it cost nothing measurable.
const CPU_MEMORY_REFRESH_INTERVAL: Duration = Duration::from_secs(3);

/// How long a successful free-space refresh keeps its marker up.
const DISK_MARKER_VISIBLE: Duration = Duration::from_secs(5);

/// Decimal gigabyte, matching how `diskutil` and storage vendors report sizes.
const GB: u64 = 1_000_000_000;

/// Free-space bands. Below 20 GB a build or install is likely to fail outright,
/// which is the only case loud enough to warrant a critical marker.
const DISK_CRITICAL_GB: u64 = 20;
const DISK_HIGH_WARNING_GB: u64 = 30;
const DISK_WARNING_GB: u64 = 50;

/// Swap bands. Sustained swap means memory pressure is already hurting
/// throughput, so the first band starts well before thrashing.
const SWAP_WARNING_GB: u64 = 5;
const SWAP_HIGH_WARNING_GB: u64 = 10;

/// Pressure band for a host resource reading.
///
/// Bands pick an existing theme color rather than introducing new decoration,
/// so a healthy sidebar stays quiet and only `Critical` demands attention.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ResourcePressure {
    #[default]
    Normal,
    Warning,
    HighWarning,
    Critical,
}

/// One cached host resource reading and the band it falls in.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ResourceReading {
    pub bytes: u64,
    pub pressure: ResourcePressure,
}

/// What the disk row shows beside its reading.
///
/// The marker is decided when a sample lands rather than while drawing, so the
/// renderer stays a pure function of state and the tick expires on the same
/// deadline loop that raised it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum DiskRefreshMarker {
    /// Steady state: the reading speaks for itself.
    #[default]
    None,
    /// The last refresh succeeded and its marker has not timed out yet.
    Refreshed,
    /// The last refresh failed, so the reading beside it is the last good one.
    Stale,
}

/// Physical memory in use against physical memory installed.
///
/// Held as bytes rather than a formatted string so the renderer stays the only
/// place that decides how a number is written.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MemoryReading {
    pub used_bytes: u64,
    pub total_bytes: u64,
}

/// Cached host readings shown in the sidebar footer.
///
/// A reading stays `None` when the platform cannot report it. Disk and swap
/// hide their line entirely in that case; CPU and memory print `--`, because
/// they sit above two rows that are still there and a gap would read as a
/// layout fault rather than a missing number.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct SystemResources {
    pub disk_available: Option<ResourceReading>,
    pub swap_used: Option<ResourceReading>,
    pub disk_marker: DiskRefreshMarker,
    /// Whole-machine CPU load, 0..=100, or `None` before the first pair of
    /// samples and while the platform cannot answer.
    pub cpu_percent: Option<u8>,
    pub memory: Option<MemoryReading>,
    /// Whether this host reports CPU and memory at all. Platforms that never
    /// will leave the two rows out rather than printing a permanent `--`.
    pub cpu_memory_supported: bool,
}

/// Sampling schedule and marker timing for the sidebar footer.
///
/// Deadlines live beside the readings they drive, and every one of them is set
/// from an `Instant` handed in by the caller, so the marker's lifetime is
/// testable without waiting on a real clock.
#[derive(Debug, Default)]
pub(crate) struct SystemResourceSchedule {
    last_swap_sample: Option<Instant>,
    last_disk_sample: Option<Instant>,
    /// When a `Refreshed` marker stops being drawn. A `Stale` marker has no
    /// expiry: it holds until a sample succeeds.
    marker_expires_at: Option<Instant>,
    /// Set once a failing volume has been logged, so a volume that stays
    /// broken does not write the same line every refresh.
    disk_failure_logged: bool,
    last_cpu_memory_sample: Option<Instant>,
    /// The previous CPU counters, which is what makes a percentage possible:
    /// the kernel reports cumulative time, and a rate needs two readings.
    previous_cpu_ticks: Option<CpuTicks>,
}

/// Bands free disk space, where *less* space is worse.
fn disk_pressure(bytes: u64) -> ResourcePressure {
    let gb = bytes / GB;
    if gb < DISK_CRITICAL_GB {
        ResourcePressure::Critical
    } else if gb < DISK_HIGH_WARNING_GB {
        ResourcePressure::HighWarning
    } else if gb < DISK_WARNING_GB {
        ResourcePressure::Warning
    } else {
        ResourcePressure::Normal
    }
}

/// Bands swap in use, where *more* swap is worse.
fn swap_pressure(bytes: u64) -> ResourcePressure {
    let gb = bytes / GB;
    if gb >= SWAP_HIGH_WARNING_GB {
        ResourcePressure::HighWarning
    } else if gb >= SWAP_WARNING_GB {
        ResourcePressure::Warning
    } else {
        ResourcePressure::Normal
    }
}

/// Whole-machine CPU load between two readings, as a percentage.
///
/// `None` when the pair says nothing: a counter that ran backwards (the 32-bit
/// mach counters do wrap, and a machine can sleep through a reboot of them) or
/// two readings taken so close together that no tick elapsed. Neither is a
/// failure, so the caller keeps whatever it was already showing rather than
/// blinking a number in and out.
fn cpu_percent_between(previous: CpuTicks, current: CpuTicks) -> Option<u8> {
    let busy = current.busy.checked_sub(previous.busy)?;
    let total = current.total.checked_sub(previous.total)?;
    if total == 0 || busy > total {
        return None;
    }
    // Round half up on integers, then clamp: the ratio cannot exceed one, but
    // saying so here means no arithmetic surprise can print 101%.
    let percent = (busy.saturating_mul(100) + total / 2) / total;
    Some(percent.min(100) as u8)
}

impl SystemResources {
    /// Folds a CPU reading in against the previous counters.
    ///
    /// Returns the counters to remember for next time. A read that failed
    /// clears the figure to `--` and forgets the baseline, so the next
    /// successful pair starts fresh instead of measuring across the gap.
    fn set_cpu(
        &mut self,
        previous: Option<CpuTicks>,
        current: Option<CpuTicks>,
    ) -> Option<CpuTicks> {
        let Some(current) = current else {
            self.cpu_percent = None;
            return None;
        };
        self.cpu_memory_supported = true;
        if let Some(previous) = previous {
            if let Some(percent) = cpu_percent_between(previous, current) {
                self.cpu_percent = Some(percent);
            }
        }
        Some(current)
    }

    /// Folds a memory reading in, clearing to `--` when the read failed.
    fn set_memory(&mut self, memory: Option<crate::platform::MemoryUsage>) {
        self.memory = memory.map(|memory| {
            self.cpu_memory_supported = true;
            MemoryReading {
                used_bytes: memory.used_bytes.min(memory.total_bytes),
                total_bytes: memory.total_bytes,
            }
        });
    }

    /// Folds a swap sample in, leaving the disk side untouched.
    fn set_swap(&mut self, bytes: Option<u64>) {
        self.swap_used = bytes.map(|bytes| ResourceReading {
            bytes,
            pressure: swap_pressure(bytes),
        });
    }

    /// Folds a free-space sample in, reporting whether it succeeded.
    ///
    /// A failed sample keeps the last good reading rather than blanking the
    /// row: the number stops being current, not true, and the marker is what
    /// says so.
    fn set_disk(&mut self, bytes: Option<u64>) -> bool {
        let Some(bytes) = bytes else {
            // A volume that has never answered has no last good reading to
            // qualify, so the row stays hidden instead of gaining a lone
            // marker.
            if self.disk_available.is_some() {
                self.disk_marker = DiskRefreshMarker::Stale;
            }
            return false;
        };
        self.disk_available = Some(ResourceReading {
            bytes,
            pressure: disk_pressure(bytes),
        });
        self.disk_marker = DiskRefreshMarker::Refreshed;
        true
    }

    /// True when no reading is available, which hides the footer entirely.
    ///
    /// Disk and swap alone decide this. CPU and memory ride on top of those
    /// two rows and fall back to them when the sidebar is short, so a footer
    /// that could only show CPU and memory would have nothing to fall back to.
    pub(crate) fn is_empty(&self) -> bool {
        self.disk_available.is_none() && self.swap_used.is_none()
    }
}

/// Whether `interval` has elapsed since `last`, treating "never sampled" as due.
fn sample_due(last: Option<Instant>, now: Instant, interval: Duration) -> bool {
    last.is_none_or(|last| now.saturating_duration_since(last) >= interval)
}

impl App {
    /// Samples host resources when due, reporting whether the visible readout
    /// changed.
    ///
    /// Returns `false` on an unchanged sample so an idle host does not mark the
    /// frame dirty every few seconds. The footer therefore holds still instead
    /// of flickering while disk and swap sit where they were.
    pub(crate) fn handle_system_resource_refresh(&mut self, now: Instant) -> bool {
        self.refresh_system_resources(now, crate::platform::system_resource_sample)
    }

    /// The body of [`App::handle_system_resource_refresh`], with the platform
    /// read left as a parameter.
    ///
    /// Taking the sampler rather than calling it keeps the schedule and the
    /// marker states exercisable against synthetic readings — including the
    /// failures a healthy host will not produce on demand.
    fn refresh_system_resources(
        &mut self,
        now: Instant,
        sample: impl FnOnce() -> SystemResourceSample,
    ) -> bool {
        if !self.system_resource_readout_visible() {
            return false;
        }
        // Due-ness is compared against the caller's `now` rather than a
        // deadline this function reads itself: an "immediately due" deadline
        // taken from a later clock than `now` is never actually reached, which
        // left the first reading permanently pending.
        let schedule = &self.system_resource_schedule;
        let swap_due = sample_due(schedule.last_swap_sample, now, SWAP_REFRESH_INTERVAL);
        let disk_due = sample_due(schedule.last_disk_sample, now, DISK_REFRESH_INTERVAL);
        let cpu_memory_due = sample_due(
            schedule.last_cpu_memory_sample,
            now,
            CPU_MEMORY_REFRESH_INTERVAL,
        );
        let marker_due = schedule.marker_expires_at.is_some_and(|at| now >= at);
        if !swap_due && !disk_due && !cpu_memory_due && !marker_due {
            return false;
        }

        let before = self.state.system_resources;

        if marker_due {
            self.state.system_resources.disk_marker = DiskRefreshMarker::None;
            self.system_resource_schedule.marker_expires_at = None;
        }

        if swap_due || disk_due || cpu_memory_due {
            // One platform call answers every reading, so the slower disk beat
            // never costs a syscall of its own.
            let sample = sample();
            if cpu_memory_due {
                self.system_resource_schedule.last_cpu_memory_sample = Some(now);
                let previous = self.system_resource_schedule.previous_cpu_ticks;
                self.system_resource_schedule.previous_cpu_ticks = self
                    .state
                    .system_resources
                    .set_cpu(previous, sample.cpu_ticks);
                self.state.system_resources.set_memory(sample.memory);
            }
            if swap_due {
                self.system_resource_schedule.last_swap_sample = Some(now);
                self.state.system_resources.set_swap(sample.swap_used_bytes);
            }
            if disk_due {
                self.system_resource_schedule.last_disk_sample = Some(now);
                let succeeded = self
                    .state
                    .system_resources
                    .set_disk(sample.disk_available_bytes);
                self.note_disk_refresh_outcome(succeeded, now);
            }
        }

        self.state.system_resources != before
    }

    /// Times the marker a finished disk sample earns, and logs a failure once.
    fn note_disk_refresh_outcome(&mut self, succeeded: bool, now: Instant) {
        let schedule = &mut self.system_resource_schedule;
        if succeeded {
            // Re-reading the same number is still a successful refresh, so the
            // marker goes up whether or not the value moved, and a marker
            // already showing has its deadline pushed out from now.
            schedule.marker_expires_at = Some(now + DISK_MARKER_VISIBLE);
            schedule.disk_failure_logged = false;
            return;
        }
        // A stale reading holds its marker until a sample succeeds, so there is
        // nothing left to expire.
        schedule.marker_expires_at = None;
        if !schedule.disk_failure_logged {
            schedule.disk_failure_logged = true;
            tracing::warn!("failed to read free disk space; keeping the last reading");
        }
    }

    /// When the next sample or marker change is due, or `None` while the footer
    /// cannot be seen.
    pub(crate) fn next_system_resource_deadline(&self) -> Option<Instant> {
        if !self.system_resource_readout_visible() {
            return None;
        }
        let schedule = &self.system_resource_schedule;
        // Wake the loop straight away so the footer is populated by the time
        // the sidebar first draws.
        let next_sample = |last: Option<Instant>, interval| {
            last.map_or_else(Instant::now, |last| last + interval)
        };
        [
            Some(next_sample(
                schedule.last_swap_sample,
                SWAP_REFRESH_INTERVAL,
            )),
            Some(next_sample(
                schedule.last_disk_sample,
                DISK_REFRESH_INTERVAL,
            )),
            Some(next_sample(
                schedule.last_cpu_memory_sample,
                CPU_MEMORY_REFRESH_INTERVAL,
            )),
            schedule.marker_expires_at,
        ]
        .into_iter()
        .flatten()
        .min()
    }

    /// Whether an expanded desktop sidebar is currently showing the footer.
    ///
    /// A laid-out `sidebar_rect` is the precise signal: the mobile layout and a
    /// hidden collapsed sidebar both leave it zero-width, so neither pays for a
    /// reading it cannot draw. It is safe to depend on because view geometry is
    /// derived from the sidebar's configured width, never from the footer.
    fn system_resource_readout_visible(&self) -> bool {
        self.state.sidebar_resources.enabled
            && !self.state.sidebar_collapsed
            && self.state.view.sidebar_rect.width > 0
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_app() -> App {
        let (_api_tx, api_rx) = tokio::sync::mpsc::unbounded_channel();
        App::new(
            &crate::config::Config::default(),
            true,
            None,
            api_rx,
            crate::api::EventHub::default(),
        )
    }

    #[test]
    fn disk_bands_follow_free_space_thresholds() {
        let cases = [
            (250 * GB, ResourcePressure::Normal),
            (100 * GB, ResourcePressure::Normal),
            (50 * GB, ResourcePressure::Normal),
            (49 * GB, ResourcePressure::Warning),
            (30 * GB, ResourcePressure::Warning),
            (29 * GB, ResourcePressure::HighWarning),
            (20 * GB, ResourcePressure::HighWarning),
            (19 * GB, ResourcePressure::Critical),
            (0, ResourcePressure::Critical),
        ];

        for (bytes, expected) in cases {
            assert_eq!(disk_pressure(bytes), expected, "{bytes} bytes free");
        }
    }

    #[test]
    fn swap_bands_grow_with_usage() {
        let cases = [
            (0, ResourcePressure::Normal),
            (4 * GB, ResourcePressure::Normal),
            (5 * GB, ResourcePressure::Warning),
            (9 * GB, ResourcePressure::Warning),
            (10 * GB, ResourcePressure::HighWarning),
            (64 * GB, ResourcePressure::HighWarning),
        ];

        for (bytes, expected) in cases {
            assert_eq!(swap_pressure(bytes), expected, "{bytes} bytes swapped");
        }
    }

    /// A sample with the disk and swap readings present.
    fn sample(disk_bytes: Option<u64>) -> SystemResourceSample {
        SystemResourceSample {
            disk_available_bytes: disk_bytes,
            swap_used_bytes: Some(0),
            ..Default::default()
        }
    }

    fn secs(n: u64) -> Duration {
        Duration::from_secs(n)
    }

    /// A sample carrying every reading, so a test can drive one of them.
    fn full_sample(
        cpu: Option<CpuTicks>,
        memory: Option<crate::platform::MemoryUsage>,
    ) -> SystemResourceSample {
        SystemResourceSample {
            disk_available_bytes: Some(100 * GB),
            swap_used_bytes: Some(0),
            cpu_ticks: cpu,
            memory,
        }
    }

    fn ticks(busy: u64, total: u64) -> Option<CpuTicks> {
        Some(CpuTicks { busy, total })
    }

    fn memory(used_bytes: u64, total_bytes: u64) -> Option<crate::platform::MemoryUsage> {
        Some(crate::platform::MemoryUsage {
            used_bytes,
            total_bytes,
        })
    }

    /// An app whose sidebar is wide enough to want the footer.
    fn visible_app() -> App {
        let mut app = test_app();
        app.state.view.sidebar_rect = ratatui::layout::Rect::new(0, 0, 26, 40);
        app
    }

    #[test]
    fn cpu_needs_two_samples_before_it_can_say_anything() {
        let mut app = visible_app();
        let start = Instant::now();

        // One reading of a cumulative counter is not a rate.
        app.refresh_system_resources(start, || full_sample(ticks(1_000, 4_000), None));
        assert_eq!(app.state.system_resources.cpu_percent, None);

        // The second reading is measured against the first, not against zero.
        app.refresh_system_resources(start + secs(3), || full_sample(ticks(1_500, 6_000), None));
        assert_eq!(app.state.system_resources.cpu_percent, Some(25));
    }

    #[test]
    fn cpu_load_is_the_busy_share_of_the_whole_machine() {
        // Deltas, not absolutes: every case starts from the same baseline.
        let cases = [
            // (busy delta, total delta, percent)
            (0, 1_000, 0),
            (500, 1_000, 50),
            (1_000, 1_000, 100),
            // Rounding is half up, and to a whole percent.
            (4, 1_000, 0),
            (5, 1_000, 1),
            (144, 1_000, 14),
            (145, 1_000, 15),
            (995, 1_000, 100),
            // Eight cores fully busy is 100%, not 800%: the counters the
            // platform hands over are already summed across every CPU.
            (8_000, 8_000, 100),
            (1_000, 8_000, 13),
        ];
        for (busy, total, expected) in cases {
            let mut app = visible_app();
            let start = Instant::now();
            app.refresh_system_resources(start, || full_sample(ticks(10_000, 40_000), None));
            app.refresh_system_resources(start + secs(3), || {
                full_sample(ticks(10_000 + busy, 40_000 + total), None)
            });
            assert_eq!(
                app.state.system_resources.cpu_percent,
                Some(expected),
                "{busy}/{total}"
            );
            assert!(app
                .state
                .system_resources
                .cpu_percent
                .is_some_and(|p| p <= 100));
        }
    }

    #[test]
    fn a_counter_that_resets_or_stands_still_keeps_the_last_figure() {
        let mut app = visible_app();
        let start = Instant::now();
        app.refresh_system_resources(start, || full_sample(ticks(1_000, 4_000), None));
        app.refresh_system_resources(start + secs(3), || full_sample(ticks(1_500, 6_000), None));
        assert_eq!(app.state.system_resources.cpu_percent, Some(25));

        // Counters running backwards — a wrap of the 32-bit mach counters, or
        // a machine waking up — say nothing about the interval, so the figure
        // that is already up stays rather than blinking.
        app.refresh_system_resources(start + secs(6), || full_sample(ticks(10, 40), None));
        assert_eq!(app.state.system_resources.cpu_percent, Some(25));

        // And the reset reading becomes the new baseline, so the next pair
        // measures forward from it instead of across the gap.
        app.refresh_system_resources(start + secs(9), || full_sample(ticks(60, 140), None));
        assert_eq!(app.state.system_resources.cpu_percent, Some(50));

        // Two readings with no elapsed tick between them are the same story.
        app.refresh_system_resources(start + secs(12), || full_sample(ticks(60, 140), None));
        assert_eq!(app.state.system_resources.cpu_percent, Some(50));
    }

    #[test]
    fn a_busy_count_larger_than_the_total_is_refused() {
        // Not physically possible, but the counters are sampled as a group and
        // a garbled read must not print 400%.
        let mut app = visible_app();
        let start = Instant::now();
        app.refresh_system_resources(start, || full_sample(ticks(1_000, 4_000), None));
        app.refresh_system_resources(start + secs(3), || full_sample(ticks(5_000, 4_100), None));
        assert_eq!(app.state.system_resources.cpu_percent, None);
    }

    #[test]
    fn a_cpu_read_that_fails_shows_nothing_rather_than_a_stale_figure() {
        let mut app = visible_app();
        let start = Instant::now();
        app.refresh_system_resources(start, || full_sample(ticks(1_000, 4_000), None));
        app.refresh_system_resources(start + secs(3), || full_sample(ticks(1_500, 6_000), None));
        assert_eq!(app.state.system_resources.cpu_percent, Some(25));

        app.refresh_system_resources(start + secs(6), || full_sample(None, None));
        assert_eq!(app.state.system_resources.cpu_percent, None);

        // The failed read is not a baseline either, so the next single reading
        // still cannot produce a rate.
        app.refresh_system_resources(start + secs(9), || full_sample(ticks(9_000, 12_000), None));
        assert_eq!(app.state.system_resources.cpu_percent, None);
        app.refresh_system_resources(start + secs(12), || full_sample(ticks(9_500, 13_000), None));
        assert_eq!(app.state.system_resources.cpu_percent, Some(50));
    }

    #[test]
    fn memory_reports_what_is_in_use_against_what_is_installed() {
        let mut app = visible_app();
        let start = Instant::now();
        let gib = 1_073_741_824u64;

        app.refresh_system_resources(start, || full_sample(None, memory(gib * 98 / 10, gib * 16)));
        let reading = app.state.system_resources.memory.expect("a reading");
        assert_eq!(reading.total_bytes, gib * 16);
        assert_eq!(reading.used_bytes, gib * 98 / 10);

        // A reading can never claim more memory than the machine has.
        app.refresh_system_resources(start + secs(3), || {
            full_sample(None, memory(gib * 20, gib * 16))
        });
        let reading = app.state.system_resources.memory.expect("a reading");
        assert_eq!(reading.used_bytes, gib * 16);
        assert!(reading.used_bytes <= reading.total_bytes);

        // A failed read shows nothing rather than pinning the last value.
        app.refresh_system_resources(start + secs(6), || full_sample(None, None));
        assert_eq!(app.state.system_resources.memory, None);
    }

    #[test]
    fn cpu_and_memory_keep_their_own_three_second_beat() {
        let mut app = visible_app();
        let start = Instant::now();
        app.refresh_system_resources(start, || full_sample(ticks(1_000, 4_000), None));

        // Nothing is re-read before the interval is up.
        for early in [1u64, 2] {
            app.refresh_system_resources(start + secs(early), || {
                panic!("sampled after only {early}s")
            });
        }
        assert_eq!(app.state.system_resources.cpu_percent, None);

        app.refresh_system_resources(start + secs(3), || full_sample(ticks(2_000, 5_000), None));
        assert_eq!(app.state.system_resources.cpu_percent, Some(100));
    }

    #[test]
    fn the_slower_readings_keep_their_own_cadences() {
        // Adding a three-second beat must not drag disk or swap onto it.
        let mut app = visible_app();
        let start = Instant::now();
        app.refresh_system_resources(start, || full_sample(ticks(0, 1), memory(1, 2)));

        let schedule = &app.system_resource_schedule;
        assert_eq!(schedule.last_cpu_memory_sample, Some(start));
        assert_eq!(schedule.last_swap_sample, Some(start));
        assert_eq!(schedule.last_disk_sample, Some(start));

        // Three seconds on: CPU and memory are due, the other two are not.
        let at = start + secs(3);
        assert!(sample_due(
            app.system_resource_schedule.last_cpu_memory_sample,
            at,
            CPU_MEMORY_REFRESH_INTERVAL
        ));
        assert!(!sample_due(
            app.system_resource_schedule.last_swap_sample,
            at,
            SWAP_REFRESH_INTERVAL
        ));
        assert!(!sample_due(
            app.system_resource_schedule.last_disk_sample,
            at,
            DISK_REFRESH_INTERVAL
        ));

        // The intervals themselves are unchanged.
        assert_eq!(CPU_MEMORY_REFRESH_INTERVAL, secs(3));
        assert_eq!(SWAP_REFRESH_INTERVAL, secs(4));
        assert_eq!(DISK_REFRESH_INTERVAL, secs(30));
        assert_eq!(DISK_MARKER_VISIBLE, secs(5));
    }

    #[test]
    fn the_next_deadline_is_the_soonest_of_the_three_beats() {
        let mut app = visible_app();
        let start = Instant::now();
        app.refresh_system_resources(start, || full_sample(ticks(0, 1), memory(1, 2)));

        // With every reading just taken, CPU and memory come round first.
        let deadline = app.next_system_resource_deadline().expect("a deadline");
        assert_eq!(deadline, start + CPU_MEMORY_REFRESH_INTERVAL);
        assert!(deadline < start + SWAP_REFRESH_INTERVAL);
        assert!(deadline < start + DISK_REFRESH_INTERVAL);
    }

    #[test]
    fn a_hidden_footer_samples_nothing_at_all() {
        // No client watching the sidebar means no syscall and no wake-up, for
        // the new readings as much as the old ones.
        let mut app = test_app();
        app.state.view.sidebar_rect = ratatui::layout::Rect::default();
        let changed = app.refresh_system_resources(Instant::now(), || {
            panic!("sampled while the footer could not be seen")
        });
        assert!(!changed);
        assert_eq!(app.next_system_resource_deadline(), None);
        assert_eq!(app.state.system_resources.cpu_percent, None);
        assert_eq!(app.state.system_resources.memory, None);
    }

    #[test]
    fn a_host_that_cannot_report_cpu_or_memory_is_not_marked_as_supporting_them() {
        let mut app = visible_app();
        app.refresh_system_resources(Instant::now(), || full_sample(None, None));
        assert!(!app.state.system_resources.cpu_memory_supported);

        let mut app = visible_app();
        app.refresh_system_resources(Instant::now(), || full_sample(ticks(1, 2), None));
        assert!(app.state.system_resources.cpu_memory_supported);
    }

    #[test]
    fn absent_platform_readings_leave_the_footer_empty() {
        let mut resources = SystemResources::default();
        resources.set_swap(None);
        assert!(!resources.set_disk(None));

        assert!(resources.is_empty());
        assert_eq!(resources.disk_available, None);
        assert_eq!(resources.swap_used, None);
        // Nothing was ever read, so there is no last good value to qualify.
        assert_eq!(resources.disk_marker, DiskRefreshMarker::None);
    }

    #[test]
    fn zero_swap_is_a_reading_rather_than_a_missing_one() {
        let mut resources = SystemResources::default();
        resources.set_swap(Some(0));
        resources.set_disk(Some(199 * GB));

        assert!(!resources.is_empty());
        assert_eq!(
            resources.swap_used,
            Some(ResourceReading {
                bytes: 0,
                pressure: ResourcePressure::Normal,
            })
        );
    }

    #[test]
    fn the_first_reading_is_due_immediately_rather_than_one_interval_late() {
        let mut app = visible_app();
        let now = Instant::now();

        // The deadline is read from a later clock than the `now` the loop
        // hands in, so due-ness must not be decided by comparing the two.
        assert!(app
            .next_system_resource_deadline()
            .is_some_and(|deadline| deadline > now));
        assert!(app.handle_system_resource_refresh(now));
        assert!(!app.state.system_resources.is_empty());
    }

    #[test]
    fn swap_is_not_resampled_until_its_interval_elapses() {
        let mut app = visible_app();
        let now = Instant::now();

        assert!(app.refresh_system_resources(now, || sample(Some(199 * GB))));
        assert!(
            !app.refresh_system_resources(now + SWAP_REFRESH_INTERVAL / 2, || {
                panic!("sampled before the interval elapsed")
            })
        );
        assert_eq!(app.system_resource_schedule.last_swap_sample, Some(now));

        // An unchanged sample still counts as handled, just not as a redraw.
        let later = now + SWAP_REFRESH_INTERVAL;
        app.refresh_system_resources(later, || sample(Some(199 * GB)));
        assert_eq!(app.system_resource_schedule.last_swap_sample, Some(later));
    }

    #[test]
    fn free_space_is_read_at_startup_and_then_once_per_interval() {
        let mut app = visible_app();
        let start = Instant::now();

        // Startup: read straight away rather than one interval late.
        assert!(app.refresh_system_resources(start, || sample(Some(199 * GB))));
        assert_eq!(
            app.state.system_resources.disk_available.map(|r| r.bytes),
            Some(199 * GB)
        );
        assert_eq!(app.system_resource_schedule.last_disk_sample, Some(start));

        // Swap keeps its own faster beat, and the disk reading rides along
        // without being re-read.
        let mid = start + DISK_REFRESH_INTERVAL - Duration::from_secs(1);
        app.refresh_system_resources(mid, || sample(Some(GB)));
        assert_eq!(app.system_resource_schedule.last_disk_sample, Some(start));
        assert_eq!(
            app.state.system_resources.disk_available.map(|r| r.bytes),
            Some(199 * GB)
        );

        let due = start + DISK_REFRESH_INTERVAL;
        app.refresh_system_resources(due, || sample(Some(150 * GB)));
        assert_eq!(app.system_resource_schedule.last_disk_sample, Some(due));
        assert_eq!(
            app.state.system_resources.disk_available.map(|r| r.bytes),
            Some(150 * GB)
        );
    }

    #[test]
    fn a_successful_refresh_marks_itself_even_when_the_number_did_not_move() {
        let mut app = visible_app();
        let start = Instant::now();

        app.refresh_system_resources(start, || sample(Some(199 * GB)));
        assert_eq!(
            app.state.system_resources.disk_marker,
            DiskRefreshMarker::Refreshed
        );

        // Just short of the marker's lifetime it is still up.
        let nearly = start + DISK_MARKER_VISIBLE - Duration::from_millis(1);
        app.refresh_system_resources(nearly, || sample(Some(199 * GB)));
        assert_eq!(
            app.state.system_resources.disk_marker,
            DiskRefreshMarker::Refreshed
        );

        // On expiry only the marker goes; the reading stays put.
        let expired = start + DISK_MARKER_VISIBLE;
        assert!(app.refresh_system_resources(expired, || panic!("no sample is due yet")));
        assert_eq!(
            app.state.system_resources.disk_marker,
            DiskRefreshMarker::None
        );
        assert_eq!(
            app.state.system_resources.disk_available.map(|r| r.bytes),
            Some(199 * GB)
        );

        // An identical reading one interval later is still a refresh.
        let again = start + DISK_REFRESH_INTERVAL;
        assert!(app.refresh_system_resources(again, || sample(Some(199 * GB))));
        assert_eq!(
            app.state.system_resources.disk_marker,
            DiskRefreshMarker::Refreshed
        );
        assert_eq!(
            app.system_resource_schedule.marker_expires_at,
            Some(again + DISK_MARKER_VISIBLE)
        );
    }

    #[test]
    fn a_refresh_during_the_marker_extends_it_from_that_moment() {
        let mut app = visible_app();
        let start = Instant::now();

        app.refresh_system_resources(start, || sample(Some(199 * GB)));
        // Force a second successful disk read while the first marker is up.
        app.system_resource_schedule.last_disk_sample = None;
        let again = start + Duration::from_secs(2);
        app.refresh_system_resources(again, || sample(Some(199 * GB)));

        assert_eq!(
            app.system_resource_schedule.marker_expires_at,
            Some(again + DISK_MARKER_VISIBLE)
        );
        // The original deadline has passed, and the marker is still up. Swap
        // is due at this point, so the sampler still answers.
        app.refresh_system_resources(start + DISK_MARKER_VISIBLE, || sample(Some(199 * GB)));
        assert_eq!(
            app.state.system_resources.disk_marker,
            DiskRefreshMarker::Refreshed
        );
    }

    #[test]
    fn a_failed_refresh_keeps_the_last_reading_and_flags_it_until_one_succeeds() {
        let mut app = visible_app();
        let start = Instant::now();

        app.refresh_system_resources(start, || sample(Some(199 * GB)));

        let failed = start + DISK_REFRESH_INTERVAL;
        assert!(app.refresh_system_resources(failed, || sample(None)));
        assert_eq!(
            app.state.system_resources.disk_available.map(|r| r.bytes),
            Some(199 * GB),
            "the last good reading is kept rather than blanked"
        );
        assert_eq!(
            app.state.system_resources.disk_marker,
            DiskRefreshMarker::Stale
        );
        assert!(app.system_resource_schedule.disk_failure_logged);

        // A stale marker has no expiry, so it survives well past the tick's
        // lifetime and a further failure keeps it without logging again.
        assert_eq!(app.system_resource_schedule.marker_expires_at, None);
        let still_failing = failed + DISK_REFRESH_INTERVAL;
        assert!(!app.refresh_system_resources(still_failing, || sample(None)));
        assert_eq!(
            app.state.system_resources.disk_marker,
            DiskRefreshMarker::Stale
        );

        // Recovery swaps the flag for a fresh tick.
        let recovered = still_failing + DISK_REFRESH_INTERVAL;
        assert!(app.refresh_system_resources(recovered, || sample(Some(150 * GB))));
        assert_eq!(
            app.state.system_resources.disk_marker,
            DiskRefreshMarker::Refreshed
        );
        assert_eq!(
            app.state.system_resources.disk_available.map(|r| r.bytes),
            Some(150 * GB)
        );
        assert!(!app.system_resource_schedule.disk_failure_logged);
    }

    #[test]
    fn a_volume_that_never_answered_shows_nothing_rather_than_a_lone_marker() {
        let mut app = visible_app();
        let start = Instant::now();

        app.refresh_system_resources(start, || SystemResourceSample {
            disk_available_bytes: None,
            swap_used_bytes: Some(0),
            ..Default::default()
        });

        assert_eq!(app.state.system_resources.disk_available, None);
        assert_eq!(
            app.state.system_resources.disk_marker,
            DiskRefreshMarker::None
        );
    }

    #[test]
    fn the_marker_deadline_wakes_the_loop_before_the_next_sample_would() {
        let mut app = visible_app();
        let start = Instant::now();

        app.refresh_system_resources(start, || sample(Some(199 * GB)));

        // Swap is due first here, but the marker must still be reachable: it
        // expires long before the disk beat comes round again.
        let deadline = app
            .next_system_resource_deadline()
            .expect("a visible footer always has a deadline");
        assert!(deadline <= start + DISK_MARKER_VISIBLE);
        assert!(app.system_resource_schedule.marker_expires_at.is_some());
    }

    #[test]
    fn disabling_or_collapsing_the_sidebar_stops_sampling() {
        let mut app = test_app();
        app.state.view.sidebar_rect = ratatui::layout::Rect::new(0, 0, 26, 40);
        app.state.sidebar_resources.enabled = false;
        assert_eq!(app.next_system_resource_deadline(), None);
        assert!(!app.handle_system_resource_refresh(Instant::now()));

        app.state.sidebar_resources.enabled = true;
        app.state.sidebar_collapsed = true;
        assert_eq!(app.next_system_resource_deadline(), None);
        assert!(!app.handle_system_resource_refresh(Instant::now()));

        app.state.sidebar_collapsed = false;
        assert!(app.handle_system_resource_refresh(Instant::now()));

        // The mobile layout leaves no sidebar to draw into.
        let mut mobile = test_app();
        assert_eq!(mobile.state.view.sidebar_rect.width, 0);
        assert_eq!(mobile.next_system_resource_deadline(), None);
        assert!(!mobile.handle_system_resource_refresh(Instant::now()));
    }
}

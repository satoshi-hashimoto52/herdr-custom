//! Host disk and swap readings for the sidebar footer.
//!
//! Sampling happens on the app's existing deadline loop rather than on a timer
//! of its own, and only while the expanded sidebar can actually show the
//! footer. A collapsed sidebar and the mobile layout both leave
//! [`App::next_system_resource_deadline`] empty, so neither wakes the loop to
//! compute a reading nobody will see.

use std::time::{Duration, Instant};

use super::App;
use crate::platform::SystemResourceSample;

/// Sampling cadence. Slow enough to stay invisible in a CPU profile, quick
/// enough that a disk filling up during a build is noticed.
const REFRESH_INTERVAL: Duration = Duration::from_secs(4);

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

/// Cached host disk and swap readings shown in the sidebar footer.
///
/// A reading stays `None` when the platform cannot report it, which hides that
/// line instead of showing a fabricated zero.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct SystemResources {
    pub disk_available: Option<ResourceReading>,
    pub swap_used: Option<ResourceReading>,
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

impl SystemResources {
    fn from_sample(sample: SystemResourceSample) -> Self {
        Self {
            disk_available: sample.disk_available_bytes.map(|bytes| ResourceReading {
                bytes,
                pressure: disk_pressure(bytes),
            }),
            swap_used: sample.swap_used_bytes.map(|bytes| ResourceReading {
                bytes,
                pressure: swap_pressure(bytes),
            }),
        }
    }

    /// True when no reading is available, which hides the footer entirely.
    pub(crate) fn is_empty(&self) -> bool {
        self.disk_available.is_none() && self.swap_used.is_none()
    }
}

impl App {
    /// Samples host resources when due, reporting whether the visible reading
    /// changed.
    ///
    /// Returns `false` on an unchanged sample so an idle host does not mark the
    /// frame dirty every few seconds. The footer therefore holds still instead
    /// of flickering while disk and swap sit where they were.
    pub(crate) fn handle_system_resource_refresh(&mut self, now: Instant) -> bool {
        if !self.system_resource_readout_visible() {
            return false;
        }
        // Compared against the caller's `now` rather than a deadline this
        // function samples itself: an "immediately due" deadline read from a
        // later clock than `now` is never actually reached, which left the
        // first reading permanently pending.
        let due = match self.last_system_resource_refresh {
            Some(last) => now.saturating_duration_since(last) >= REFRESH_INTERVAL,
            None => true,
        };
        if !due {
            return false;
        }

        self.last_system_resource_refresh = Some(now);
        let sampled = SystemResources::from_sample(crate::platform::system_resource_sample());
        let changed = self.state.system_resources != sampled;
        self.state.system_resources = sampled;
        changed
    }

    /// When the next sample is due, or `None` while the footer cannot be seen.
    pub(crate) fn next_system_resource_deadline(&self) -> Option<Instant> {
        if !self.system_resource_readout_visible() {
            return None;
        }
        Some(match self.last_system_resource_refresh {
            Some(last) => last + REFRESH_INTERVAL,
            // Wake the loop straight away so the footer is populated by the
            // time the sidebar first draws.
            None => Instant::now(),
        })
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

    #[test]
    fn absent_platform_readings_leave_the_footer_empty() {
        let resources = SystemResources::from_sample(SystemResourceSample::default());

        assert!(resources.is_empty());
        assert_eq!(resources.disk_available, None);
        assert_eq!(resources.swap_used, None);
    }

    #[test]
    fn zero_swap_is_a_reading_rather_than_a_missing_one() {
        let resources = SystemResources::from_sample(SystemResourceSample {
            disk_available_bytes: Some(199 * GB),
            swap_used_bytes: Some(0),
        });

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
        let mut app = test_app();
        let now = Instant::now();

        // The deadline is read from a later clock than the `now` the loop
        // hands in, so due-ness must not be decided by comparing the two.
        app.state.view.sidebar_rect = ratatui::layout::Rect::new(0, 0, 26, 40);
        assert!(app
            .next_system_resource_deadline()
            .is_some_and(|deadline| deadline > now));
        assert!(app.handle_system_resource_refresh(now));
        assert!(!app.state.system_resources.is_empty());
    }

    #[test]
    fn readings_are_not_resampled_until_the_interval_elapses() {
        let mut app = test_app();
        app.state.view.sidebar_rect = ratatui::layout::Rect::new(0, 0, 26, 40);
        let now = Instant::now();

        assert!(app.handle_system_resource_refresh(now));
        assert!(!app.handle_system_resource_refresh(now + REFRESH_INTERVAL / 2));
        assert_eq!(app.last_system_resource_refresh, Some(now));

        // An unchanged sample still counts as handled, just not as a redraw.
        let later = now + REFRESH_INTERVAL;
        app.handle_system_resource_refresh(later);
        assert_eq!(app.last_system_resource_refresh, Some(later));
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

//! How long an agent has actually spent working, shown beside its state word.
//!
//! This is measured time, not a guess at progress: the clock runs only while
//! an agent is `working`, pauses while it is `waiting` on a person, and stops
//! for good when the turn ends. A row therefore says how much work went into
//! the answer, which is a fact, rather than how close it is to finishing,
//! which nothing here could know.
//!
//! Time enters through arguments rather than through `Instant::now()` inside
//! the logic, so every transition is testable without waiting on a clock.
//!
//! Totals live for as long as the server process does. [`Instant`] has no
//! meaning outside the process that produced it, so a server restart or a live
//! handoff — which is a restart that keeps the panes — starts every row from
//! zero rather than carrying a figure across. Persisting working time would
//! mean writing a wall-clock record to disk and reconciling it on start-up,
//! which is a great deal of machinery for a readout whose whole point is the
//! turn happening now.

use std::collections::HashMap;
use std::time::{Duration, Instant};

use crate::detect::AgentState;
use crate::layout::PaneId;

/// One pane's working time.
///
/// Kept per [`PaneId`], which is allocated from a single global counter, so
/// two agents can never share an entry however their workspaces are arranged
/// or however alike their names are.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub(crate) struct WorkTimer {
    /// Working time banked by earlier stretches of this turn.
    accumulated: Duration,
    /// When the current stretch began, or `None` while the clock is stopped.
    running_since: Option<Instant>,
}

impl WorkTimer {
    /// Working time as of `now`, including any stretch still running.
    pub(crate) fn elapsed(&self, now: Instant) -> Duration {
        match self.running_since {
            // `saturating_duration_since` is what keeps a clock that steps
            // backwards from producing a negative, and therefore huge,
            // duration.
            Some(since) => self.accumulated + now.saturating_duration_since(since),
            None => self.accumulated,
        }
    }

    pub(crate) fn is_running(&self) -> bool {
        self.running_since.is_some()
    }

    /// Banks the running stretch and stops the clock.
    fn pause(&mut self, now: Instant) {
        if let Some(since) = self.running_since.take() {
            self.accumulated += now.saturating_duration_since(since);
        }
    }

    /// Starts a new turn from zero.
    fn restart(&mut self, now: Instant) {
        self.accumulated = Duration::ZERO;
        self.running_since = Some(now);
    }

    /// Picks the clock back up where it left off.
    fn resume(&mut self, now: Instant) {
        if self.running_since.is_none() {
            self.running_since = Some(now);
        }
    }
}

/// Every pane's working time.
#[derive(Debug, Clone, Default)]
pub(crate) struct WorkTimers {
    timers: HashMap<PaneId, WorkTimer>,
}

impl WorkTimers {
    pub(crate) fn get(&self, pane_id: PaneId) -> Option<&WorkTimer> {
        self.timers.get(&pane_id)
    }

    pub(crate) fn is_empty(&self) -> bool {
        self.timers.is_empty()
    }

    pub(crate) fn values(&self) -> impl Iterator<Item = &WorkTimer> {
        self.timers.values()
    }

    /// Applies one state transition.
    ///
    /// Only called when the state actually changed, so a repeated report of
    /// the state a pane is already in cannot restart or rewind anything.
    ///
    /// `Unknown` is treated exactly as `Idle`: the sidebar already prints both
    /// as `idle`, and inventing a fourth behavior for a state the detector
    /// uses to mean "this is a plain shell" would put time on rows that never
    /// worked.
    pub(crate) fn on_state_change(
        &mut self,
        pane_id: PaneId,
        previous: AgentState,
        state: AgentState,
        now: Instant,
    ) {
        match state {
            AgentState::Working => {
                let timer = self.timers.entry(pane_id).or_default();
                if previous == AgentState::Blocked {
                    // Coming back from a question resumes the same turn.
                    timer.resume(now);
                } else {
                    // Anything else — idle, done, error, a fresh agent — is a
                    // new piece of work and starts from zero.
                    timer.restart(now);
                }
            }
            AgentState::Blocked => match previous {
                // Waiting on a person is not working, but the turn is not
                // over either: bank the time and keep showing it.
                AgentState::Working => {
                    if let Some(timer) = self.timers.get_mut(&pane_id) {
                        timer.pause(now);
                    }
                }
                // Waiting reached without working first. Either this turn has
                // asked something before doing anything, or the last turn
                // already finished and this is the start of a new one. No
                // work has been measured either way, and the finished turn's
                // total would be a wrong figure to print beside `waiting`.
                AgentState::Idle | AgentState::Unknown => {
                    self.timers.remove(&pane_id);
                }
                // Not a real transition; the caller filters these out. Leave
                // the banked total exactly as it is if one slips through.
                AgentState::Blocked => {}
            },
            // The turn ended. The total stays put: `done` and `error` both
            // show the final figure until the next turn starts.
            AgentState::Idle | AgentState::Unknown => {
                if let Some(timer) = self.timers.get_mut(&pane_id) {
                    timer.pause(now);
                }
            }
        }
    }

    /// Forgets a pane's time entirely.
    ///
    /// Used when a pane closes, and when a pane's agent is replaced, so a new
    /// agent never inherits the last one's total.
    pub(crate) fn clear(&mut self, pane_id: PaneId) {
        self.timers.remove(&pane_id);
    }

    /// When a displayed figure will next change, or `None` while none is
    /// running.
    ///
    /// Only a running clock moves, so a sidebar full of finished agents costs
    /// no wake-ups at all. The deadline is the next whole second of the
    /// *displayed* value rather than a fixed one-second tick, so the redraw
    /// lands on the change rather than near it.
    pub(crate) fn next_deadline(&self, now: Instant) -> Option<Instant> {
        self.timers
            .values()
            .filter_map(|timer| {
                let since = timer.running_since?;
                let elapsed = now.saturating_duration_since(since) + timer.accumulated;
                let remainder = Duration::from_nanos(u64::from(elapsed.subsec_nanos()));
                Some(now + (Duration::from_secs(1) - remainder))
            })
            .min()
    }
}

/// Formats working time the way the sidebar prints it.
///
/// `MM:SS` under an hour and `H:MM:SS` at or over one, seconds truncated
/// rather than rounded so a figure never reads ahead of the work. Hours are
/// not wrapped at 24: a day-long run should say so rather than start again.
pub(crate) fn format_work_duration(elapsed: Duration) -> String {
    let total = elapsed.as_secs();
    let (hours, minutes, seconds) = (total / 3_600, total % 3_600 / 60, total % 60);
    if hours == 0 {
        format!("{minutes:02}:{seconds:02}")
    } else {
        format!("{hours}:{minutes:02}:{seconds:02}")
    }
}

impl super::App {
    /// Advances the clock the sidebar reads working time against.
    ///
    /// Returns whether a drawn figure actually changed, so a sidebar of
    /// finished agents never marks the frame dirty.
    pub(crate) fn handle_work_timer_tick(&mut self, now: Instant) -> bool {
        if self.state.work_timers.is_empty() {
            return false;
        }
        let previous = self.state.work_clock.replace(now);
        let Some(previous) = previous else {
            return true;
        };
        self.state.work_timers.values().any(|timer| {
            timer.is_running() && timer.elapsed(previous).as_secs() != timer.elapsed(now).as_secs()
        })
    }

    /// When a drawn figure will next change, or `None` while none is running.
    pub(crate) fn next_work_timer_deadline(&self) -> Option<Instant> {
        let now = self.state.work_clock.unwrap_or_else(Instant::now);
        self.state.work_timers.next_deadline(now)
    }
}

#[cfg(test)]
mod app_tests {
    use super::*;
    use crate::detect::{Agent, AgentState};

    fn test_app() -> crate::app::App {
        let (_api_tx, api_rx) = tokio::sync::mpsc::unbounded_channel();
        crate::app::App::new(
            &crate::config::Config::default(),
            true,
            None,
            api_rx,
            crate::api::EventHub::default(),
        )
    }

    /// One workspace with one Claude pane, driven through the real detector
    /// entry point rather than by poking at the timer.
    fn app_with_agent() -> (crate::app::App, PaneId, crate::terminal::TerminalId) {
        let mut app = test_app();
        let workspace = crate::workspace::Workspace::test_new("one");
        let pane_id = workspace.tabs[0].root_pane;
        app.state.workspaces = vec![workspace];
        app.state.ensure_test_terminals();
        app.state.active = Some(0);
        let terminal_id = app.state.workspaces[0].tabs[0].panes[&pane_id]
            .attached_terminal_id
            .clone();
        (app, pane_id, terminal_id)
    }

    fn report(app: &mut crate::app::App, pane_id: PaneId, state: AgentState) {
        app.state
            .update_terminal_state_for_test(pane_id, |terminal| {
                terminal.set_detected_state(Some(Agent::Claude), state)
            });
    }

    fn elapsed(app: &crate::app::App, pane_id: PaneId, at: Instant) -> Option<Duration> {
        app.state.work_timers.get(pane_id).map(|t| t.elapsed(at))
    }

    #[test]
    fn a_real_report_sequence_measures_only_the_working_stretches() {
        let (mut app, pane_id, _) = app_with_agent();
        report(&mut app, pane_id, AgentState::Working);
        assert!(
            app.state
                .work_timers
                .get(pane_id)
                .is_some_and(|t| t.is_running()),
            "a working report starts the clock"
        );

        report(&mut app, pane_id, AgentState::Blocked);
        assert!(
            !app.state
                .work_timers
                .get(pane_id)
                .expect("a timer")
                .is_running(),
            "a blocked report stops it"
        );

        report(&mut app, pane_id, AgentState::Working);
        assert!(app
            .state
            .work_timers
            .get(pane_id)
            .expect("a timer")
            .is_running());

        report(&mut app, pane_id, AgentState::Idle);
        let frozen = elapsed(&app, pane_id, Instant::now()).expect("a total");
        assert!(!app
            .state
            .work_timers
            .get(pane_id)
            .expect("a timer")
            .is_running());
        assert_eq!(
            elapsed(&app, pane_id, Instant::now() + Duration::from_secs(60)),
            Some(frozen),
            "a finished total does not keep growing"
        );
    }

    #[test]
    fn repeating_a_report_does_not_restart_the_clock() {
        let (mut app, pane_id, _) = app_with_agent();
        report(&mut app, pane_id, AgentState::Working);
        let started = app
            .state
            .work_timers
            .get(pane_id)
            .copied()
            .expect("a timer");
        // The same state again is not a transition, so nothing is touched.
        report(&mut app, pane_id, AgentState::Working);
        report(&mut app, pane_id, AgentState::Working);
        assert_eq!(app.state.work_timers.get(pane_id).copied(), Some(started));
    }

    #[test]
    fn a_config_reload_leaves_running_totals_alone() {
        let (mut app, pane_id, _) = app_with_agent();
        report(&mut app, pane_id, AgentState::Working);
        let before = app
            .state
            .work_timers
            .get(pane_id)
            .copied()
            .expect("a timer");
        app.apply_config_from_disk(false);
        assert_eq!(
            app.state.work_timers.get(pane_id).copied(),
            Some(before),
            "reloading configuration must not reset measured work"
        );
    }

    #[test]
    fn closing_a_pane_forgets_its_total() {
        let (mut app, pane_id, _) = app_with_agent();
        report(&mut app, pane_id, AgentState::Working);
        assert!(app.state.work_timers.get(pane_id).is_some());
        app.state.remove_plugin_pane_records([pane_id]);
        assert!(app.state.work_timers.get(pane_id).is_none());
    }

    #[test]
    fn collapsing_the_sidebar_neither_stops_nor_rewinds_the_clock() {
        // Working time is measured from the transitions, not sampled while
        // drawing, so a hidden sidebar keeps counting and reappears correct.
        let (mut app, pane_id, _) = app_with_agent();
        report(&mut app, pane_id, AgentState::Working);
        let running = app
            .state
            .work_timers
            .get(pane_id)
            .copied()
            .expect("a timer");

        app.state.sidebar_collapsed = true;
        app.handle_work_timer_tick(Instant::now());
        app.state.sidebar_collapsed = false;

        assert_eq!(
            app.state.work_timers.get(pane_id).copied(),
            Some(running),
            "collapsing the sidebar must not touch the measurement"
        );
        assert!(running.is_running());
    }

    #[test]
    fn selecting_another_agent_leaves_every_total_alone() {
        let (mut app, first, _) = app_with_agent();
        let second = app.state.workspaces[0].test_split(ratatui::layout::Direction::Vertical);
        app.state.ensure_test_terminals();
        report(&mut app, first, AgentState::Working);
        report(&mut app, second, AgentState::Working);
        let before: Vec<_> = [first, second]
            .map(|id| app.state.work_timers.get(id).copied())
            .to_vec();

        // `test_split` leaves the focus on the new pane, so this moves the
        // selection twice, once in each direction.
        assert!(app.state.focus_pane_in_workspace(0, first));
        assert!(app.state.focus_pane_in_workspace(0, second));

        let after: Vec<_> = [first, second]
            .map(|id| app.state.work_timers.get(id).copied())
            .to_vec();
        assert_eq!(before, after, "selection is not a state change");
        assert!(before.iter().all(|timer| timer.is_some()));
    }

    #[test]
    fn a_restart_or_live_handoff_starts_from_a_clean_slate() {
        // `Instant` is meaningless outside the process that made it, so
        // totals deliberately do not survive a restart, and a live handoff is
        // a restart that keeps the panes. A new process must therefore come
        // up with nothing rather than with a stale figure.
        let (mut app, pane_id, _) = app_with_agent();
        report(&mut app, pane_id, AgentState::Working);
        assert!(app.state.work_timers.get(pane_id).is_some());

        let successor = test_app();
        assert!(
            successor.state.work_timers.is_empty(),
            "a fresh process must not inherit working time"
        );
        assert_eq!(successor.next_work_timer_deadline(), None);
    }

    #[test]
    fn only_a_running_row_wakes_the_loop_and_only_on_a_second_boundary() {
        let (mut app, pane_id, _) = app_with_agent();
        assert_eq!(
            app.next_work_timer_deadline(),
            None,
            "no timers, no wake-ups"
        );

        report(&mut app, pane_id, AgentState::Working);
        let now = Instant::now();
        app.state.work_clock = Some(now);
        let deadline = app.next_work_timer_deadline().expect("a running row");
        assert!(deadline > now && deadline <= now + Duration::from_secs(1));

        // Inside a second nothing is redrawn; crossing one is.
        assert!(!app.handle_work_timer_tick(now + Duration::from_millis(200)));
        assert!(app.handle_work_timer_tick(now + Duration::from_millis(1_200)));

        report(&mut app, pane_id, AgentState::Idle);
        app.state.work_clock = Some(Instant::now());
        assert_eq!(
            app.next_work_timer_deadline(),
            None,
            "a finished row must not keep waking the loop"
        );
        assert!(!app.handle_work_timer_tick(Instant::now() + Duration::from_secs(5)));
    }

    #[test]
    fn the_working_deadline_does_not_disturb_the_disk_readout() {
        // Both live on the same loop; the disk footer keeps its own cadence.
        let (mut app, pane_id, _) = app_with_agent();
        app.state.view.sidebar_rect = ratatui::layout::Rect::new(0, 0, 26, 40);
        report(&mut app, pane_id, AgentState::Working);
        let now = Instant::now();
        app.state.work_clock = Some(now);
        assert!(app.handle_system_resource_refresh(now));
        let disk = app
            .next_system_resource_deadline()
            .expect("a disk deadline");
        let work = app.next_work_timer_deadline().expect("a work deadline");
        assert!(
            work < disk,
            "the second-by-second row should wake sooner than the four-second disk sample"
        );
        // And ticking the work timer does not consume the disk deadline.
        app.handle_work_timer_tick(now + Duration::from_millis(1_100));
        assert_eq!(app.next_system_resource_deadline(), Some(disk));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pane(raw: u32) -> PaneId {
        PaneId::from_raw(raw)
    }

    fn secs(n: u64) -> Duration {
        Duration::from_secs(n)
    }

    /// Drives one pane through a sequence of `(state, seconds from start)`.
    fn run(steps: &[(AgentState, u64)]) -> (WorkTimers, Instant, PaneId) {
        let start = Instant::now();
        let mut timers = WorkTimers::default();
        let id = pane(1);
        let mut previous = AgentState::Idle;
        for (state, at) in steps {
            timers.on_state_change(id, previous, *state, start + secs(*at));
            previous = *state;
        }
        (timers, start, id)
    }

    fn elapsed_at(timers: &WorkTimers, id: PaneId, start: Instant, at: u64) -> Option<Duration> {
        timers.get(id).map(|timer| timer.elapsed(start + secs(at)))
    }

    #[test]
    fn an_idle_pane_has_no_timer_at_all() {
        let (timers, _, id) = run(&[(AgentState::Idle, 0)]);
        assert!(timers.get(id).is_none(), "idle should not create a timer");
        assert!(timers.is_empty());
    }

    #[test]
    fn work_starts_from_zero_and_counts_up() {
        let (timers, start, id) = run(&[(AgentState::Working, 0)]);
        assert_eq!(elapsed_at(&timers, id, start, 0), Some(secs(0)));
        assert_eq!(elapsed_at(&timers, id, start, 1), Some(secs(1)));
        assert_eq!(elapsed_at(&timers, id, start, 84), Some(secs(84)));
    }

    #[test]
    fn waiting_stops_the_clock_and_keeps_the_figure() {
        let (timers, start, id) = run(&[(AgentState::Working, 0), (AgentState::Blocked, 30)]);
        assert_eq!(elapsed_at(&timers, id, start, 30), Some(secs(30)));
        // Still 30 a minute later: waiting on a person is not working.
        assert_eq!(elapsed_at(&timers, id, start, 90), Some(secs(30)));
        assert!(!timers.get(id).expect("a timer").is_running());
    }

    #[test]
    fn answering_resumes_from_the_banked_total() {
        let (timers, start, id) = run(&[
            (AgentState::Working, 0),
            (AgentState::Blocked, 30),
            (AgentState::Working, 90),
        ]);
        assert_eq!(elapsed_at(&timers, id, start, 90), Some(secs(30)));
        assert_eq!(elapsed_at(&timers, id, start, 100), Some(secs(40)));
        assert!(timers.get(id).expect("a timer").is_running());
    }

    #[test]
    fn finishing_freezes_the_total_whichever_state_it_came_from() {
        // working -> done
        let (timers, start, id) = run(&[(AgentState::Working, 0), (AgentState::Idle, 45)]);
        assert_eq!(elapsed_at(&timers, id, start, 45), Some(secs(45)));
        assert_eq!(elapsed_at(&timers, id, start, 600), Some(secs(45)));

        // working -> waiting -> done keeps only the working part
        let (timers, start, id) = run(&[
            (AgentState::Working, 0),
            (AgentState::Blocked, 20),
            (AgentState::Idle, 300),
        ]);
        assert_eq!(elapsed_at(&timers, id, start, 900), Some(secs(20)));
    }

    #[test]
    fn a_new_turn_starts_from_zero_again() {
        let (mut timers, start, id) = run(&[(AgentState::Working, 0), (AgentState::Idle, 45)]);
        timers.on_state_change(id, AgentState::Idle, AgentState::Working, start + secs(100));
        assert_eq!(elapsed_at(&timers, id, start, 100), Some(secs(0)));
        assert_eq!(elapsed_at(&timers, id, start, 105), Some(secs(5)));
    }

    #[test]
    fn repeating_a_state_banks_nothing_extra() {
        // The caller only reports genuine transitions. This pins the property
        // that matters if one ever slips through: a second `waiting` while
        // already waiting must not add the time spent waiting.
        let start = Instant::now();
        let mut timers = WorkTimers::default();
        let id = pane(1);
        timers.on_state_change(id, AgentState::Idle, AgentState::Working, start);
        timers.on_state_change(
            id,
            AgentState::Working,
            AgentState::Blocked,
            start + secs(10),
        );
        timers.on_state_change(
            id,
            AgentState::Blocked,
            AgentState::Blocked,
            start + secs(50),
        );
        assert_eq!(elapsed_at(&timers, id, start, 100), Some(secs(10)));

        // The same for a repeated finish.
        timers.on_state_change(id, AgentState::Blocked, AgentState::Idle, start + secs(120));
        timers.on_state_change(id, AgentState::Idle, AgentState::Idle, start + secs(300));
        assert_eq!(elapsed_at(&timers, id, start, 900), Some(secs(10)));
    }

    #[test]
    fn every_pane_keeps_its_own_total() {
        let start = Instant::now();
        let mut timers = WorkTimers::default();
        timers.on_state_change(pane(1), AgentState::Idle, AgentState::Working, start);
        timers.on_state_change(
            pane(2),
            AgentState::Idle,
            AgentState::Working,
            start + secs(10),
        );
        timers.on_state_change(
            pane(3),
            AgentState::Idle,
            AgentState::Working,
            start + secs(20),
        );
        timers.on_state_change(
            pane(2),
            AgentState::Working,
            AgentState::Idle,
            start + secs(35),
        );

        assert_eq!(elapsed_at(&timers, pane(1), start, 60), Some(secs(60)));
        assert_eq!(elapsed_at(&timers, pane(2), start, 60), Some(secs(25)));
        assert_eq!(elapsed_at(&timers, pane(3), start, 60), Some(secs(40)));
    }

    #[test]
    fn a_clock_that_steps_backwards_never_shows_a_negative() {
        let start = Instant::now() + secs(100);
        let mut timers = WorkTimers::default();
        let id = pane(1);
        timers.on_state_change(id, AgentState::Idle, AgentState::Working, start);
        // Reading at a moment before the start yields zero, not a wrap-around.
        assert_eq!(
            timers.get(id).expect("a timer").elapsed(start - secs(50)),
            Duration::ZERO
        );
    }

    #[test]
    fn only_a_running_clock_asks_for_a_redraw() {
        let start = Instant::now();
        let mut timers = WorkTimers::default();
        let id = pane(1);
        assert_eq!(timers.next_deadline(start), None, "no timers, no wake-ups");

        timers.on_state_change(id, AgentState::Idle, AgentState::Working, start);
        let deadline = timers.next_deadline(start).expect("a running clock");
        assert!(deadline > start && deadline <= start + secs(1));

        timers.on_state_change(
            id,
            AgentState::Working,
            AgentState::Blocked,
            start + secs(5),
        );
        assert_eq!(
            timers.next_deadline(start + secs(5)),
            None,
            "a paused clock never moves, so it must not wake the loop"
        );
    }

    #[test]
    fn the_redraw_lands_on_the_second_the_figure_changes() {
        let start = Instant::now();
        let mut timers = WorkTimers::default();
        let id = pane(1);
        timers.on_state_change(id, AgentState::Idle, AgentState::Working, start);
        // A third of a second in, the figure changes two thirds later, not a
        // whole second later.
        let now = start + Duration::from_millis(333);
        let deadline = timers.next_deadline(now).expect("a running clock");
        let wait = deadline.saturating_duration_since(now);
        assert!(
            wait > Duration::from_millis(650) && wait <= Duration::from_millis(667),
            "waited {wait:?}"
        );
    }

    #[test]
    fn closed_panes_are_forgotten() {
        let start = Instant::now();
        let mut timers = WorkTimers::default();
        timers.on_state_change(pane(1), AgentState::Idle, AgentState::Working, start);
        timers.on_state_change(pane(2), AgentState::Idle, AgentState::Working, start);

        timers.clear(pane(1));
        assert!(timers.get(pane(1)).is_none());
        assert!(
            timers.get(pane(2)).is_some(),
            "closing one pane keeps the others"
        );

        timers.clear(pane(2));
        assert!(timers.is_empty());
    }

    #[test]
    fn waiting_reached_without_working_has_nothing_to_show() {
        // A turn that asks something before doing any work has no measured
        // time, so there must be no timer for the row to print.
        let (timers, _, id) = run(&[(AgentState::Blocked, 0)]);
        assert!(timers.get(id).is_none());
        assert!(timers.is_empty());

        // And answering it starts that turn from zero rather than from
        // whatever the pane was doing before.
        let (mut timers, start, id) = run(&[(AgentState::Blocked, 0)]);
        timers.on_state_change(
            id,
            AgentState::Blocked,
            AgentState::Working,
            start + secs(30),
        );
        assert_eq!(elapsed_at(&timers, id, start, 40), Some(secs(10)));
    }

    #[test]
    fn a_finished_total_is_dropped_rather_than_reprinted_beside_the_next_wait() {
        // done 00:45, then the next turn opens with a question. The 45
        // seconds belong to the turn that ended, not to this one.
        let (timers, _, id) = run(&[
            (AgentState::Working, 0),
            (AgentState::Idle, 45),
            (AgentState::Blocked, 100),
        ]);
        assert!(
            timers.get(id).is_none(),
            "the previous turn's total must not follow the row into waiting"
        );
    }

    #[test]
    fn a_real_wait_inside_a_turn_still_keeps_its_figure() {
        // The guard above must not cost the ordinary case anything.
        let (timers, start, id) = run(&[(AgentState::Working, 0), (AgentState::Blocked, 30)]);
        assert_eq!(elapsed_at(&timers, id, start, 300), Some(secs(30)));
    }

    #[test]
    fn the_figure_is_minutes_and_seconds_until_an_hour_then_hours() {
        let cases = [
            (0u64, "00:00"),
            (5, "00:05"),
            (59, "00:59"),
            (60, "01:00"),
            (84, "01:24"),
            (222, "03:42"),
            (3_599, "59:59"),
            (3_600, "1:00:00"),
            (3_661, "1:01:01"),
            (86_399, "23:59:59"),
            (86_400, "24:00:00"),
            (360_000, "100:00:00"),
        ];
        for (seconds, expected) in cases {
            assert_eq!(format_work_duration(secs(seconds)), expected, "{seconds}s");
        }
    }

    #[test]
    fn part_seconds_are_dropped_rather_than_rounded_up() {
        // A figure that rounded up would read ahead of the work done.
        assert_eq!(format_work_duration(Duration::from_millis(999)), "00:00");
        assert_eq!(format_work_duration(Duration::from_millis(1_999)), "00:01");
        assert_eq!(
            format_work_duration(Duration::from_millis(3_599_999)),
            "59:59"
        );
    }
}

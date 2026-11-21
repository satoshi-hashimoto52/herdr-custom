use std::time::Instant;

#[cfg(test)]
use std::time::Duration;

use crossterm::terminal;

use super::{
    background_update_check_enabled, App, AUTO_UPDATE_CHECK_INTERVAL, MIN_RENDER_INTERVAL,
    RESIZE_POLL_INTERVAL, SELECTION_AUTOSCROLL_INTERVAL,
};
fn retain_detached_process_after_wait(
    pid: u32,
    result: std::io::Result<Option<std::process::ExitStatus>>,
) -> bool {
    match result {
        Ok(None) => true,
        Ok(Some(_)) => false,
        Err(err) if err.kind() == std::io::ErrorKind::Interrupted => true,
        Err(err) => {
            tracing::warn!(pid, err = %err, "failed to reap detached process");
            false
        }
    }
}

impl App {
    pub(crate) fn reap_finished_detached_processes(&mut self) {
        self.detached_process_children
            .retain_mut(|child| retain_detached_process_after_wait(child.id(), child.try_wait()));
    }

    pub(crate) fn shutdown_terminal_runtime(&mut self, terminal_id: crate::terminal::TerminalId) {
        let target = super::TerminalInputTarget {
            terminal_id: terminal_id.clone(),
        };
        self.release_input_target_headless(&target);
        if let Some(runtime) = self.terminal_runtimes.remove(&terminal_id) {
            runtime.shutdown();
        }
    }

    pub(crate) fn shutdown_detached_terminal_runtimes(&mut self) {
        let terminal_ids = std::mem::take(&mut self.state.terminal_runtime_shutdowns);
        for terminal_id in terminal_ids {
            self.shutdown_terminal_runtime(terminal_id);
        }
    }

    pub(crate) fn drain_api_requests(&mut self) -> bool {
        let mut changed = false;
        while let Ok(msg) = self.api_rx.try_recv() {
            changed |= self.handle_api_request_message(msg);
            self.shutdown_detached_terminal_runtimes();
        }
        changed
    }

    pub(super) fn handle_api_request_message(
        &mut self,
        msg: crate::api::ApiRequestMessage,
    ) -> bool {
        let previous_mode = self.state.mode;
        let stream_open = match &msg.request.method {
            crate::api::schema::Method::PaneGraphicsStreamOpen(params) => Some(params.clone()),
            _ => None,
        };
        let stream_active = msg.stream_active.clone();
        let mut changed = self.expire_due_metadata(Instant::now());
        changed |= crate::api::request_changes_ui(&msg.request);
        let skip_default_workspace = matches!(
            &msg.request.method,
            crate::api::schema::Method::ServerStop(_)
                | crate::api::schema::Method::ServerLiveHandoff(_)
        );
        if matches!(
            &msg.request.method,
            crate::api::schema::Method::WorktreeCreate(_)
                | crate::api::schema::Method::WorktreeRemove(_)
        ) {
            self.drain_all_internal_events();
            let deferred_changed =
                self.handle_deferred_worktree_api_request(msg.request, msg.respond_to);
            if !skip_default_workspace {
                changed |= self.ensure_default_workspace();
            }
            self.sync_prefix_input_source(previous_mode);
            return changed | deferred_changed;
        }
        let response = self.handle_api_request(msg.request);
        if let (Some(params), Some(active)) = (stream_open.as_ref(), stream_active) {
            self.attach_pane_graphics_stream_active(params, active, &response);
        }
        if !skip_default_workspace {
            changed |= self.ensure_default_workspace();
        }
        let _ = msg.respond_to.send(response);
        self.sync_prefix_input_source(previous_mode);
        changed
    }

    pub(super) async fn handle_raw_input_batch(
        &mut self,
        first: crate::raw_input::RawInputEvent,
    ) -> bool {
        let mut changed = self.handle_raw_input_event(first).await;

        while let Some(rx) = self.input_rx.as_mut() {
            match rx.try_recv() {
                Ok(event) => changed |= self.handle_raw_input_event(event).await,
                Err(tokio::sync::mpsc::error::TryRecvError::Empty) => break,
                Err(tokio::sync::mpsc::error::TryRecvError::Disconnected) => {
                    self.input_rx = None;
                    break;
                }
            }
        }

        changed
    }

    async fn execute_repeat_plan(
        &mut self,
        lease_key: super::input::InputLeaseKey,
        key: crate::input::TerminalKey,
        plan: super::input::RepeatPlan,
    ) -> bool {
        match plan {
            super::input::RepeatPlan::Forwarded(target) => {
                if !self.forward_terminal_key_to_target(&target, key).await {
                    self.input_leases.remove(&lease_key);
                }
                true
            }
            super::input::RepeatPlan::Reprocess {
                context,
                repetitions,
                tracked,
            } => {
                let key = key
                    .with_kind(crossterm::event::KeyEventKind::Repeat)
                    .with_repeat_count(1);
                let mut forwarded_target = None;
                for _ in 0..repetitions {
                    if let Some(target) = &forwarded_target {
                        if !self
                            .forward_terminal_key_to_target(target, key.clone())
                            .await
                        {
                            self.input_leases.remove(&lease_key);
                            break;
                        }
                        continue;
                    }
                    let current_context = self.terminal_input_context();
                    if !self.input_leases.reprocess_allowed(
                        lease_key,
                        &context,
                        current_context.as_ref(),
                        tracked,
                    ) {
                        break;
                    }
                    if let Some(target) = self.handle_key(key.clone()).await {
                        if tracked {
                            self.input_leases.insert_forwarded(
                                lease_key,
                                target.clone(),
                                key.clone(),
                            );
                            forwarded_target = Some(target);
                        }
                    }
                }
                true
            }
            super::input::RepeatPlan::Ignore => false,
        }
    }

    pub(super) async fn handle_raw_input_event(
        &mut self,
        event: crate::raw_input::RawInputEvent,
    ) -> bool {
        let previous_mode = self.state.mode;
        let changed = match event {
            crate::raw_input::RawInputEvent::Key(key) => {
                let lease_key = super::input::InputLeaseKey::new(super::LOCAL_INPUT_SOURCE, &key);
                let key = self.input_leases.normalize_press(&lease_key, key);
                match key.kind {
                    crossterm::event::KeyEventKind::Press => {
                        let initial_context = self.terminal_input_context();
                        let target = self.handle_key(key.clone()).await;
                        let resulting_context = self.terminal_input_context();
                        let plan = self.input_leases.complete_press(
                            lease_key,
                            &key,
                            initial_context.as_ref(),
                            resulting_context.as_ref(),
                            target,
                        );
                        self.execute_repeat_plan(lease_key, key, plan).await;
                        true
                    }
                    crossterm::event::KeyEventKind::Repeat => {
                        let current_context = self.terminal_input_context();
                        let plan = self.input_leases.plan_repeat(
                            lease_key,
                            &key,
                            current_context.as_ref(),
                        );
                        self.execute_repeat_plan(lease_key, key, plan).await
                    }
                    crossterm::event::KeyEventKind::Release => {
                        if let Some(lease) = self.input_leases.remove_forwarded(&lease_key) {
                            let _ = self
                                .forward_terminal_key_to_target(&lease.target, key)
                                .await;
                        }
                        false
                    }
                }
            }
            crate::raw_input::RawInputEvent::Text(text) => {
                self.handle_text_commit(text.into_string()).await;
                true
            }
            crate::raw_input::RawInputEvent::Paste(text) => {
                self.handle_paste(text).await;
                true
            }
            crate::raw_input::RawInputEvent::Mouse(mouse) => {
                let changes_view = !matches!(mouse.kind, crossterm::event::MouseEventKind::Moved)
                    || self.state.mode.mouse_motion_changes_view();
                let sidebar = self.state.view.sidebar_rect;
                let files_geometry = self.state.view.files_geometry.as_ref();
                let in_files_geometry = files_geometry.is_some_and(|geometry| {
                    geometry.area.contains((mouse.column, mouse.row).into())
                });
                let in_sidebar = sidebar.width > 0
                    && sidebar.height > 0
                    && sidebar.contains((mouse.column, mouse.row).into());
                // Button-less motion is traced only while a drag is active
                // (where it matters for divider diagnostics).
                let trace_enabled =
                    (!matches!(mouse.kind, crossterm::event::MouseEventKind::Moved)
                        || self.state.drag.is_some())
                        && super::input::files_trace::begin(&mouse);
                if self.state.popup_pane.is_some()
                    || self.state.mouse_capture
                    || self.state.drag.is_some()
                    || in_sidebar
                    || in_files_geometry
                {
                    if trace_enabled {
                        super::input::files_trace::record_with(|| {
                            format!(
                            "top_level_dispatch=reached route=handle_mouse popup={} capture={} in_sidebar={} in_files_geometry={}",
                            self.state.popup_pane.is_some(),
                            self.state.mouse_capture,
                            in_sidebar,
                            in_files_geometry,
                        )
                        });
                    }
                    self.handle_mouse(mouse);
                } else {
                    if trace_enabled {
                        super::input::files_trace::record(
                            "top_level_dispatch=skipped route=handle_pane_mouse_only",
                        );
                    }
                    self.state
                        .handle_pane_mouse_only(&self.terminal_runtimes, mouse);
                }
                if trace_enabled {
                    super::input::files_trace::finish(changes_view);
                }
                changes_view
            }
            crate::raw_input::RawInputEvent::OuterFocusGained => {
                #[cfg(not(windows))]
                self.query_host_terminal_appearance();
                self.send_outer_focus_event(crate::ghostty::FocusEvent::Gained);
                if self.state.redraw_on_focus_gained {
                    self.request_repaint();
                }
                self.state.outer_terminal_focus = Some(true);
                self.state.mark_active_tab_seen();
                true
            }
            crate::raw_input::RawInputEvent::OuterFocusLost => {
                self.release_input_source(super::LOCAL_INPUT_SOURCE).await;
                self.send_outer_focus_event(crate::ghostty::FocusEvent::Lost);
                self.state.outer_terminal_focus = Some(false);
                false
            }
            crate::raw_input::RawInputEvent::HostDefaultColor { kind, color } => {
                self.update_host_terminal_theme(kind, color)
            }
            crate::raw_input::RawInputEvent::HostPaletteColors { colors } => {
                self.update_host_terminal_palette_colors(&colors)
            }
            crate::raw_input::RawInputEvent::HostColorSchemeChanged(appearance) => {
                self.query_host_terminal_theme();
                self.set_host_terminal_appearance(appearance, true)
            }
            // Cell size reports are consumed by the thin client, not the runtime.
            crate::raw_input::RawInputEvent::HostCellSizeReport { .. } => false,
            crate::raw_input::RawInputEvent::Unsupported => false,
        };
        self.sync_prefix_input_source(previous_mode);
        self.shutdown_detached_terminal_runtimes();
        changed
    }

    fn handle_resize_poll(&mut self) -> bool {
        let Ok(size) = terminal::size() else {
            return false;
        };
        if self.last_terminal_size != Some(size) {
            self.last_terminal_size = Some(size);
            return true;
        }
        false
    }

    pub(crate) fn handle_scheduled_tasks(&mut self, now: Instant, geometry_dirty: bool) -> bool {
        let mut changed = false;
        let mut resized = false;

        if now >= self.next_resize_poll {
            resized = self.handle_resize_poll();
            changed |= resized;
            self.next_resize_poll = now + RESIZE_POLL_INTERVAL;
        }

        if self
            .config_diagnostic_deadline
            .is_some_and(|deadline| now >= deadline)
        {
            self.config_diagnostic_deadline = None;
            self.state.config_diagnostic = None;
            changed = true;
        }

        if self.toast_deadline.is_some_and(|deadline| now >= deadline) {
            self.toast_deadline = None;
            self.state.toast = None;
            changed = true;
        }

        if self
            .state
            .next_pending_agent_notification_deadline()
            .is_some_and(|deadline| now >= deadline)
        {
            let previous_toast = self.state.toast.clone();
            let mut deliveries = self.state.drain_due_agent_notifications(now);
            if !deliveries.is_empty() {
                self.refresh_agent_notification_delivery_contexts(&mut deliveries);
                self.emit_delayed_client_local_agent_notifications(&deliveries);
                self.sync_toast_deadline(previous_toast);
                changed = true;
            }
        }

        if self
            .state
            .next_managed_agent_deadline()
            .is_some_and(|deadline| now >= deadline)
        {
            let panes = self.state.reconcile_managed_agents_at(now);
            if !panes.is_empty() {
                for (ws_idx, pane_id) in panes {
                    self.emit_pane_updated(ws_idx, pane_id);
                }
                self.schedule_session_save();
                changed = true;
            }
        }

        if self
            .copy_feedback_deadline
            .is_some_and(|deadline| now >= deadline)
        {
            self.copy_feedback_deadline = None;
            self.state.copy_feedback = None;
            changed = true;
        }

        if self
            .selection_autoscroll_deadline
            .is_some_and(|deadline| now >= deadline)
        {
            self.tick_selection_autoscroll(now);
            changed = true;
        }

        changed |= self.clear_due_selection_highlight(now);

        self.start_git_status_refresh_if_due(now);

        if self
            .next_auto_update_check
            .is_some_and(|deadline| now >= deadline)
        {
            self.run_auto_update_check();
        }

        if self
            .next_agent_manifest_update_check
            .is_some_and(|deadline| now >= deadline)
        {
            self.run_agent_manifest_update_check();
        }

        if self
            .session_save_deadline
            .is_some_and(|deadline| now >= deadline)
        {
            self.start_background_session_save();
        }

        changed |= self.expire_due_metadata(now);
        changed |= self.handle_tab_bar_status_tasks(now);
        changed |= self.handle_system_resource_refresh(now);
        changed |= self.handle_work_timer_tick(now);
        if !geometry_dirty && !resized {
            changed |= self.handle_files_tasks(now);
        }

        if geometry_dirty || resized {
            self.pending_agent_resume_deadline = None;
        } else {
            self.sync_pending_agent_resume_deadline(now);
            changed |= self.start_pending_agent_resumes(self.pending_agent_resume_due(now));
        }
        changed
    }

    /// Files root sync, scroll clamp and delayed expansion for the active pane.
    /// Runs in the monolithic App loop and, in session mode, in the headless
    /// server loop while an app client is attached (never in both).
    pub(crate) fn handle_files_tasks(&mut self, now: Instant) -> bool {
        // A released Files button goes back to normal once its minimum
        // visible time is over; the event loop wakes for it (see
        // next_files_expand_deadline), nothing sleeps.
        let button_released = self
            .state
            .files_button_press
            .is_some_and(|press| press.expired(now));
        if button_released {
            self.state.files_button_press = None;
        }
        self.handle_files_listing_tasks(now) || button_released
    }

    fn handle_files_listing_tasks(&mut self, now: Instant) -> bool {
        let valid_panes = self
            .state
            .workspaces
            .iter()
            // pane_details() only lists agent panes; Files works on every pane,
            // so plain shell panes must count as valid here.
            .flat_map(|workspace| workspace.tabs.iter())
            .flat_map(|tab| tab.panes.keys().copied())
            .collect::<std::collections::HashSet<_>>();
        self.state
            .files_by_pane
            .retain(|pane_id, _| valid_panes.contains(pane_id));
        if self
            .state
            .files_focus
            .is_some_and(|pane_id| !valid_panes.contains(&pane_id))
        {
            self.state.files_focus = None;
        }
        let Some(ws_idx) = self.state.active else {
            return false;
        };
        let Some(pane_id) = self
            .state
            .workspaces
            .get(ws_idx)
            .and_then(|ws| ws.focused_pane_id())
        else {
            return false;
        };
        if self
            .state
            .files_focus
            .is_some_and(|focused| focused != pane_id)
        {
            self.state.files_focus = None;
        }
        let pane_switched = self.files_synced_pane != Some(pane_id);
        let previous_pane = self.files_synced_pane;
        self.files_synced_pane = Some(pane_id);
        let mut changed = pane_switched;

        // Files root = the selected pane's live cwd, the same source the
        // workspace/agent labels use (Tab::cwd_for_pane).
        let existing_root = self
            .state
            .files_by_pane
            .get(&pane_id)
            .and_then(|state| state.root.clone());
        let (has_runtime, live_cwd, stored_cwd) = self.files_cwd_candidates(ws_idx, pane_id);
        let (candidate, cwd_source) = match (live_cwd, stored_cwd) {
            (Some(live), _) => (Some(live), "runtime-live-cwd"),
            // A running pane whose live cwd is momentarily unreadable keeps
            // the root it already has instead of flipping to a stale value.
            (None, _) if has_runtime && existing_root.is_some() => {
                (existing_root.clone(), "kept-existing-root")
            }
            // No runtime (or no root yet): the stored, OSC 7-updated cwd.
            (None, Some(stored)) => (Some(stored), "terminal-state-cwd"),
            (None, None) => (None, "unavailable"),
        };
        let root = match candidate {
            Some(cwd) if Some(&cwd) == existing_root.as_ref() => Some(cwd),
            // Never adopt a path that does not exist; never canonicalize it.
            Some(cwd) if cwd.is_absolute() && cwd.is_dir() => Some(cwd),
            _ => existing_root.clone(),
        };
        let root_changed = root.is_some() && root != existing_root;
        let reset_reason = match (&existing_root, root_changed) {
            (None, true) => Some("pane-changed"),
            (Some(_), true) => Some("cwd-changed"),
            _ => None,
        };
        if let (Some(reason), Some(root)) = (reset_reason, root.clone()) {
            let before = self.state.files_trace_snapshot(pane_id);
            let old = self.state.files_by_pane.remove(&pane_id);
            let mut state = crate::ui::FilesPaneState::reset(Some(root));
            if let Some(old) = old {
                // Keep generations monotonic so requests for the old root can
                // never match the new state.
                state.focus_generation = old.focus_generation.wrapping_add(1);
                state.request_generation = old.request_generation;
                state.listing_generation = old.listing_generation;
                state.metadata_generation = old.metadata_generation.wrapping_add(1);
            }
            state.schedule_initial_listing(pane_id, now);
            self.state.files_by_pane.insert(pane_id, state);
            self.state
                .trace_files_change(pane_id, reason, "initialize-root", before);
            changed = true;
        }
        if (pane_switched || root_changed) && super::input::files_trace::enabled() {
            super::input::files_trace::state_change(
                if pane_switched {
                    "agent-selected"
                } else {
                    "cwd-changed"
                },
                cwd_source,
                self.state.files_focus,
                &format!(
                    "selected_pane_id_before={previous_pane:?} files_root_before={existing_root:?}"
                ),
                &format!(
                    "task_owner={} selected_pane_id_after={pane_id:?} active_pane_id={pane_id:?} geometry_pane_id={:?} tree={:?} buttons={:?} details={:?} resources={:?} divider={:?} files_split={:.4} cwd_source={cwd_source} cwd_value={root:?} files_root_after={:?} root_changed={root_changed} state_reused={} state_reset={} redraw_requested={changed}",
                    if self.no_session {
                        "monolithic-app(--no-session)"
                    } else {
                        "session-server"
                    },
                    self.state.view.files_geometry.as_ref().map(|geometry| geometry.pane_id),
                    self.state.view.files_geometry.as_ref().map(|geometry| geometry.viewport),
                    self.state.view.files_geometry.as_ref().map(|geometry| geometry.button_row),
                    self.state.view.files_geometry.as_ref().map(|geometry| geometry.details),
                    self.state.view.files_geometry.as_ref().map(|geometry| geometry.resources),
                    self.state.view.files_divider_rect,
                    self.state.files_split,
                    self.state.files_by_pane.get(&pane_id).and_then(|state| state.root.as_ref()),
                    existing_root.is_some() && !root_changed,
                    reset_reason.is_some(),
                ),
            );
        }

        // Viewport shrink: clamp only, never reset to zero.
        let viewport = self
            .state
            .view
            .files_geometry
            .as_ref()
            .filter(|geometry| geometry.pane_id == pane_id)
            .map(|geometry| geometry.viewport.height as usize);
        if let Some(viewport) = viewport {
            let before = self.state.files_trace_snapshot(pane_id);
            if self
                .state
                .files_by_pane
                .get_mut(&pane_id)
                .is_some_and(|state| state.clamp_scroll(viewport))
            {
                self.state
                    .trace_files_change(pane_id, "scroll-clamped", "viewport-shrink", before);
                changed = true;
            }
        }

        let files_focus = self.state.files_focus;
        let Some(state) = self.state.files_by_pane.get_mut(&pane_id) else {
            return changed;
        };
        let mut check = state.check_expand(pane_id, files_focus, now);
        if check == crate::ui::FilesExpandCheck::Ready
            && !state
                .pending_expand
                .as_ref()
                .is_some_and(|request| request.target.is_dir())
        {
            check = crate::ui::FilesExpandCheck::Stale("target_not_directory");
        }
        match check {
            crate::ui::FilesExpandCheck::Idle | crate::ui::FilesExpandCheck::Waiting => changed,
            crate::ui::FilesExpandCheck::Stale(reason) => {
                let before = self.state.files_trace_snapshot(pane_id);
                if let Some(state) = self.state.files_by_pane.get_mut(&pane_id) {
                    state.pending_expand = None;
                }
                self.state.trace_files_change(
                    pane_id,
                    "delayed-expand-rejected-stale",
                    reason,
                    before,
                );
                changed
            }
            crate::ui::FilesExpandCheck::Ready => {
                // Listings run on Tokio's blocking pool; without a runtime
                // (some tests) the request simply stays pending.
                let Ok(runtime) = tokio::runtime::Handle::try_current() else {
                    return changed;
                };
                let before = self.state.files_trace_snapshot(pane_id);
                let Some(request) = self
                    .state
                    .files_by_pane
                    .get_mut(&pane_id)
                    .and_then(|state| state.pending_expand.as_mut())
                else {
                    return changed;
                };
                request.in_flight = true;
                let request = request.clone();
                self.state.trace_files_change(
                    pane_id,
                    "delayed-expand-due",
                    "listing-started",
                    before,
                );
                let event_tx = self.event_tx.clone();
                runtime.spawn_blocking(move || {
                    let result = crate::ui::list_directory(&request.root, &request.target);
                    let _ = event_tx.blocking_send(crate::events::AppEvent::FilesDirectoryListed {
                        request,
                        result,
                    });
                });
                changed
            }
        }
    }

    /// (has runtime, live runtime cwd, stored terminal cwd) for a pane. The
    /// live value is the OSC 7 report or the pane process's cwd, the same
    /// source as `Tab::cwd_for_pane`; no external command runs.
    fn files_cwd_candidates(
        &self,
        ws_idx: usize,
        pane_id: crate::layout::PaneId,
    ) -> (bool, Option<std::path::PathBuf>, Option<std::path::PathBuf>) {
        let Some(terminal_id) = self
            .state
            .workspaces
            .get(ws_idx)
            .and_then(|ws| ws.terminal_id(pane_id))
        else {
            return (false, None, None);
        };
        let runtime = self.terminal_runtimes.get(terminal_id);
        let live = runtime.and_then(crate::terminal::TerminalRuntime::cwd);
        let stored = self
            .state
            .terminals
            .get(terminal_id)
            .map(|terminal| terminal.cwd.clone());
        (runtime.is_some(), live, stored)
    }

    /// Next Files deadline for the event-loop wakeup: the active pane's due
    /// delayed expansion, or the end of a released button's pressed look.
    pub(crate) fn next_files_expand_deadline(&self) -> Option<Instant> {
        let press = self
            .state
            .files_button_press
            .filter(|press| !press.held)
            .map(|press| press.visible_until);
        [press, self.next_files_listing_deadline()]
            .into_iter()
            .flatten()
            .min()
    }

    fn next_files_listing_deadline(&self) -> Option<Instant> {
        let pane_id = self
            .state
            .active
            .and_then(|idx| self.state.workspaces.get(idx)?.focused_pane_id())?;
        self.state
            .files_by_pane
            .get(&pane_id)?
            .pending_expand
            .as_ref()
            .filter(|request| !request.in_flight)
            .map(|request| request.due_at)
    }

    /// Clears temporary copied-token highlights, such as after double-click copy.
    pub(crate) fn clear_due_selection_highlight(&mut self, now: Instant) -> bool {
        if self
            .selection_highlight_clear_deadline
            .is_none_or(|deadline| now < deadline)
        {
            return false;
        }

        self.selection_highlight_clear_deadline = None;
        if self
            .state
            .selection
            .as_ref()
            .is_some_and(|selection| !selection.is_in_progress())
        {
            self.state.clear_selection();
            return true;
        }
        false
    }

    pub(crate) fn sync_agent_metadata_deadline(&mut self) {
        self.agent_metadata_deadline = self.state.next_agent_metadata_expiry();
    }

    pub(crate) fn expire_due_metadata(&mut self, now: Instant) -> bool {
        let Some(deadline) = self
            .agent_metadata_deadline
            .filter(|deadline| now >= *deadline)
        else {
            return false;
        };
        self.expire_metadata_at(deadline, now);
        true
    }

    pub(crate) fn expire_metadata_at(&mut self, deadline: Instant, now: Instant) {
        let previous_toast = self.state.toast.clone();
        for update in self.state.expire_agent_metadata_at(deadline, now) {
            self.refresh_new_herdr_toast_context_for_update(&update, &previous_toast);
            self.emit_pane_state_update(&update);
        }
        let (panes, workspaces) = self.state.expire_metadata_tokens(now);
        for (ws_idx, pane_id) in panes {
            self.emit_pane_updated(ws_idx, pane_id);
        }
        for ws_idx in workspaces {
            self.emit_workspace_token_updated(ws_idx);
        }
        self.sync_agent_metadata_deadline();
    }

    pub(crate) fn tick_selection_autoscroll(&mut self, now: Instant) {
        let Some(autoscroll) = self.state.selection_autoscroll.clone() else {
            // Self-heal: state cleared but deadline leaked
            self.selection_autoscroll_deadline = None;
            return;
        };

        // Selection must still be in progress for autoscroll to continue
        let Some(pane_id) = self.state.selection.as_ref().map(|s| s.pane_id) else {
            self.stop_selection_autoscroll();
            return;
        };
        if !self
            .state
            .selection
            .as_ref()
            .is_some_and(|s| s.is_dragging())
        {
            self.stop_selection_autoscroll();
            return;
        }

        // Rect-change detection: if inner_rect changed since drag, stop
        let current_rect = self
            .state
            .pane_info_by_id(pane_id)
            .map(|info| info.inner_rect);
        if current_rect != Some(autoscroll.inner_rect) {
            self.stop_selection_autoscroll();
            return;
        }

        // Scrollback boundary detection via ScrollMetrics — fail-closed if unavailable
        let Some(metrics) = self
            .state
            .pane_scroll_metrics(&self.terminal_runtimes, pane_id)
        else {
            self.stop_selection_autoscroll();
            return;
        };
        match autoscroll.direction {
            crate::app::state::SelectionAutoscrollDirection::Up => {
                let at_top = metrics.offset_from_bottom >= metrics.max_offset_from_bottom;
                if at_top {
                    self.stop_selection_autoscroll();
                    return;
                }
                self.state
                    .scroll_pane_up(&self.terminal_runtimes, pane_id, 1);
            }
            crate::app::state::SelectionAutoscrollDirection::Down => {
                let at_bottom = metrics.offset_from_bottom == 0;
                if at_bottom {
                    self.stop_selection_autoscroll();
                    return;
                }
                self.state
                    .scroll_pane_down(&self.terminal_runtimes, pane_id, 1);
            }
        }

        // Extend selection cursor to last known mouse position
        self.state.update_selection_cursor(
            &self.terminal_runtimes,
            pane_id,
            autoscroll.last_mouse_screen_col,
            autoscroll.last_mouse_screen_row,
        );

        // Reschedule
        self.selection_autoscroll_deadline = Some(now + SELECTION_AUTOSCROLL_INTERVAL);
    }

    pub(crate) fn stop_selection_autoscroll(&mut self) {
        self.state.stop_selection_autoscroll_state();
        self.selection_autoscroll_deadline = None;
    }

    pub(crate) fn can_render_now(&self, now: Instant) -> bool {
        match self.last_render_at {
            Some(last_render_at) => now.duration_since(last_render_at) >= MIN_RENDER_INTERVAL,
            None => true,
        }
    }

    pub(crate) fn can_present_now(&self, now: Instant) -> bool {
        match self.last_presentation_at {
            Some(last_presentation_at) => {
                now.duration_since(last_presentation_at) >= MIN_RENDER_INTERVAL
            }
            None => true,
        }
    }

    pub(crate) fn record_render_attempt(&mut self, now: Instant, presentation: bool) {
        self.last_render_at = Some(now);
        if presentation {
            self.last_presentation_at = Some(now);
        }
    }

    pub(crate) fn run_auto_update_check(&mut self) {
        if !background_update_check_enabled(self.no_session, self.update_version_check_enabled) {
            self.next_auto_update_check = None;
            return;
        }

        self.next_auto_update_check = self
            .state
            .update_available
            .is_none()
            .then_some(Instant::now() + AUTO_UPDATE_CHECK_INTERVAL);

        if self.state.update_available.is_some() {
            return;
        }

        let update_tx = self.event_tx.clone();
        std::thread::spawn(move || crate::update::auto_update(update_tx));
    }

    pub(crate) fn run_agent_manifest_update_check(&mut self) {
        if !background_update_check_enabled(self.no_session, self.update_manifest_check_enabled) {
            self.next_agent_manifest_update_check = None;
            return;
        }

        self.next_agent_manifest_update_check = Some(Instant::now() + AUTO_UPDATE_CHECK_INTERVAL);

        let manifest_update_tx = self.event_tx.clone();
        std::thread::spawn(move || crate::detect::manifest_update::auto_update(manifest_update_tx));
    }

    pub(crate) fn next_loop_deadline(&self, now: Instant, needs_render: bool) -> Option<Instant> {
        self.next_loop_deadline_with_resize_poll(now, needs_render, true, true)
    }

    pub(crate) fn next_headless_loop_deadline_with_git_refresh(
        &self,
        now: Instant,
        needs_render: bool,
        include_git_refresh: bool,
    ) -> Option<Instant> {
        self.next_loop_deadline_with_resize_poll(now, needs_render, false, include_git_refresh)
    }

    fn next_loop_deadline_with_resize_poll(
        &self,
        now: Instant,
        needs_render: bool,
        include_resize_poll: bool,
        // Gates refreshes that only matter while a client is watching the UI:
        // sidebar Git status, the sidebar's host resource footer, and the
        // ticking working time on Agent rows.
        include_client_visible_refresh: bool,
    ) -> Option<Instant> {
        let render_deadline = if needs_render {
            self.last_render_at
                .map(|last_render_at| last_render_at + MIN_RENDER_INTERVAL)
                .filter(|deadline| *deadline > now)
        } else {
            None
        };

        [
            include_resize_poll.then_some(self.next_resize_poll),
            self.config_diagnostic_deadline,
            self.toast_deadline,
            self.state.next_pending_agent_notification_deadline(),
            self.state.next_managed_agent_deadline(),
            self.copy_feedback_deadline,
            include_client_visible_refresh
                .then(|| self.git_refresh_deadline())
                .flatten(),
            self.next_auto_update_check,
            self.next_agent_manifest_update_check,
            self.agent_metadata_deadline,
            self.pending_agent_resume_deadline,
            self.session_save_deadline,
            self.selection_autoscroll_deadline,
            self.selection_highlight_clear_deadline,
            include_client_visible_refresh
                .then(|| self.next_files_expand_deadline())
                .flatten(),
            self.next_tab_bar_status_deadline(),
            include_client_visible_refresh
                .then(|| self.next_system_resource_deadline())
                .flatten(),
            include_client_visible_refresh
                .then(|| self.next_work_timer_deadline())
                .flatten(),
            render_deadline,
        ]
        .into_iter()
        .flatten()
        .min()
    }

    pub(crate) fn drain_internal_events(&mut self) -> bool {
        self.drain_internal_events_up_to(super::APP_EVENT_DRAIN_LIMIT)
            .1
    }

    pub(crate) fn drain_all_internal_events(&mut self) -> bool {
        let mut changed = false;
        loop {
            let (had_event, batch_changed) =
                self.drain_internal_events_up_to(super::APP_EVENT_DRAIN_LIMIT);
            changed |= batch_changed;
            if !had_event {
                break;
            }
        }
        changed
    }

    fn drain_internal_events_up_to(&mut self, limit: usize) -> (bool, bool) {
        let mut had_event = false;
        let mut changed = false;
        for _ in 0..limit {
            let Ok(ev) = self.event_rx.try_recv() else {
                break;
            };
            had_event = true;
            changed |= self.handle_internal_event_with_prefix_sync(ev);
        }
        (had_event, changed)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::app::state;
    use crate::workspace::Workspace;

    #[test]
    fn hidden_render_attempt_keeps_presentation_cadence_available() {
        let (mut app, _) = test_app_with_pane();
        let initial_presentation = Instant::now();
        app.record_render_attempt(initial_presentation, true);

        let hidden_attempt = initial_presentation + MIN_RENDER_INTERVAL;
        app.record_render_attempt(hidden_attempt, false);
        let foreground_echo = hidden_attempt + Duration::from_millis(1);

        assert!(!app.can_render_now(foreground_echo));
        assert!(app.can_present_now(foreground_echo));
    }

    #[test]
    fn interrupted_detached_process_wait_keeps_child_for_retry() {
        let interrupted = std::io::Error::new(std::io::ErrorKind::Interrupted, "test interrupt");

        assert!(retain_detached_process_after_wait(42, Err(interrupted)));
    }

    fn test_app_with_pane() -> (super::super::App, crate::layout::PaneId) {
        let mut app = super::super::App::new(
            &crate::config::Config::default(),
            true,
            None,
            tokio::sync::mpsc::unbounded_channel().1,
            crate::api::EventHub::default(),
        );
        let ws = Workspace::test_new("test");
        let pane_id = ws.tabs[0].root_pane;
        app.state.workspaces.push(ws);
        app.state.active = Some(0);
        app.state.view.pane_infos.push(crate::layout::PaneInfo {
            id: pane_id,
            rect: ratatui::layout::Rect::new(0, 0, 80, 24),
            inner_rect: ratatui::layout::Rect::new(0, 0, 80, 24),
            scrollbar_rect: None,
            borders: ratatui::widgets::Borders::NONE,
            is_focused: true,
        });
        (app, pane_id)
    }

    #[test]
    fn tick_selection_autoscroll_stops_when_metrics_unavailable() {
        // Without a runtime, pane_scroll_metrics returns None.
        // Fail-closed: stop autoscroll instead of rescheduling forever.
        let (mut app, pane_id) = test_app_with_pane();
        let now = Instant::now();
        let mut sel = crate::selection::Selection::anchor(pane_id, 0, 0, None);
        // Drag to a different cell so it becomes Dragging
        sel.drag(5, 5, ratatui::layout::Rect::new(0, 0, 80, 24), None);
        app.state.selection = Some(sel);
        app.state.selection_autoscroll = Some(state::SelectionAutoscroll {
            direction: state::SelectionAutoscrollDirection::Down,
            last_mouse_screen_col: 5,
            last_mouse_screen_row: 23,
            inner_rect: ratatui::layout::Rect::new(0, 0, 80, 24),
        });
        app.selection_autoscroll_deadline = Some(now);
        app.tick_selection_autoscroll(now);
        // Should stop because no runtime metrics available
        assert!(app.state.selection_autoscroll.is_none());
        assert!(app.selection_autoscroll_deadline.is_none());
    }

    #[test]
    fn tick_selection_autoscroll_stops_when_selection_done() {
        let (mut app, pane_id) = test_app_with_pane();
        let now = Instant::now();
        // Create a selection that is already finished (not in progress)
        let mut sel = crate::selection::Selection::anchor(pane_id, 0, 0, None);
        // Drag to a different cell so it becomes visible, then finish
        sel.drag(5, 5, ratatui::layout::Rect::new(0, 0, 80, 24), None);
        sel.finish(); // now it's Done, not in progress
        app.state.selection = Some(sel);
        app.state.selection_autoscroll = Some(state::SelectionAutoscroll {
            direction: state::SelectionAutoscrollDirection::Down,
            last_mouse_screen_col: 0,
            last_mouse_screen_row: 23,
            inner_rect: ratatui::layout::Rect::new(0, 0, 80, 24),
        });
        app.selection_autoscroll_deadline = Some(now);
        app.tick_selection_autoscroll(now);
        assert!(app.state.selection_autoscroll.is_none());
        assert!(app.selection_autoscroll_deadline.is_none());
    }

    #[test]
    fn tick_selection_autoscroll_stops_when_selection_cleared() {
        let (mut app, _pane_id) = test_app_with_pane();
        let now = Instant::now();
        app.state.selection = None;
        app.state.selection_autoscroll = Some(state::SelectionAutoscroll {
            direction: state::SelectionAutoscrollDirection::Down,
            last_mouse_screen_col: 0,
            last_mouse_screen_row: 23,
            inner_rect: ratatui::layout::Rect::new(0, 0, 80, 24),
        });
        app.selection_autoscroll_deadline = Some(now);
        app.tick_selection_autoscroll(now);
        assert!(app.state.selection_autoscroll.is_none());
        assert!(app.selection_autoscroll_deadline.is_none());
    }

    #[test]
    fn tick_selection_autoscroll_stops_when_selection_anchored() {
        // Anchored (click, no drag) should not keep the timer running.
        let (mut app, pane_id) = test_app_with_pane();
        let now = Instant::now();
        app.state.selection = Some(crate::selection::Selection::anchor(pane_id, 0, 0, None));
        app.state.selection_autoscroll = Some(state::SelectionAutoscroll {
            direction: state::SelectionAutoscrollDirection::Down,
            last_mouse_screen_col: 0,
            last_mouse_screen_row: 23,
            inner_rect: ratatui::layout::Rect::new(0, 0, 80, 24),
        });
        app.selection_autoscroll_deadline = Some(now);
        app.tick_selection_autoscroll(now);
        assert!(app.state.selection_autoscroll.is_none());
        assert!(app.selection_autoscroll_deadline.is_none());
    }

    /// Creates an app with a real TerminalRuntime (no PTY) so scroll_metrics
    /// returns meaningful data. Uses test_with_scrollback_bytes.
    fn test_app_with_runtime(
        cols: u16,
        rows: u16,
        bytes: &[u8],
    ) -> (super::super::App, crate::layout::PaneId) {
        let mut app = super::super::App::new(
            &crate::config::Config::default(),
            true,
            None,
            tokio::sync::mpsc::unbounded_channel().1,
            crate::api::EventHub::default(),
        );
        let mut ws = Workspace::test_new("test");
        let pane_id = ws.tabs[0].root_pane;
        let runtime =
            crate::terminal::TerminalRuntime::test_with_scrollback_bytes(cols, rows, 0, bytes);
        ws.tabs[0].runtimes.insert(pane_id, runtime);
        app.state.workspaces.push(ws);
        app.state.active = Some(0);
        app.state.view.pane_infos.push(crate::layout::PaneInfo {
            id: pane_id,
            rect: ratatui::layout::Rect::new(0, 0, cols, rows),
            inner_rect: ratatui::layout::Rect::new(0, 0, cols, rows),
            scrollbar_rect: None,
            borders: ratatui::widgets::Borders::NONE,
            is_focused: true,
        });
        (app, pane_id)
    }

    #[tokio::test]
    async fn tick_selection_autoscroll_stops_at_scrollback_top() {
        // Create a runtime with no scrollback content — we're already at
        // the top (offset_from_bottom == max_offset_from_bottom).
        let (mut app, pane_id) = test_app_with_runtime(80, 24, &[]);
        let now = Instant::now();
        let mut sel = crate::selection::Selection::anchor(pane_id, 5, 5, None);
        sel.drag(0, 0, ratatui::layout::Rect::new(0, 0, 80, 24), None);
        app.state.selection = Some(sel);
        app.state.selection_autoscroll = Some(state::SelectionAutoscroll {
            direction: state::SelectionAutoscrollDirection::Up,
            last_mouse_screen_col: 0,
            last_mouse_screen_row: 0,
            inner_rect: ratatui::layout::Rect::new(0, 0, 80, 24),
        });
        app.selection_autoscroll_deadline = Some(now);
        app.tick_selection_autoscroll(now);
        // At scrollback top, can't scroll further up — should stop
        assert!(app.state.selection_autoscroll.is_none());
        assert!(app.selection_autoscroll_deadline.is_none());
    }

    #[tokio::test]
    async fn tick_selection_autoscroll_stops_at_scrollback_bottom() {
        // Create a runtime with no scrollback content — we're already at
        // the bottom (offset_from_bottom == 0).
        let (mut app, pane_id) = test_app_with_runtime(80, 24, &[]);
        let now = Instant::now();
        let mut sel = crate::selection::Selection::anchor(pane_id, 0, 0, None);
        sel.drag(5, 5, ratatui::layout::Rect::new(0, 0, 80, 24), None);
        app.state.selection = Some(sel);
        app.state.selection_autoscroll = Some(state::SelectionAutoscroll {
            direction: state::SelectionAutoscrollDirection::Down,
            last_mouse_screen_col: 5,
            last_mouse_screen_row: 23,
            inner_rect: ratatui::layout::Rect::new(0, 0, 80, 24),
        });
        app.selection_autoscroll_deadline = Some(now);
        app.tick_selection_autoscroll(now);
        // At scrollback bottom, can't scroll further down — should stop
        assert!(app.state.selection_autoscroll.is_none());
        assert!(app.selection_autoscroll_deadline.is_none());
    }

    #[tokio::test]
    async fn passive_mouse_motion_does_not_request_monolithic_render() {
        let (mut app, _) = test_app_with_pane();
        app.state.mode = crate::app::Mode::Terminal;
        let motion = || {
            crate::raw_input::RawInputEvent::Mouse(crossterm::event::MouseEvent {
                kind: crossterm::event::MouseEventKind::Moved,
                column: 10,
                row: 5,
                modifiers: crossterm::event::KeyModifiers::empty(),
            })
        };

        assert!(!app.handle_raw_input_event(motion()).await);
        app.state.mode = crate::app::Mode::GlobalMenu;
        assert!(app.handle_raw_input_event(motion()).await);
    }

    #[tokio::test]
    async fn top_level_mouse_dispatch_reaches_files_geometry() {
        let (mut app, pane_id) = test_app_with_pane();
        let root = std::path::PathBuf::from("/tmp/herdr-files-test");
        let child = root.join("child");
        let mut files = crate::ui::FilesPaneState::reset(Some(root.clone()));
        files.accept_listing(
            &root,
            vec![crate::ui::FileEntry {
                path: child.clone(),
                name: "child".into(),
                is_dir: true,
            }],
        );
        app.state.files_by_pane.insert(pane_id, files);
        let area = FILES_VIEW;
        app.state.mode = crate::app::Mode::Terminal;
        crate::ui::compute_view(&mut app.state, area);

        let geometry = app
            .state
            .view
            .files_geometry
            .clone()
            .expect("rendered Files area should produce input geometry");
        let hit = geometry
            .rows
            .iter()
            .find(|hit| hit.path == child)
            .expect("Files should contain the child row");
        assert!(geometry.area.contains((hit.name.x, hit.name.y).into()));
        assert!(geometry.viewport.height > 0);
        app.state.view.sidebar_rect = ratatui::layout::Rect::default();

        let changed = app
            .handle_raw_input_event(crate::raw_input::RawInputEvent::Mouse(
                crossterm::event::MouseEvent {
                    kind: crossterm::event::MouseEventKind::Down(
                        crossterm::event::MouseButton::Left,
                    ),
                    column: hit.name.x,
                    row: hit.name.y,
                    modifiers: crossterm::event::KeyModifiers::empty(),
                },
            ))
            .await;

        assert!(changed, "a Files click must request a redraw");
        assert_eq!(app.state.files_focus, Some(pane_id));
        assert_eq!(
            app.state
                .files_by_pane
                .get(&pane_id)
                .and_then(|state| state.focused_dir.as_ref()),
            Some(&child)
        );

        // The row right above Files is now the Agents/Files divider (a resize
        // handle), so "outside Files" is the Agents row above the divider.
        let outside_files = ratatui::layout::Rect::new(
            geometry.divider.x,
            geometry.divider.y.saturating_sub(1),
            geometry.divider.width.min(1),
            1,
        );
        assert!(!geometry
            .area
            .contains((outside_files.x, outside_files.y).into()));
        app.state.files_focus = Some(pane_id);
        let changed = app
            .handle_raw_input_event(crate::raw_input::RawInputEvent::Mouse(
                crossterm::event::MouseEvent {
                    kind: crossterm::event::MouseEventKind::Down(
                        crossterm::event::MouseButton::Left,
                    ),
                    column: outside_files.x,
                    row: outside_files.y,
                    modifiers: crossterm::event::KeyModifiers::empty(),
                },
            ))
            .await;
        assert!(changed);
        assert_eq!(app.state.files_focus, None);
    }

    /// Regression: on a plain shell pane (no agent, e.g. `--no-session`), the
    /// periodic Files task used to treat the pane as invalid, clearing
    /// `files_focus` and resetting the pane's Files state on every tick.
    #[tokio::test]
    async fn files_click_focus_survives_scheduled_tick_on_plain_shell_pane() {
        let (mut app, pane_id) = test_app_with_pane();
        app.state.ensure_test_terminals();
        let terminal_id = app.state.workspaces[0]
            .terminal_id(pane_id)
            .cloned()
            .expect("test pane should have a terminal");
        let root = app.state.terminals[&terminal_id].cwd.clone();
        assert!(
            app.state.workspaces[0]
                .pane_details(&app.state.terminals)
                .is_empty(),
            "precondition: the pane is a plain shell pane without agent details"
        );
        let first = root.join("first");
        let second = root.join("second");
        let mut files = crate::ui::FilesPaneState::reset(Some(root.clone()));
        let entries = (0..30)
            .map(|idx| crate::ui::FileEntry {
                path: if idx == 0 {
                    first.clone()
                } else if idx == 1 {
                    second.clone()
                } else {
                    root.join(format!("dir{idx:02}"))
                },
                name: match idx {
                    0 => "first".into(),
                    1 => "second".into(),
                    _ => format!("dir{idx:02}"),
                },
                is_dir: true,
            })
            .collect();
        files.accept_listing(&root, entries);
        app.state.files_by_pane.insert(pane_id, files);
        app.state.mode = crate::app::Mode::Terminal;
        crate::ui::compute_view(&mut app.state, FILES_VIEW);
        let geometry = app
            .state
            .view
            .files_geometry
            .clone()
            .expect("rendered Files area should produce input geometry");
        let hit = geometry
            .rows
            .iter()
            .find(|hit| hit.path == first)
            .expect("Files should contain the first row");
        let center =
            |rect: ratatui::layout::Rect| (rect.x + rect.width / 2, rect.y + rect.height / 2);
        let mouse = |kind, (column, row): (u16, u16)| {
            crate::raw_input::RawInputEvent::Mouse(crossterm::event::MouseEvent {
                kind,
                column,
                row,
                modifiers: crossterm::event::KeyModifiers::empty(),
            })
        };
        let left = crossterm::event::MouseButton::Left;

        assert!(
            app.handle_raw_input_event(mouse(
                crossterm::event::MouseEventKind::Down(left),
                center(hit.row)
            ))
            .await
        );
        app.handle_raw_input_event(mouse(
            crossterm::event::MouseEventKind::Up(left),
            center(hit.row),
        ))
        .await;
        app.handle_files_tasks(Instant::now());

        assert_eq!(app.state.files_focus, Some(pane_id));
        assert_eq!(
            app.state.files_by_pane[&pane_id].focused_dir.as_ref(),
            Some(&first)
        );

        assert!(app
            .handle_key(crate::input::TerminalKey::new(
                crossterm::event::KeyCode::Char('j'),
                crossterm::event::KeyModifiers::empty(),
            ))
            .await
            .is_none());
        app.handle_files_tasks(Instant::now());
        assert_eq!(app.state.files_focus, Some(pane_id));
        assert_eq!(
            app.state.files_by_pane[&pane_id].focused_dir.as_ref(),
            Some(&second)
        );

        assert!(
            app.handle_raw_input_event(mouse(
                crossterm::event::MouseEventKind::ScrollDown,
                center(geometry.viewport)
            ))
            .await
        );
        let scrolled = app.state.files_by_pane[&pane_id].scroll;
        assert!(scrolled > 0, "viewport wheel should scroll Files");
        app.handle_files_tasks(Instant::now());
        assert_eq!(app.state.files_by_pane[&pane_id].scroll, scrolled);
        assert_eq!(
            app.state.files_by_pane[&pane_id].focused_dir.as_ref(),
            Some(&second),
            "scrolling must not change the focused directory"
        );
    }

    /// Real on-disk tree used as the pane cwd. The path is deliberately left
    /// non-canonical (macOS temp dirs live behind the `/var` symlink).
    struct FilesFixtureDir(std::path::PathBuf);

    impl Drop for FilesFixtureDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    // Tall enough that the tree keeps several rows above the fixed button
    // row and the six reserved detail rows.
    const FILES_VIEW: ratatui::layout::Rect = ratatui::layout::Rect {
        x: 0,
        y: 0,
        width: 106,
        height: 60,
    };

    fn files_mouse(
        kind: crossterm::event::MouseEventKind,
        (column, row): (u16, u16),
    ) -> crate::raw_input::RawInputEvent {
        crate::raw_input::RawInputEvent::Mouse(crossterm::event::MouseEvent {
            kind,
            column,
            row,
            modifiers: crossterm::event::KeyModifiers::empty(),
        })
    }

    fn files_left_down(position: (u16, u16)) -> crate::raw_input::RawInputEvent {
        files_mouse(
            crossterm::event::MouseEventKind::Down(crossterm::event::MouseButton::Left),
            position,
        )
    }

    fn rect_center(rect: ratatui::layout::Rect) -> (u16, u16) {
        (rect.x + rect.width / 2, rect.y + rect.height / 2)
    }

    /// App whose active plain-shell pane has a real directory tree as cwd:
    /// `dNN/` directories (each holding `inner/` and `note.txt`) and one file.
    async fn files_fixture(
        name: &str,
        dirs: usize,
    ) -> (
        super::super::App,
        crate::layout::PaneId,
        std::path::PathBuf,
        FilesFixtureDir,
    ) {
        let root = std::env::temp_dir().join(format!("herdr-files-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        for idx in 0..dirs {
            let dir = root.join(format!("d{idx:02}"));
            std::fs::create_dir_all(dir.join("inner")).expect("fixture dir");
            std::fs::write(dir.join("note.txt"), "x").expect("fixture file");
        }
        std::fs::write(root.join("zz-file.txt"), "x").expect("fixture file");
        let guard = FilesFixtureDir(root.clone());

        let (mut app, pane_id) = test_app_with_pane();
        app.state.ensure_test_terminals();
        let terminal_id = app.state.workspaces[0]
            .terminal_id(pane_id)
            .cloned()
            .expect("test pane should have a terminal");
        app.state
            .terminals
            .get_mut(&terminal_id)
            .expect("terminal")
            .cwd = root.clone();
        app.state.mode = crate::app::Mode::Terminal;
        // Pane initialization lists the root immediately (no 2 s delay).
        settle_files(&mut app, pane_id, Instant::now()).await;
        assert!(app.state.files_by_pane[&pane_id]
            .directories
            .contains_key(&root));
        crate::ui::compute_view(&mut app.state, FILES_VIEW);
        (app, pane_id, root, guard)
    }

    /// Runs the scheduled Files task at `now` and waits for any listing it
    /// started to come back through the real internal event channel.
    async fn settle_files(
        app: &mut super::super::App,
        pane_id: crate::layout::PaneId,
        now: Instant,
    ) {
        app.handle_files_tasks(now);
        for _ in 0..500 {
            let in_flight = app
                .state
                .files_by_pane
                .get(&pane_id)
                .and_then(|state| state.pending_expand.as_ref())
                .is_some_and(|request| request.in_flight);
            if !in_flight {
                break;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
            app.drain_internal_events();
        }
        app.handle_files_tasks(now);
        crate::ui::compute_view(&mut app.state, FILES_VIEW);
    }

    fn files_geometry(app: &super::super::App) -> crate::ui::FilesHitGeometry {
        app.state
            .view
            .files_geometry
            .clone()
            .expect("rendered Files area should produce input geometry")
    }

    fn files_row(app: &super::super::App, path: &std::path::Path) -> ratatui::layout::Rect {
        files_geometry(app)
            .rows
            .iter()
            .find(|hit| hit.path == path)
            .map(|hit| hit.row)
            .unwrap_or_else(|| panic!("{path:?} should be a rendered Files row"))
    }

    fn depth2_rows(
        app: &super::super::App,
        pane_id: crate::layout::PaneId,
    ) -> Vec<std::path::PathBuf> {
        app.state.files_by_pane[&pane_id]
            .visible_rows()
            .into_iter()
            .filter(|row| row.depth >= 2)
            .map(|row| row.path)
            .collect()
    }

    #[tokio::test]
    async fn files_scroll_survives_ticks_renders_and_listings_without_moving_focus() {
        let (mut app, pane_id, root, _guard) = files_fixture("scroll", 30).await;
        // Enter Files focus on the root row: focus stays on root, nothing pending.
        let root_row = files_row(&app, &root);
        app.handle_raw_input_event(files_left_down(rect_center(root_row)))
            .await;
        assert_eq!(app.state.files_focus, Some(pane_id));
        let viewport = files_geometry(&app).viewport;
        for _ in 0..8 {
            assert!(
                app.handle_raw_input_event(files_mouse(
                    crossterm::event::MouseEventKind::ScrollDown,
                    rect_center(viewport),
                ))
                .await
            );
            crate::ui::compute_view(&mut app.state, FILES_VIEW);
        }
        let state = &app.state.files_by_pane[&pane_id];
        let scrolled = state.scroll;
        assert_eq!(scrolled, 8);
        assert_eq!(state.focused_dir.as_ref(), Some(&root));
        assert_eq!(state.expanded_dir.as_ref(), Some(&root));
        assert_eq!(state.pending_expand, None);

        let start = Instant::now();
        for step in 1..=6 {
            settle_files(&mut app, pane_id, start + Duration::from_secs(step * 3)).await;
        }
        // A listing that arrives without a matching pending request is discarded.
        let stray = crate::ui::FilesExpandRequest {
            pane_id,
            root: root.clone(),
            target: root.clone(),
            focus_generation: app.state.files_by_pane[&pane_id].focus_generation,
            request_generation: app.state.files_by_pane[&pane_id].request_generation,
            scheduled_at: start,
            due_at: start,
            requires_files_focus: false,
            in_flight: true,
        };
        app.handle_internal_event_with_render_impact(
            crate::events::AppEvent::FilesDirectoryListed {
                request: stray,
                result: Ok(Vec::new()),
            },
        );
        crate::ui::compute_view(&mut app.state, FILES_VIEW);
        let state = &app.state.files_by_pane[&pane_id];
        assert_eq!(
            state.scroll, scrolled,
            "ticks/renders/listings must not reset scroll"
        );
        assert_eq!(state.focused_dir.as_ref(), Some(&root));
        assert_eq!(state.expanded_dir.as_ref(), Some(&root));
        assert_eq!(state.pending_expand, None);
        assert!(!state.visible_rows().is_empty());

        // An explicit click on a visible row and its applied listing keep the
        // offset too, because children are inserted below the clicked row.
        let target = files_geometry(&app).rows[2].path.clone();
        assert!(target.is_dir());
        app.handle_raw_input_event(files_left_down(rect_center(files_row(&app, &target))))
            .await;
        settle_files(&mut app, pane_id, Instant::now() + Duration::from_secs(3)).await;
        let state = &app.state.files_by_pane[&pane_id];
        assert_eq!(state.expanded_dir.as_ref(), Some(&target));
        assert_eq!(state.scroll, scrolled);
        // Esc returns to the PTY but keeps the Files position.
        app.handle_key(crate::input::TerminalKey::new(
            crossterm::event::KeyCode::Esc,
            crossterm::event::KeyModifiers::empty(),
        ))
        .await;
        assert_eq!(app.state.files_focus, None);
        let state = &app.state.files_by_pane[&pane_id];
        assert_eq!(state.scroll, scrolled);
        assert_eq!(state.focused_dir.as_ref(), Some(&target));
    }

    #[tokio::test]
    async fn files_bottom_row_is_never_expanded_by_scroll_or_hover() {
        let (mut app, pane_id, root, _guard) = files_fixture("bottom", 30).await;
        let viewport = files_geometry(&app).viewport;
        for _ in 0..60 {
            app.handle_raw_input_event(files_mouse(
                crossterm::event::MouseEventKind::ScrollDown,
                rect_center(viewport),
            ))
            .await;
        }
        crate::ui::compute_view(&mut app.state, FILES_VIEW);
        let geometry = files_geometry(&app);
        let bottom = geometry.rows.last().expect("bottom row").clone();
        let bottom_dir = geometry
            .rows
            .iter()
            .rev()
            .find(|hit| hit.is_dir)
            .expect("a directory near the bottom")
            .clone();
        for hit in [&bottom, &bottom_dir] {
            app.handle_raw_input_event(files_mouse(
                crossterm::event::MouseEventKind::Moved,
                rect_center(hit.row),
            ))
            .await;
        }
        let scrolled = app.state.files_by_pane[&pane_id].scroll;
        assert!(scrolled > 0);
        let start = Instant::now();
        for step in 1..=5 {
            settle_files(&mut app, pane_id, start + Duration::from_secs(step * 2)).await;
        }
        let state = &app.state.files_by_pane[&pane_id];
        assert_eq!(
            app.state.files_focus, None,
            "hover never takes keyboard focus"
        );
        assert_eq!(state.focused_dir.as_ref(), Some(&root));
        assert_eq!(state.expanded_dir.as_ref(), Some(&root));
        assert_eq!(state.pending_expand, None);
        assert!(depth2_rows(&app, pane_id).is_empty(), "nothing may expand");
        assert_eq!(state.scroll, scrolled);

        // After an explicit expansion near the top, the expanded children are
        // drawn under that directory, never after the bottom row.
        let top = root.join("d00");
        app.state
            .files_by_pane
            .get_mut(&pane_id)
            .expect("files")
            .scroll = 0;
        crate::ui::compute_view(&mut app.state, FILES_VIEW);
        app.handle_raw_input_event(files_left_down(rect_center(files_row(&app, &top))))
            .await;
        settle_files(&mut app, pane_id, Instant::now() + Duration::from_secs(3)).await;
        let rows = app.state.files_by_pane[&pane_id].visible_rows();
        assert_eq!(
            app.state.files_by_pane[&pane_id].expanded_dir.as_ref(),
            Some(&top)
        );
        let last_root_entry = rows
            .iter()
            .rposition(|row| row.depth == 1)
            .expect("root entries");
        assert_eq!(
            last_root_entry,
            rows.len() - 1,
            "no expanded children may follow the bottom row"
        );
        for _ in 0..60 {
            app.handle_raw_input_event(files_mouse(
                crossterm::event::MouseEventKind::ScrollDown,
                rect_center(viewport),
            ))
            .await;
        }
        crate::ui::compute_view(&mut app.state, FILES_VIEW);
        let bottom_rows = files_geometry(&app).rows;
        assert!(bottom_rows
            .iter()
            .all(|hit| !hit.path.starts_with(&top) || hit.path == top));
    }

    #[tokio::test]
    async fn files_explicit_click_expands_only_that_directory_after_delay_and_drops_stale_timers() {
        let (mut app, pane_id, root, _guard) = files_fixture("expand", 5).await;
        let (dir1, dir2) = (root.join("d01"), root.join("d03"));

        app.handle_raw_input_event(files_left_down(rect_center(files_row(&app, &dir1))))
            .await;
        let clicked_at = Instant::now();
        assert_eq!(app.state.files_focus, Some(pane_id));
        assert_eq!(
            app.state.files_by_pane[&pane_id].focused_dir.as_ref(),
            Some(&dir1)
        );
        // Before the delay nothing expands.
        settle_files(&mut app, pane_id, clicked_at + Duration::from_millis(1500)).await;
        assert!(depth2_rows(&app, pane_id).is_empty());

        let dir1_due = app.state.files_by_pane[&pane_id]
            .pending_expand
            .as_ref()
            .expect("dir1 expansion pending")
            .due_at;
        // Switch to dir2 before dir1's timer fires.
        tokio::time::sleep(Duration::from_millis(20)).await;
        app.handle_raw_input_event(files_left_down(rect_center(files_row(&app, &dir2))))
            .await;
        let state = &app.state.files_by_pane[&pane_id];
        assert_eq!(state.focused_dir.as_ref(), Some(&dir2));
        assert_eq!(
            state.pending_expand.as_ref().map(|request| &request.target),
            Some(&dir2)
        );
        // dir1's original deadline passes: it must not open.
        assert!(app.state.files_by_pane[&pane_id]
            .pending_expand
            .as_ref()
            .is_some_and(|request| request.due_at > dir1_due));
        settle_files(&mut app, pane_id, dir1_due).await;
        assert!(depth2_rows(&app, pane_id).is_empty());
        assert!(!app.state.files_by_pane[&pane_id]
            .directories
            .contains_key(&dir1));
        // dir2's deadline: only dir2 opens, nested right below its row.
        settle_files(&mut app, pane_id, Instant::now() + Duration::from_secs(3)).await;
        let state = &app.state.files_by_pane[&pane_id];
        assert_eq!(state.expanded_dir.as_ref(), Some(&dir2));
        assert!(!state.directories.contains_key(&dir1));
        let rows = state.visible_rows();
        let dir2_index = rows
            .iter()
            .position(|row| row.path == dir2)
            .expect("dir2 row");
        assert_eq!(rows[dir2_index + 1].path, dir2.join("inner"));
        // dir2 is focused: its own files follow its folders directly.
        assert_eq!(rows[dir2_index + 2].path, dir2.join("note.txt"));
        assert_eq!(
            depth2_rows(&app, pane_id),
            vec![dir2.join("inner"), dir2.join("note.txt")]
        );

        // Moving to another branch closes dir2; its old listing cannot reopen it.
        app.handle_raw_input_event(files_left_down(rect_center(files_row(&app, &dir1))))
            .await;
        assert!(depth2_rows(&app, pane_id).is_empty());
        assert!(!app.state.files_by_pane[&pane_id]
            .directories
            .contains_key(&dir2));
    }

    /// Creates a real directory with `dirs` subdirectories named `<prefix>NN`.
    fn files_tree(name: &str, prefix: &str, dirs: usize) -> FilesFixtureDir {
        let root = std::env::temp_dir().join(format!("herdr-files-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        for idx in 0..dirs {
            std::fs::create_dir_all(root.join(format!("{prefix}{idx:02}")).join("inner"))
                .expect("fixture dir");
        }
        FilesFixtureDir(root)
    }

    /// One workspace, two tabs, one pane each. `agents[i]` marks pane i as an
    /// agent pane (None = plain terminal). Panes' stored cwds are `cwds[i]`.
    fn two_pane_app(
        cwds: [&std::path::Path; 2],
        agents: [Option<crate::detect::Agent>; 2],
    ) -> (super::super::App, [crate::layout::PaneId; 2]) {
        let mut app = super::super::App::new(
            &crate::config::Config::default(),
            true,
            None,
            tokio::sync::mpsc::unbounded_channel().1,
            crate::api::EventHub::default(),
        );
        app.state.update_available = None;
        app.state.latest_release_notes_available = false;
        let mut ws = Workspace::test_new("files");
        let first = ws.tabs[0].root_pane;
        let second_tab = ws.test_add_tab(Some("second"));
        let second = ws.tabs[second_tab].root_pane;
        app.state.workspaces = vec![ws];
        app.state.active = Some(0);
        app.state.selected = 0;
        app.state.mode = crate::app::Mode::Terminal;
        app.state.ensure_test_terminals();
        for (idx, pane_id) in [first, second].into_iter().enumerate() {
            let terminal_id = app.state.workspaces[0]
                .terminal_id(pane_id)
                .cloned()
                .expect("terminal");
            let terminal = app.state.terminals.get_mut(&terminal_id).expect("terminal");
            terminal.cwd = cwds[idx].to_path_buf();
            terminal.detected_agent = agents[idx];
        }
        (app, [first, second])
    }

    fn active_pane(app: &super::super::App) -> Option<crate::layout::PaneId> {
        app.state
            .active
            .and_then(|idx| app.state.workspaces.get(idx)?.focused_pane_id())
    }

    /// Clicks the sidebar agent row for `pane_id` through the top-level input
    /// path, locating the row with the sidebar's own hit test.
    async fn click_agent_row(app: &mut super::super::App, pane_id: crate::layout::PaneId) -> bool {
        crate::ui::compute_view(&mut app.state, FILES_VIEW);
        let row = (0..FILES_VIEW.height)
            .find(|row| {
                app.state
                    .agent_detail_target_at(*row)
                    .is_some_and(|(_, _, target)| target == pane_id)
            })
            .unwrap_or_else(|| panic!("agent row for {pane_id:?} should be rendered"));
        app.handle_raw_input_event(files_left_down((2, row))).await
    }

    fn files_state(
        app: &super::super::App,
        pane_id: crate::layout::PaneId,
    ) -> &crate::ui::FilesPaneState {
        app.state
            .files_by_pane
            .get(&pane_id)
            .unwrap_or_else(|| panic!("Files state for {pane_id:?}"))
    }

    #[tokio::test]
    async fn files_root_follows_sidebar_agent_selection_and_restores_each_pane_state() {
        let a = files_tree("agent-a", "a", 20);
        let b = files_tree("agent-b", "b", 20);
        let (mut app, [first, second]) = two_pane_app(
            [&a.0, &b.0],
            [
                Some(crate::detect::Agent::Codex),
                Some(crate::detect::Agent::Claude),
            ],
        );
        settle_files(&mut app, first, Instant::now()).await;
        assert_eq!(files_state(&app, first).root.as_ref(), Some(&a.0));
        assert_eq!(files_geometry(&app).pane_id, first);

        // Change pane 1's Files state: focus a directory and scroll.
        let a03 = a.0.join("a03");
        app.handle_raw_input_event(files_left_down(rect_center(files_row(&app, &a03))))
            .await;
        for _ in 0..4 {
            app.handle_raw_input_event(files_mouse(
                crossterm::event::MouseEventKind::ScrollDown,
                rect_center(files_geometry(&app).viewport),
            ))
            .await;
        }
        let first_snapshot = (
            files_state(&app, first).focused_dir.clone(),
            files_state(&app, first).scroll,
        );
        assert_eq!(first_snapshot, (Some(a03.clone()), 4));

        // Select pane 2 from its sidebar agent row.
        assert!(click_agent_row(&mut app, second).await, "redraw requested");
        assert_eq!(active_pane(&app), Some(second));
        settle_files(&mut app, second, Instant::now()).await;
        assert_eq!(files_state(&app, second).root.as_ref(), Some(&b.0));
        assert_eq!(files_geometry(&app).pane_id, second);
        assert!(files_geometry(&app)
            .rows
            .iter()
            .all(|hit| hit.path.starts_with(&b.0)));

        let b07 = b.0.join("b02");
        app.handle_raw_input_event(files_left_down(rect_center(files_row(&app, &b07))))
            .await;
        app.handle_raw_input_event(files_mouse(
            crossterm::event::MouseEventKind::ScrollDown,
            rect_center(files_geometry(&app).viewport),
        ))
        .await;
        let second_snapshot = (
            files_state(&app, second).focused_dir.clone(),
            files_state(&app, second).scroll,
        );
        assert_eq!(second_snapshot.0, Some(b07));

        // Back to pane 1: its earlier state is restored, pane 2 is untouched.
        assert!(click_agent_row(&mut app, first).await);
        settle_files(&mut app, first, Instant::now()).await;
        assert_eq!(files_geometry(&app).pane_id, first);
        assert_eq!(
            (
                files_state(&app, first).focused_dir.clone(),
                files_state(&app, first).scroll
            ),
            first_snapshot
        );
        assert_eq!(files_state(&app, first).root.as_ref(), Some(&a.0));
        assert_eq!(
            (
                files_state(&app, second).focused_dir.clone(),
                files_state(&app, second).scroll
            ),
            second_snapshot
        );
    }

    #[tokio::test]
    async fn files_state_is_independent_for_agents_sharing_one_cwd() {
        let shared = files_tree("shared", "s", 20);
        let (mut app, [first, second]) = two_pane_app(
            [&shared.0, &shared.0],
            [
                Some(crate::detect::Agent::Codex),
                Some(crate::detect::Agent::Claude),
            ],
        );
        settle_files(&mut app, first, Instant::now()).await;
        let s02 = shared.0.join("s02");
        app.handle_raw_input_event(files_left_down(rect_center(files_row(&app, &s02))))
            .await;
        app.handle_raw_input_event(files_mouse(
            crossterm::event::MouseEventKind::ScrollDown,
            rect_center(files_geometry(&app).viewport),
        ))
        .await;
        app.state
            .files_by_pane
            .get_mut(&first)
            .expect("files")
            .selected_file = Some(shared.0.join("s02/file"));

        click_agent_row(&mut app, second).await;
        settle_files(&mut app, second, Instant::now()).await;
        let other = files_state(&app, second);
        assert_eq!(other.root.as_ref(), Some(&shared.0));
        assert_eq!(other.focused_dir.as_ref(), Some(&shared.0));
        assert_eq!(other.selected_file, None);
        assert_eq!(other.scroll, 0);

        let s05 = shared.0.join("s04");
        app.handle_raw_input_event(files_left_down(rect_center(files_row(&app, &s05))))
            .await;
        let state = files_state(&app, first);
        assert_eq!(state.focused_dir.as_ref(), Some(&s02));
        assert_eq!(state.scroll, 1);
        assert_eq!(
            state.selected_file.as_ref(),
            Some(&shared.0.join("s02/file"))
        );
        assert_eq!(files_state(&app, second).focused_dir.as_ref(), Some(&s05));
    }

    #[tokio::test]
    async fn files_root_follows_a_plain_terminal_pane_selection() {
        let a = files_tree("plain-a", "a", 3);
        let b = files_tree("plain-b", "b", 3);
        let (mut app, [first, second]) =
            two_pane_app([&a.0, &b.0], [Some(crate::detect::Agent::Codex), None]);
        settle_files(&mut app, first, Instant::now()).await;
        assert_eq!(files_state(&app, first).root.as_ref(), Some(&a.0));
        // The same focus path terminal clicks and agent rows use.
        app.focus_pane_internal_via_api(0, second);
        assert_eq!(active_pane(&app), Some(second));
        settle_files(&mut app, second, Instant::now()).await;
        assert_eq!(files_state(&app, second).root.as_ref(), Some(&b.0));
        assert_eq!(files_geometry(&app).pane_id, second);
    }

    #[tokio::test]
    async fn files_root_follows_a_cwd_change_and_drops_old_root_requests() {
        let a = files_tree("cwd-a", "a", 5);
        let b = files_tree("cwd-b", "b", 5);
        let (mut app, [first, _]) = two_pane_app([&a.0, &b.0], [None, None]);
        settle_files(&mut app, first, Instant::now()).await;
        let a01 = a.0.join("a01");
        app.handle_raw_input_event(files_left_down(rect_center(files_row(&app, &a01))))
            .await;
        app.state
            .files_by_pane
            .get_mut(&first)
            .expect("files")
            .selected_file = Some(a.0.join("a01/x"));
        // Start the old root's listing but do not deliver it yet.
        app.handle_files_tasks(Instant::now() + Duration::from_secs(3));
        assert!(files_state(&app, first)
            .pending_expand
            .as_ref()
            .is_some_and(|request| request.in_flight && request.target == a01));

        let terminal_id = app.state.workspaces[0]
            .terminal_id(first)
            .cloned()
            .expect("terminal");
        app.state
            .terminals
            .get_mut(&terminal_id)
            .expect("terminal")
            .cwd = b.0.clone();
        settle_files(&mut app, first, Instant::now()).await;
        // Let the stale listing for a01 arrive too.
        for _ in 0..50 {
            tokio::time::sleep(Duration::from_millis(5)).await;
            app.drain_internal_events();
        }
        crate::ui::compute_view(&mut app.state, FILES_VIEW);
        let state = files_state(&app, first);
        assert_eq!(state.root.as_ref(), Some(&b.0));
        assert_eq!(state.focused_dir.as_ref(), Some(&b.0));
        assert_eq!(state.expanded_dir.as_ref(), Some(&b.0));
        assert_eq!(state.selected_file, None);
        assert_eq!(state.scroll, 0);
        assert!(state.directories.keys().all(|dir| dir.starts_with(&b.0)));
        let rows = state.visible_rows();
        assert!(rows.iter().all(|row| row.path.starts_with(&b.0)));
        assert!(rows.iter().any(|row| row.path == b.0.join("b00")));
    }

    #[tokio::test]
    async fn files_without_a_usable_cwd_stay_empty_and_recover_on_the_next_update() {
        let a = files_tree("nocwd-a", "a", 3);
        let b = files_tree("nocwd-b", "b", 3);
        let missing = b.0.join("does-not-exist");
        let (mut app, [first, second]) = two_pane_app([&a.0, &missing], [None, None]);
        settle_files(&mut app, first, Instant::now()).await;
        let first_before = files_state(&app, first).trace_summary();

        app.focus_pane_internal_via_api(0, second);
        settle_files(&mut app, second, Instant::now()).await;
        assert!(!app.state.files_by_pane.contains_key(&second));
        assert!(
            app.state.view.files_geometry.is_none(),
            "another pane's Files must not be shown"
        );
        assert_eq!(files_state(&app, first).trace_summary(), first_before);

        for empty in [
            std::path::PathBuf::new(),
            std::path::PathBuf::from("relative/dir"),
        ] {
            let terminal_id = app.state.workspaces[0]
                .terminal_id(second)
                .cloned()
                .expect("terminal");
            app.state
                .terminals
                .get_mut(&terminal_id)
                .expect("terminal")
                .cwd = empty;
            settle_files(&mut app, second, Instant::now()).await;
            assert!(!app.state.files_by_pane.contains_key(&second));
        }

        let terminal_id = app.state.workspaces[0]
            .terminal_id(second)
            .cloned()
            .expect("terminal");
        app.state
            .terminals
            .get_mut(&terminal_id)
            .expect("terminal")
            .cwd = b.0.clone();
        settle_files(&mut app, second, Instant::now()).await;
        assert_eq!(files_state(&app, second).root.as_ref(), Some(&b.0));
        assert_eq!(files_geometry(&app).pane_id, second);
    }

    /// The reported bug: the stored terminal cwd is the launch directory
    /// (`$HOME`) while the pane's live cwd (what the agent header shows) is a
    /// subdirectory. Files must use the live cwd.
    #[tokio::test]
    async fn files_root_uses_the_live_pane_cwd_not_the_stale_launch_cwd() {
        let home = files_tree("live-home", "h", 3);
        let vscode = files_tree("live-vscode", "v", 3);
        let (mut app, [first, second]) = two_pane_app(
            [&home.0, &home.0],
            [None, Some(crate::detect::Agent::Codex)],
        );
        let terminal_id = app.state.workspaces[0]
            .terminal_id(second)
            .cloned()
            .expect("terminal");
        let (events, _events_rx) = tokio::sync::mpsc::channel(4);
        let runtime = crate::terminal::TerminalRuntime::spawn(
            second,
            24,
            80,
            vscode.0.clone(),
            0,
            crate::terminal_theme::TerminalTheme::default(),
            None,
            crate::pane::PaneShellConfig::new("/bin/sh", crate::config::ShellModeConfig::NonLogin),
            &crate::pane::PaneLaunchEnv::default(),
            events,
            std::sync::Arc::new(tokio::sync::Notify::new()),
            std::sync::Arc::new(crate::render_signal::RenderSignal::new()),
        )
        .expect("spawn shell");
        let deadline = Instant::now() + Duration::from_secs(2);
        while runtime.cwd().is_none() && Instant::now() < deadline {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        app.terminal_runtimes.insert(terminal_id, runtime);

        settle_files(&mut app, first, Instant::now()).await;
        assert_eq!(files_state(&app, first).root.as_ref(), Some(&home.0));
        click_agent_row(&mut app, second).await;
        settle_files(&mut app, second, Instant::now()).await;
        let root = files_state(&app, second).root.clone();
        for (_, runtime) in app.terminal_runtimes.drain() {
            runtime.shutdown();
        }
        let root = root.expect("Files root");
        assert_ne!(root, home.0, "Files must not stay on the stale launch cwd");
        // The live cwd may be reported in canonical form (e.g. /private/var).
        assert_eq!(
            root.canonicalize().expect("root exists"),
            vscode.0.canonicalize().expect("fixture exists")
        );
        // Going back to the plain terminal restores its own root.
        // (pane 1 has no runtime, so its stored cwd is its live cwd here.)
        assert_eq!(files_state(&app, first).root.as_ref(), Some(&home.0));
    }

    #[tokio::test]
    async fn files_buttons_light_on_press_and_clear_after_release_without_blocking() {
        let (mut app, pane_id, root, _guard) = files_fixture("buttons", 3).await;
        let geometry = files_geometry(&app);
        let left = crossterm::event::MouseButton::Left;
        // Root is focused, so `[↑]` is disabled: pressing it never lights.
        app.handle_raw_input_event(files_left_down(rect_center(geometry.buttons[0])))
            .await;
        assert_eq!(app.state.files_button_press, None);
        // Mouse Down lights the button at once (and acts).
        app.handle_raw_input_event(files_left_down(rect_center(geometry.buttons[1])))
            .await;
        let press = app.state.files_button_press.expect("pressed on Down");
        assert_eq!((press.pane_id, press.index, press.held), (pane_id, 1, true));
        assert_eq!(
            files_state(&app, pane_id).focused_dir.as_ref(),
            Some(&root.join("d00"))
        );
        // Held: never cleared by the tick, however late.
        app.handle_files_tasks(Instant::now() + Duration::from_secs(1));
        assert!(app.state.files_button_press.is_some());
        // Up anywhere (here outside the sidebar) releases it; it stays lit
        // until its minimum visible time, then the tick clears it and asks
        // for a redraw. Nothing sleeps: the deadline wakes the loop.
        app.handle_raw_input_event(files_mouse(
            crossterm::event::MouseEventKind::Up(left),
            (90, 3),
        ))
        .await;
        let press = app.state.files_button_press.expect("still lit");
        assert!(!press.held);
        assert_eq!(app.next_files_expand_deadline(), Some(press.visible_until));
        let early = press.visible_until - Duration::from_millis(1);
        app.handle_files_tasks(early);
        assert!(app.state.files_button_press.is_some());
        assert!(app.handle_files_tasks(press.visible_until));
        assert_eq!(app.state.files_button_press, None);

        // Keyboard equivalents light the same button while Files has focus.
        assert_eq!(app.state.files_focus, Some(pane_id));
        for (code, index) in [
            (crossterm::event::KeyCode::Char('k'), 0),
            (crossterm::event::KeyCode::Down, 1),
            (crossterm::event::KeyCode::Char('p'), 3),
        ] {
            assert!(app.handle_files_overlay_key(&crossterm::event::KeyEvent::from(code)));
            let press = app.state.files_button_press.expect("key press lights");
            assert_eq!((press.index, press.held), (index, false));
        }
        // With PTY focus the keys are not taken and nothing lights.
        app.state.files_button_press = None;
        app.state.files_focus = None;
        assert!(
            !app.handle_files_overlay_key(&crossterm::event::KeyEvent::from(
                crossterm::event::KeyCode::Down
            ))
        );
        assert_eq!(app.state.files_button_press, None);
    }

    #[tokio::test]
    async fn files_gap_row_above_the_buttons_is_inert_and_fixed() {
        let (mut app, pane_id, root, _guard) = files_fixture("gap", 30).await;
        let geometry = files_geometry(&app);
        // tree viewport / one blank row / buttons, from the same geometry.
        assert_eq!(geometry.gap.height, 1);
        assert_eq!(
            geometry.gap.y,
            geometry.viewport.y + geometry.viewport.height
        );
        assert_eq!(geometry.button_row.y, geometry.gap.y + 1);
        assert!(!geometry.rows.iter().any(|hit| hit.row.y == geometry.gap.y));
        let scrollbar = geometry.scrollbar.expect("30 dirs overflow the tree");
        assert_eq!(
            scrollbar.y + scrollbar.height,
            geometry.gap.y,
            "bar stops above the gap"
        );

        // Nothing in the tree's scroll content stands for the gap.
        let rows = files_state(&app, pane_id).visible_rows();
        assert_eq!(rows.len(), 1 + 30 + 1, "root + dirs + file, no blank row");

        app.state
            .files_by_pane
            .get_mut(&pane_id)
            .expect("files state")
            .scroll = 2;
        let before = files_state(&app, pane_id).trace_summary();
        let gap_points = [
            (geometry.gap.x, geometry.gap.y),
            rect_center(geometry.gap),
            (geometry.gap.x + geometry.gap.width - 1, geometry.gap.y),
        ];
        for point in gap_points {
            app.handle_raw_input_event(files_left_down(point)).await;
            app.handle_raw_input_event(files_mouse(
                crossterm::event::MouseEventKind::Up(crossterm::event::MouseButton::Left),
                point,
            ))
            .await;
            for wheel in [
                crossterm::event::MouseEventKind::ScrollDown,
                crossterm::event::MouseEventKind::ScrollUp,
            ] {
                app.handle_raw_input_event(files_mouse(wheel, point)).await;
            }
        }
        app.handle_files_tasks(Instant::now());
        let state = files_state(&app, pane_id);
        assert_eq!(
            state.trace_summary(),
            before,
            "no focus/selection/scroll change"
        );
        assert_eq!(state.focused_dir.as_ref(), Some(&root));
        assert_eq!(
            app.state.files_focus, None,
            "gap click does not take Files focus"
        );
        assert_eq!(app.state.files_button_press, None);

        // Keyboard moves land on directories only; scrolling and a file
        // selection never move the gap or the buttons.
        app.state.files_focus = Some(pane_id);
        for _ in 0..40 {
            assert!(
                app.handle_files_overlay_key(&crossterm::event::KeyEvent::from(
                    crossterm::event::KeyCode::Down
                ))
            );
            let focused = files_state(&app, pane_id).focused_dir.clone().unwrap();
            assert!(focused.is_dir(), "{focused:?}");
        }
        crate::ui::compute_view(&mut app.state, FILES_VIEW);
        let after = files_geometry(&app);
        assert_eq!(
            (after.gap, after.button_row),
            (geometry.gap, geometry.button_row)
        );
    }

    /// Selects `path` (a file) through the real click path and delivers its
    /// metadata, so the details frame is drawn.
    async fn select_file_with_details(
        app: &mut super::super::App,
        pane_id: crate::layout::PaneId,
        path: &std::path::Path,
    ) {
        let hit = files_geometry(app)
            .rows
            .into_iter()
            .find(|hit| hit.path == path)
            .expect("file row rendered");
        app.handle_raw_input_event(files_left_down(rect_center(hit.name)))
            .await;
        for _ in 0..400 {
            if matches!(
                files_state(app, pane_id).info,
                crate::ui::FileInfoState::Ready(_)
            ) {
                break;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
            app.drain_internal_events();
        }
        assert!(matches!(
            files_state(app, pane_id).info,
            crate::ui::FileInfoState::Ready(_)
        ));
        crate::ui::compute_view(&mut app.state, FILES_VIEW);
    }

    fn assert_files_stack(geometry: &crate::ui::FilesHitGeometry) {
        let (tree, buttons, details, resources) = (
            geometry.viewport,
            geometry.button_row,
            geometry.details,
            geometry.resources,
        );
        assert!(tree.height >= 1);
        assert_eq!(buttons.height, 1);
        assert!(tree.y + tree.height <= buttons.y, "tree above buttons");
        assert_eq!(details.y, buttons.y + 1, "details right under buttons");
        assert!(details.y + details.height <= geometry.area.y + geometry.area.height);
        if resources.height > 0 {
            assert!(
                details.y + details.height <= resources.y,
                "details above resources"
            );
        }
        if let Some(frame) = geometry.details_frame {
            assert!(details.contains((frame.x, frame.y).into()));
            assert_eq!(frame.y + frame.height, details.y + details.height);
            assert!(frame.y > tree.y + tree.height - 1, "frame outside the tree");
        }
    }

    #[tokio::test]
    async fn files_details_rows_are_reserved_below_the_buttons_and_never_move_anything() {
        // root + 12 dirs + 1 file = 14 rows, more than the tree shows.
        let (mut app, pane_id, root, _guard) = files_fixture("details", 12).await;
        let before = files_geometry(&app);
        let max_scroll = 14 - before.viewport.height as usize;
        assert!(max_scroll > 0);
        assert_eq!(before.details.height, crate::ui::files::FILES_DETAILS_ROWS);
        assert_eq!(before.details_frame, None, "nothing drawn before selecting");
        assert_files_stack(&before);

        let wheel = |kind| files_mouse(kind, rect_center(before.viewport));
        for _ in 0..max_scroll {
            app.handle_raw_input_event(wheel(crossterm::event::MouseEventKind::ScrollDown))
                .await;
        }
        crate::ui::compute_view(&mut app.state, FILES_VIEW);
        assert_eq!(files_state(&app, pane_id).scroll, max_scroll);
        select_file_with_details(&mut app, pane_id, &root.join("zz-file.txt")).await;
        let after = files_geometry(&app);
        assert_files_stack(&after);
        assert_eq!(after.button_row, before.button_row);
        assert_eq!(after.resources, before.resources);
        assert_eq!(after.details, before.details);
        assert_eq!(after.viewport, before.viewport);
        let frame = after.details_frame.expect("details frame drawn");
        assert_eq!(frame.height, 6);
        // The frame is not part of the tree rows or the tree offset.
        assert_eq!(files_state(&app, pane_id).visible_rows().len(), 14);
        assert_eq!(files_state(&app, pane_id).scroll, max_scroll);

        // Scrolling the tree moves neither the buttons nor the details frame.
        for _ in 0..max_scroll {
            app.handle_raw_input_event(wheel(crossterm::event::MouseEventKind::ScrollUp))
                .await;
        }
        crate::ui::compute_view(&mut app.state, FILES_VIEW);
        let scrolled = files_geometry(&app);
        assert_eq!(files_state(&app, pane_id).scroll, 0);
        assert_eq!(scrolled.button_row, before.button_row);
        assert_eq!(scrolled.details_frame, Some(frame));

        // The wheel over buttons, details, the header and the divider never
        // scrolls the tree.
        let header = (after.area.x + 1, after.area.y);
        for position in [
            rect_center(after.button_row),
            rect_center(frame),
            (frame.x + 1, frame.y + frame.height - 1),
            header,
            rect_center(after.divider),
        ] {
            app.handle_raw_input_event(files_mouse(
                crossterm::event::MouseEventKind::ScrollDown,
                position,
            ))
            .await;
            assert_eq!(files_state(&app, pane_id).scroll, 0, "{position:?}");
        }
    }

    #[tokio::test]
    async fn stale_file_metadata_results_are_discarded() {
        let (mut app, pane_id, root, _guard) = files_fixture("stale-metadata", 2).await;
        let file = root.join("zz-file.txt");
        select_file_with_details(&mut app, pane_id, &file).await;
        let shown = files_state(&app, pane_id).info.clone();
        let generation = files_state(&app, pane_id).metadata_generation;
        // A result for an older request, or for a file no longer selected,
        // must not replace what is shown.
        for (generation, path) in [
            (generation.wrapping_sub(1), file.clone()),
            (generation, root.join("d00")),
        ] {
            app.handle_internal_event_with_render_impact(
                crate::events::AppEvent::FileMetadataReady {
                    pane_id,
                    generation,
                    path,
                    result: Err("stale".into()),
                },
            );
            assert!(matches!(
                (&files_state(&app, pane_id).info, &shown),
                (crate::ui::FileInfoState::Ready(a), crate::ui::FileInfoState::Ready(b)) if a == b
            ));
        }
    }

    fn agents_rect(app: &super::super::App) -> ratatui::layout::Rect {
        crate::ui::sidebar_files_layout(
            app.state.view.sidebar_rect,
            app.state.sidebar_section_split,
            crate::ui::sidebar_footer_rows(&app.state),
            app.state.files_split,
        )
        .0
    }

    #[tokio::test]
    async fn dragging_the_agents_files_divider_resizes_both_sections_only() {
        let (mut app, pane_id, root, _guard) = files_fixture("divider", 30).await;
        // Put some Files state in place that the drag must not touch.
        let d02 = root.join("d02");
        app.handle_raw_input_event(files_left_down(rect_center(files_row(&app, &d02))))
            .await;
        app.handle_raw_input_event(files_mouse(
            crossterm::event::MouseEventKind::ScrollDown,
            rect_center(files_geometry(&app).viewport),
        ))
        .await;
        crate::ui::compute_view(&mut app.state, FILES_VIEW);
        let files_before = files_state(&app, pane_id).trace_summary();
        let focus_before = app.state.files_focus;
        let agent_scroll_before = app.state.agent_panel_scroll;
        let active_before = active_pane(&app);
        let before = files_geometry(&app);
        let agents_before = agents_rect(&app);
        let split_before = app.state.files_split;

        let divider = app.state.view.files_divider_rect;
        assert_eq!(divider, before.divider);
        assert!(
            app.handle_raw_input_event(files_left_down(rect_center(divider)))
                .await
        );
        assert!(matches!(
            app.state.drag,
            Some(state::DragState {
                target: state::DragTarget::FilesSplitDivider { .. }
            })
        ));

        // Drag up by 3 rows: Agents shrink, the Files tree grows.
        let target = divider.y - 3;
        app.handle_raw_input_event(files_mouse(
            crossterm::event::MouseEventKind::Drag(crossterm::event::MouseButton::Left),
            (2, target),
        ))
        .await;
        crate::ui::compute_view(&mut app.state, FILES_VIEW);
        let during = files_geometry(&app);
        assert!(app.state.files_split < split_before);
        assert_eq!(app.state.view.files_divider_rect.y, target);
        assert_eq!(agents_rect(&app).height, agents_before.height - 3);
        assert_eq!(during.viewport.height, before.viewport.height + 3);
        assert_eq!(during.details.height, before.details.height);
        assert_eq!(during.resources, before.resources);
        assert_files_stack(&during);

        // Dragging across agent rows and Files rows selects nothing; it only
        // moves the divider. End back on the target row.
        for row in [
            agents_before.y + 3,
            during.viewport.y + 1,
            during.button_row.y,
            target,
        ] {
            app.handle_raw_input_event(files_mouse(
                crossterm::event::MouseEventKind::Drag(crossterm::event::MouseButton::Left),
                (3, row),
            ))
            .await;
        }
        crate::ui::compute_view(&mut app.state, FILES_VIEW);
        assert_eq!(app.state.view.files_divider_rect.y, target);

        // Release outside the sidebar still ends the drag.
        app.handle_raw_input_event(files_mouse(
            crossterm::event::MouseEventKind::Up(crossterm::event::MouseButton::Left),
            (90, 5),
        ))
        .await;
        assert!(app.state.drag.is_none());
        assert_eq!(app.state.files_focus, focus_before);
        assert_eq!(app.state.agent_panel_scroll, agent_scroll_before);
        assert_eq!(active_pane(&app), active_before);
        assert_eq!(files_state(&app, pane_id).trace_summary(), files_before);
    }

    #[tokio::test]
    async fn divider_drag_clamps_to_minimum_heights_and_esc_cancels() {
        let (mut app, _pane_id, _root, _guard) = files_fixture("divider-clamp", 5).await;
        let start = app.state.files_split;
        let divider = app.state.view.files_divider_rect;
        app.handle_raw_input_event(files_left_down(rect_center(divider)))
            .await;
        // Far up: Agents keep header + one two-row agent card.
        app.handle_raw_input_event(files_mouse(
            crossterm::event::MouseEventKind::Drag(crossterm::event::MouseButton::Left),
            (2, 0),
        ))
        .await;
        assert_eq!(app.state.files_split, 0.0);
        crate::ui::compute_view(&mut app.state, FILES_VIEW);
        assert_eq!(agents_rect(&app).height, 5);
        // Far down: Files keep header, one tree row, buttons and six details rows.
        app.handle_raw_input_event(files_mouse(
            crossterm::event::MouseEventKind::Drag(crossterm::event::MouseButton::Left),
            (2, FILES_VIEW.height - 1),
        ))
        .await;
        assert_eq!(app.state.files_split, 1.0);
        crate::ui::compute_view(&mut app.state, FILES_VIEW);
        let geometry = files_geometry(&app);
        assert_eq!(geometry.viewport.height, 1);
        assert_eq!(geometry.details.height, 6);
        assert_files_stack(&geometry);

        // Esc restores the split the drag started from and ends it.
        app.handle_key(crate::input::TerminalKey::new(
            crossterm::event::KeyCode::Esc,
            crossterm::event::KeyModifiers::empty(),
        ))
        .await;
        assert!(app.state.drag.is_none());
        assert_eq!(app.state.files_split, start);
    }

    #[tokio::test]
    async fn the_split_ratio_survives_a_resize_and_no_geometry_is_ignored() {
        let (mut app, _pane_id, _root, _guard) = files_fixture("divider-resize", 5).await;
        app.state.files_split = 0.25;
        crate::ui::compute_view(&mut app.state, FILES_VIEW);
        let small = agents_rect(&app);
        let tall = ratatui::layout::Rect::new(0, 0, 106, 90);
        crate::ui::compute_view(&mut app.state, tall);
        assert_eq!(app.state.files_split, 0.25);
        let big = agents_rect(&app);
        assert!(
            big.height > small.height,
            "rows recomputed from the new height"
        );
        let divider = app.state.view.files_divider_rect;
        assert_eq!(divider.y, big.y + big.height);
        // Hit-testing uses the post-resize divider.
        app.handle_raw_input_event(files_left_down(rect_center(divider)))
            .await;
        assert!(app.state.drag.is_some());
        app.handle_raw_input_event(files_mouse(
            crossterm::event::MouseEventKind::Up(crossterm::event::MouseButton::Left),
            rect_center(divider),
        ))
        .await;
        assert!(app.state.drag.is_none());

        // Without a laid-out divider nothing starts a drag.
        app.state.view.files_divider_rect = ratatui::layout::Rect::default();
        app.handle_raw_input_event(files_left_down((0, 0))).await;
        assert!(!matches!(
            app.state.drag,
            Some(state::DragState {
                target: state::DragTarget::FilesSplitDivider { .. }
            })
        ));
    }

    #[test]
    fn lower_sidebar_layout_never_overlaps_or_leaves_its_area_at_any_size() {
        for height in 0..=70u16 {
            for width in [0u16, 1, 2, 10, 26] {
                for footer in [0u16, 3, 5] {
                    for split in [0.0f32, 0.3, 0.5, 1.0, f32::NAN, -4.0, 9.0] {
                        let area = ratatui::layout::Rect::new(0, 0, width, height);
                        let (agents, files, divider) =
                            crate::ui::sidebar_files_layout(area, 0.5, footer, split);
                        let inside = |r: ratatui::layout::Rect| {
                            r.height == 0 || (r.y + r.height <= height && r.x + r.width <= width)
                        };
                        assert!(inside(agents) && inside(files) && inside(divider));
                        if files.height > 0 {
                            assert_eq!(divider.y, agents.y + agents.height);
                            assert_eq!(files.y, divider.y + 1);
                            let layout = crate::ui::files::files_section_layout(files);
                            assert!(layout.tree.height >= 1);
                            // The blank row sits between the tree and the
                            // buttons, or is dropped on a short section.
                            assert_eq!(layout.gap.y, layout.tree.y + layout.tree.height);
                            assert!(layout.gap.height <= 1);
                            assert_eq!(layout.button_row.y, layout.gap.y + layout.gap.height);
                            assert_eq!(layout.details.y, layout.button_row.y + 1);
                            assert_eq!(
                                layout.details.y + layout.details.height,
                                files.y + files.height
                            );
                            if height >= 40 {
                                assert_eq!(layout.details.height, 6, "h={height}");
                                assert_eq!(layout.gap.height, 1, "h={height}");
                            }
                        }
                    }
                }
            }
        }
    }

    #[tokio::test]
    async fn tiny_terminals_render_the_whole_ui_without_panicking() {
        let (mut app, _pane_id, _root, _guard) = files_fixture("tiny", 5).await;
        for (width, height) in [(1, 1), (10, 3), (30, 8), (40, 12), (60, 16), (106, 20)] {
            let area = ratatui::layout::Rect::new(0, 0, width, height);
            crate::ui::compute_view(&mut app.state, area);
            let mut terminal =
                ratatui::Terminal::new(ratatui::backend::TestBackend::new(width, height))
                    .expect("terminal");
            terminal
                .draw(|frame| crate::ui::render(&app.state, frame))
                .expect("draw");
            if let Some(geometry) = app.state.view.files_geometry.clone() {
                assert_files_stack(&geometry);
            }
        }
    }

    #[tokio::test]
    async fn files_tokens_buttons_and_wheel_use_rendered_rects_from_top_level_dispatch() {
        let (mut app, pane_id) = test_app_with_pane();
        app.state.ensure_test_terminals();
        let terminal_id = app.state.workspaces[0]
            .terminal_id(pane_id)
            .cloned()
            .expect("test pane should have a terminal");
        let root = app.state.terminals[&terminal_id].cwd.clone();
        let dir_a = root.join("a_dir");
        let dir_b = root.join("b_dir");
        let file = root.join("c.txt");
        let mut files = crate::ui::FilesPaneState::reset(Some(root.clone()));
        let entry = |path: &std::path::Path, is_dir| crate::ui::FileEntry {
            path: path.to_path_buf(),
            name: path
                .file_name()
                .map(|name| name.to_string_lossy().into_owned())
                .unwrap_or_default(),
            is_dir,
        };
        files.accept_listing(
            &root,
            vec![
                entry(&dir_a, true),
                entry(&dir_b, true),
                entry(&file, false),
            ],
        );
        app.state.files_by_pane.insert(pane_id, files);
        app.state.mode = crate::app::Mode::Terminal;
        crate::ui::compute_view(&mut app.state, FILES_VIEW);
        let geometry = app
            .state
            .view
            .files_geometry
            .clone()
            .expect("rendered Files area should produce input geometry");
        let center =
            |rect: ratatui::layout::Rect| (rect.x + rect.width / 2, rect.y + rect.height / 2);
        let left = crossterm::event::MouseButton::Left;
        let click = |(column, row): (u16, u16)| {
            crate::raw_input::RawInputEvent::Mouse(crossterm::event::MouseEvent {
                kind: crossterm::event::MouseEventKind::Down(left),
                column,
                row,
                modifiers: crossterm::event::KeyModifiers::empty(),
            })
        };
        let file_hit = geometry
            .rows
            .iter()
            .find(|hit| hit.path == file)
            .cloned()
            .expect("Files should contain the file row");

        // Motion over Files never takes keyboard focus.
        app.handle_raw_input_event(crate::raw_input::RawInputEvent::Mouse(
            crossterm::event::MouseEvent {
                kind: crossterm::event::MouseEventKind::Moved,
                column: file_hit.name.x,
                row: file_hit.name.y,
                modifiers: crossterm::event::KeyModifiers::empty(),
            },
        ))
        .await;
        assert_eq!(app.state.files_focus, None);

        // Whitespace on a file row does not select the file.
        let row_end = file_hit.row.x + file_hit.row.width - 1;
        assert!(row_end >= file_hit.name.x + file_hit.name.width);
        app.handle_raw_input_event(click((row_end, file_hit.row.y)))
            .await;
        app.handle_files_tasks(Instant::now());
        assert_eq!(app.state.files_by_pane[&pane_id].selected_file, None);

        // The file name token selects only the file, keeping the focused dir.
        app.handle_raw_input_event(click(center(file_hit.name)))
            .await;
        app.handle_files_tasks(Instant::now());
        assert_eq!(app.state.files_focus, Some(pane_id));
        assert_eq!(
            app.state.files_by_pane[&pane_id].selected_file.as_ref(),
            Some(&file)
        );
        assert_eq!(
            app.state.files_by_pane[&pane_id].focused_dir.as_ref(),
            Some(&root)
        );

        // [↓] / [↑] buttons, clicked at their rendered centers.
        app.handle_raw_input_event(click(center(geometry.buttons[1])))
            .await;
        app.handle_files_tasks(Instant::now());
        assert_eq!(
            app.state.files_by_pane[&pane_id].focused_dir.as_ref(),
            Some(&dir_a)
        );
        app.handle_raw_input_event(click(center(geometry.buttons[1])))
            .await;
        assert_eq!(
            app.state.files_by_pane[&pane_id].focused_dir.as_ref(),
            Some(&dir_b)
        );
        app.handle_raw_input_event(click(center(geometry.buttons[0])))
            .await;
        assert_eq!(
            app.state.files_by_pane[&pane_id].focused_dir.as_ref(),
            Some(&dir_a)
        );
        // [path] button copies the focused dir.
        app.handle_raw_input_event(click(center(geometry.buttons[3])))
            .await;
        assert_eq!(
            app.state.clipboard_feedback_override.as_deref(),
            Some("path copied")
        );
        assert_eq!(app.state.files_focus, Some(pane_id));

        // Wheel over sidebar rows outside the Files viewport (the button row
        // and the sidebar section above Files) does not move the Files offset.
        app.state
            .files_by_pane
            .get_mut(&pane_id)
            .expect("files state")
            .scroll = 1;
        let above_files = geometry.area.y.saturating_sub(1);
        for (column, row) in [center(geometry.buttons[2]), (1, above_files)] {
            assert!(app.state.view.sidebar_rect.contains((column, row).into()));
            assert!(!geometry.viewport.contains((column, row).into()));
            app.handle_raw_input_event(crate::raw_input::RawInputEvent::Mouse(
                crossterm::event::MouseEvent {
                    kind: crossterm::event::MouseEventKind::ScrollUp,
                    column,
                    row,
                    modifiers: crossterm::event::KeyModifiers::empty(),
                },
            ))
            .await;
            assert_eq!(app.state.files_by_pane[&pane_id].scroll, 1);
        }
    }

    #[tokio::test]
    async fn raw_input_batch_does_not_start_pending_agent_resume_before_render() {
        let (mut app, pane_id) = test_app_with_pane();
        app.state.ensure_test_terminals();
        let terminal_id = app.state.workspaces[0]
            .terminal_id(pane_id)
            .cloned()
            .expect("test pane should have a terminal");
        app.state
            .terminals
            .get_mut(&terminal_id)
            .expect("test terminal should exist")
            .pending_agent_resume_plan = Some(crate::agent_resume::AgentResumePlan {
            agent: "codex".into(),
            argv: vec!["/bin/sh".into(), "-c".into(), "sleep 5".into()],
            dedupe_key: "herdr:codex\0codex\0Id\0codex-session".into(),
        });

        assert!(
            app.handle_raw_input_batch(crate::raw_input::RawInputEvent::HostDefaultColor {
                kind: crate::terminal_theme::DefaultColorKind::Foreground,
                color: crate::terminal_theme::RgbColor {
                    r: 220,
                    g: 220,
                    b: 220,
                },
            })
            .await
        );
        assert!(
            app.terminal_runtimes.get(&terminal_id).is_none(),
            "raw input can mutate active geometry; pending resumes must wait for render to refresh pane_infos"
        );
        assert!(app
            .state
            .terminals
            .get(&terminal_id)
            .expect("test terminal should still exist")
            .pending_agent_resume_plan
            .is_some());
    }

    #[tokio::test]
    async fn scheduled_tasks_do_not_start_pending_agent_resume_when_geometry_dirty() {
        let (mut app, pane_id) = test_app_with_pane();
        app.state.ensure_test_terminals();
        app.state.host_terminal_theme = crate::terminal_theme::TerminalTheme {
            foreground: Some(crate::terminal_theme::RgbColor {
                r: 220,
                g: 220,
                b: 220,
            }),
            background: Some(crate::terminal_theme::RgbColor {
                r: 20,
                g: 20,
                b: 20,
            }),
            ..Default::default()
        };
        let terminal_id = app.state.workspaces[0]
            .terminal_id(pane_id)
            .cloned()
            .expect("test pane should have a terminal");
        app.state
            .terminals
            .get_mut(&terminal_id)
            .expect("test terminal should exist")
            .pending_agent_resume_plan = Some(crate::agent_resume::AgentResumePlan {
            agent: "codex".into(),
            argv: vec!["/bin/sh".into(), "-c".into(), "sleep 5".into()],
            dedupe_key: "herdr:codex\0codex\0Id\0codex-session".into(),
        });
        app.pending_agent_resume_deadline = Some(Instant::now() - Duration::from_millis(1));

        assert!(!app.handle_scheduled_tasks(Instant::now(), true));
        assert!(app.terminal_runtimes.get(&terminal_id).is_none());
        assert!(app
            .state
            .terminals
            .get(&terminal_id)
            .expect("test terminal should still exist")
            .pending_agent_resume_plan
            .is_some());
        assert!(app.pending_agent_resume_deadline.is_none());
    }
}

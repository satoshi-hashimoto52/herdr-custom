use super::App;

impl App {
    pub(super) fn update_config_file<F>(&mut self, error_context: &str, update: F) -> bool
    where
        F: FnOnce(&str) -> String,
    {
        #[cfg(test)]
        if std::env::var_os(crate::config::CONFIG_PATH_ENV_VAR).is_none() {
            return false;
        }

        let path = crate::config::config_path();
        if let Some(parent) = path.parent() {
            if let Err(err) = std::fs::create_dir_all(parent) {
                crate::logging::config_write_failed(&path, error_context, &err.to_string());
                self.state.config_diagnostic =
                    Some(format!("failed to save {error_context}: {err}"));
                self.config_diagnostic_deadline =
                    Some(std::time::Instant::now() + std::time::Duration::from_secs(5));
                return false;
            }
        }

        let content = std::fs::read_to_string(&path).unwrap_or_default();
        let new_content = update(&content);
        if let Err(err) = std::fs::write(&path, new_content) {
            crate::logging::config_write_failed(&path, error_context, &err.to_string());
            self.state.config_diagnostic = Some(format!("failed to save {error_context}: {err}"));
            self.config_diagnostic_deadline =
                Some(std::time::Instant::now() + std::time::Duration::from_secs(5));
            return false;
        }

        true
    }

    pub(super) fn mark_onboarding_complete(&mut self) {
        self.update_config_file("onboarding setting", |content| {
            crate::config::upsert_top_level_bool(content, "onboarding", false)
        });
    }

    pub(super) fn save_theme(&mut self, name: &str) {
        if self.update_config_file("theme", |content| {
            let content = crate::config::upsert_section_value(
                content,
                "theme",
                "name",
                &format!("\"{name}\""),
            );
            crate::config::upsert_section_bool(&content, "theme", "auto_switch", false)
        }) {
            self.apply_config_from_disk(false);
        }
    }

    pub(super) fn save_status_indicators(&mut self, style: crate::config::StatusIndicatorStyle) {
        if self.update_config_file("status indicators", |content| {
            crate::config::upsert_section_value(
                content,
                "ui",
                "status_indicators",
                &format!("\"{}\"", style.as_str()),
            )
        }) {
            self.apply_config_from_disk(false);
        }
    }

    pub(super) fn save_sound(&mut self, enabled: bool) {
        if self.update_config_file("sound setting", |content| {
            crate::config::upsert_section_bool(content, "ui.sound", "enabled", enabled)
        }) {
            self.apply_config_from_disk(false);
        }
    }

    pub(super) fn save_toast_delivery(&mut self, delivery: crate::config::ToastDelivery) {
        let value = match delivery {
            crate::config::ToastDelivery::Off => "\"off\"",
            crate::config::ToastDelivery::Herdr => "\"herdr\"",
            crate::config::ToastDelivery::Terminal => "\"terminal\"",
            crate::config::ToastDelivery::System => "\"system\"",
        };
        if self.update_config_file("toast setting", |content| {
            let content =
                crate::config::upsert_section_value(content, "ui.toast", "delivery", value);
            crate::config::remove_section_key(&content, "ui.toast", "enabled")
        }) {
            self.apply_config_from_disk(false);
        }
    }

    pub(super) fn save_agent_border_labels(&mut self, enabled: bool) {
        if self.update_config_file("agent border labels", |content| {
            crate::config::upsert_section_bool(
                content,
                "ui",
                "show_agent_labels_on_pane_borders",
                enabled,
            )
        }) {
            self.apply_config_from_disk(false);
        }
    }

    /// Saves the Agents/Files split under `[ui] files_split`. Only the one
    /// key is rewritten; the rest of the file (comments, unknown keys) is kept.
    /// The live config is not reloaded, so nothing else in the UI changes.
    pub(super) fn save_files_split(&mut self, split: f32) {
        let split = crate::ui::sanitize_files_split(Some(split));
        if self
            .state
            .files_split_saved
            .is_some_and(|saved| (saved - split).abs() < 0.0005)
        {
            trace_files_persistence("config-write-skipped-unchanged", split);
            return;
        }
        let saved = self.update_config_file("files split", |content| {
            crate::config::upsert_section_value(
                content,
                "ui",
                "files_split",
                &format!("{split:.3}"),
            )
        });
        if saved {
            self.state.files_split_saved = Some(split);
        }
        trace_files_persistence(
            if saved {
                "config-write-ok"
            } else {
                "config-write-failed"
            },
            split,
        );
    }

    /// Saves the Spaces/Agents split under `[ui] spaces_split`, the same way
    /// as [`Self::save_files_split`].
    pub(super) fn save_spaces_split(&mut self, split: f32) {
        let split = crate::ui::sanitize_spaces_split(Some(split));
        if self
            .state
            .spaces_split_saved
            .is_some_and(|saved| (saved - split).abs() < 0.0005)
        {
            trace_split_persistence("config-write-skipped-unchanged", "spaces_split", split);
            return;
        }
        let saved = self.update_config_file("spaces split", |content| {
            crate::config::upsert_section_value(
                content,
                "ui",
                "spaces_split",
                &format!("{split:.4}"),
            )
        });
        if saved {
            self.state.spaces_split_saved = Some(split);
        }
        trace_split_persistence(
            if saved {
                "config-write-ok"
            } else {
                "config-write-failed"
            },
            "spaces_split",
            split,
        );
    }

    pub(super) fn save_agent_panel_sort(&mut self, sort: crate::app::state::AgentPanelSort) {
        let value = match sort {
            crate::app::state::AgentPanelSort::Spaces => {
                crate::config::AgentPanelSortConfig::Spaces.as_str()
            }
            crate::app::state::AgentPanelSort::Priority => {
                crate::config::AgentPanelSortConfig::Priority.as_str()
            }
        };
        if self.update_config_file("agent panel sort", |content| {
            crate::config::upsert_section_value(
                content,
                "ui",
                "agent_panel_sort",
                &format!("\"{value}\""),
            )
        }) {
            self.apply_config_from_disk(false);
        }
    }
}

fn trace_files_persistence(event: &str, split: f32) {
    trace_split_persistence(event, "files_split", split);
}

fn trace_split_persistence(event: &str, key: &str, split: f32) {
    if super::input::files_trace::enabled() {
        super::input::files_trace::state_change(
            event,
            &format!("{}-persistence", key.replace('_', "-")),
            "-",
            &format!("path={:?}", crate::config::config_path()),
            &format!("[ui] {key}={split:.4}"),
        );
    }
}

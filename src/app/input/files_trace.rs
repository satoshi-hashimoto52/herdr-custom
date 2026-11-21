//! Opt-in Files diagnostics, written only to the file named by
//! `HERDR_FILES_TRACE_PATH`.
//!
//! The variable is read once per process and cached. When it is unset every
//! entry point returns after one atomic load: no environment lookup, no
//! formatting (callers pass closures or guard with [`enabled`]), no file I/O
//! and no locking. Write failures are ignored, never surfaced: diagnostics
//! must not affect Files or PTY input. Nothing here goes to stdout, stderr
//! or `tracing`.

use std::{
    cell::RefCell,
    fs::OpenOptions,
    io::Write,
    path::{Path, PathBuf},
};

use crossterm::event::MouseEvent;

thread_local! {
    /// The record of the mouse event being traced on this thread, if any.
    static TRACE: RefCell<Option<String>> = const { RefCell::new(None) };
}

fn trace_path() -> Option<&'static Path> {
    static PATH: std::sync::OnceLock<Option<PathBuf>> = std::sync::OnceLock::new();
    PATH.get_or_init(|| std::env::var_os("HERDR_FILES_TRACE_PATH").map(PathBuf::from))
        .as_deref()
}

fn append(message: impl AsRef<[u8]>) {
    if let Some(path) = trace_path() {
        append_at(path, message.as_ref());
    }
}

pub(crate) fn startup(no_session: bool) {
    if enabled() {
        append(format!("startup trace-enabled no_session={no_session}\n"));
    }
}

pub(crate) fn terminal_initialized(
    no_session: bool,
    raw_mode: bool,
    alternate_screen: bool,
    mouse_capture: bool,
) {
    if !enabled() {
        return;
    }
    append(format!(
        "terminal-init raw_mode={raw_mode} alternate_screen={alternate_screen} mouse_capture={mouse_capture} no_session={no_session}\n"
    ));
}

pub(crate) fn event_loop_started(
    no_session: bool,
    raw_mode: bool,
    alternate_screen: bool,
    mouse_capture: bool,
) {
    if !enabled() {
        return;
    }
    append(format!(
        "event-loop-start raw_mode={raw_mode} alternate_screen={alternate_screen} mouse_capture={mouse_capture} no_session={no_session}\n"
    ));
}

pub(crate) fn terminal_restored(mouse_capture_disabled: bool) {
    if !enabled() {
        return;
    }
    append(format!(
        "terminal-restore mouse_capture_disabled={mouse_capture_disabled}\n"
    ));
}

/// Process-relative clock for trace timestamps.
fn trace_epoch() -> std::time::Instant {
    static EPOCH: std::sync::OnceLock<std::time::Instant> = std::sync::OnceLock::new();
    *EPOCH.get_or_init(std::time::Instant::now)
}

/// Starts the record of one mouse event. Returns whether tracing is on.
pub(crate) fn begin(mouse: &MouseEvent) -> bool {
    if !enabled() {
        return false;
    }
    TRACE.with(|trace| {
        *trace.borrow_mut() = Some(format!(
            "mouse_event t_ms={} raw={mouse:?}\nkind={:?} button={:?} column={} row={} modifiers={:?}\n",
            trace_epoch().elapsed().as_millis(),
            mouse.kind,
            mouse_button(mouse),
            mouse.column,
            mouse.row,
            mouse.modifiers,
        ));
    });
    true
}

fn mouse_button(mouse: &MouseEvent) -> Option<crossterm::event::MouseButton> {
    match mouse.kind {
        crossterm::event::MouseEventKind::Down(button)
        | crossterm::event::MouseEventKind::Up(button)
        | crossterm::event::MouseEventKind::Drag(button) => Some(button),
        _ => None,
    }
}

/// Adds a fixed line to the current mouse-event record (no-op otherwise).
pub(crate) fn record(message: &str) {
    record_with(|| message.to_string());
}

/// Adds a line built by `message` to the current mouse-event record. The
/// closure runs only while a traced event is being recorded.
pub(crate) fn record_with(message: impl FnOnce() -> String) {
    if !enabled() {
        return;
    }
    TRACE.with(|trace| {
        if let Some(trace) = trace.borrow_mut().as_mut() {
            trace.push_str(&message());
            trace.push('\n');
        }
    });
}

pub(crate) fn finish(redraw: bool) {
    let Some(path) = trace_path() else {
        return;
    };
    let record = TRACE.with(|trace| trace.borrow_mut().take());
    let Some(mut record) = record else {
        return;
    };
    record.push_str(&format!("redraw_requested={redraw}\n---\n"));
    append_at(path, record.as_bytes());
}

fn append_at(path: &Path, message: &[u8]) {
    let Ok(mut file) = OpenOptions::new().create(true).append(true).open(path) else {
        return;
    };
    let _ = file.write_all(message);
    let _ = file.flush();
}

pub(crate) fn basename(path: &Path) -> &str {
    path.file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("<non-utf8>")
}

/// Whether `HERDR_FILES_TRACE_PATH` is set. Callers guard any diagnostic
/// formatting with this.
pub(crate) fn enabled() -> bool {
    trace_path().is_some()
}

/// Records one Files state transition. Inside a traced mouse event it joins
/// that event's record; otherwise (tick, listing completion) it is appended
/// directly. Callers only invoke this when the state actually changed.
pub(crate) fn state_change(
    event: &str,
    reason: &str,
    files_focus: impl std::fmt::Debug,
    before: &str,
    after: &str,
) {
    if !enabled() {
        return;
    }
    let line = format!(
        "files_state event={event} reason={reason} files_focus={files_focus:?}\n  before: {before}\n  after:  {after}"
    );
    let buffered = TRACE.with(|trace| {
        if let Some(trace) = trace.borrow_mut().as_mut() {
            trace.push_str(&line);
            trace.push('\n');
            true
        } else {
            false
        }
    });
    if !buffered {
        append(format!("{line}\n---\n"));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Unit tests run without `HERDR_FILES_TRACE_PATH`: every entry point
    /// must be a no-op that never builds its message.
    #[test]
    fn disabled_trace_never_formats_or_writes() {
        if enabled() {
            return; // a developer ran the suite with tracing on
        }
        let mouse = MouseEvent {
            kind: crossterm::event::MouseEventKind::Moved,
            column: 1,
            row: 2,
            modifiers: crossterm::event::KeyModifiers::empty(),
        };
        assert!(!begin(&mouse));
        record_with(|| panic!("message built while tracing is off"));
        record("fixed");
        state_change("event", "reason", "-", "before", "after");
        startup(true);
        finish(true);
        TRACE.with(|trace| assert!(trace.borrow().is_none()));
    }
}

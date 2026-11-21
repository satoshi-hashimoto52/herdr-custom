//! Sidebar file tree. Files are presentation state keyed by pane identity.

use std::{
    collections::HashMap,
    path::{Path, PathBuf},
    time::{Duration, Instant},
};

use super::scrollbar::{render_scrollbar, should_show_scrollbar};
use ratatui::{
    layout::Rect,
    style::Style,
    text::{Line, Span},
    widgets::Paragraph,
    Frame,
};
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

use crate::app::state::AppState;

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct FileEntry {
    pub path: PathBuf,
    pub name: String,
    pub is_dir: bool,
}

/// What a tree row is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum FileRowKind {
    Directory,
    File,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct FileRow {
    pub path: PathBuf,
    pub name: String,
    pub kind: FileRowKind,
    /// `kind == Directory`, kept as a field for hit-testing and callers.
    pub is_dir: bool,
    /// Directory whose listing produced the row (`None` for the root).
    pub parent: Option<PathBuf>,
    pub depth: u16,
    /// Directory rows only: the last *directory* among its parent's
    /// directories (files that follow do not count) → `└`. Always false for
    /// file and spacer rows.
    pub is_last_directory_sibling: bool,
    /// For each ancestor level from depth 1 up to the parent: whether that
    /// ancestor directory has a later directory sibling, i.e. whether its `│`
    /// guide continues past it.
    pub ancestor_has_later_directory_sibling: Vec<bool>,
}

/// The two-column cells drawn before a row's name, one per depth level,
/// decided from the tree model, never from the row's position on screen:
/// - levels above the row: `│ ` when that ancestor has a later directory
///   sibling, else `  ` (these are the only `│` a file row ever shows);
/// - the row's own level: `├ ` / `└ ` for directories, `  ` for files, so
///   file names start where same-depth directory names start.
pub(crate) fn tree_prefix_segments(row: &FileRow) -> Vec<&'static str> {
    if row.depth == 0 {
        return Vec::new();
    }
    let mut segments = row
        .ancestor_has_later_directory_sibling
        .iter()
        .map(|continues| if *continues { "│ " } else { "  " })
        .collect::<Vec<_>>();
    segments.push(match row.kind {
        FileRowKind::Directory if row.is_last_directory_sibling => "└ ",
        FileRowKind::Directory => "├ ",
        FileRowKind::File => "  ",
    });
    segments
}

#[cfg(test)]
pub(crate) fn tree_prefix(row: &FileRow) -> String {
    tree_prefix_segments(row).concat()
}

/// Display width of [`tree_prefix`] (each glyph and space is one column).
pub(crate) fn tree_prefix_width(row: &FileRow) -> usize {
    usize::from(row.depth) * 2
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct FilesRowHit {
    pub row: Rect,
    pub name: Rect,
    pub path: PathBuf,
    pub is_dir: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct FilesHitGeometry {
    pub pane_id: crate::layout::PaneId,
    pub generation: u64,
    /// Whole Files section (header through details).
    pub area: Rect,
    /// Tree rows, excluding the scrollbar column. The only wheel target.
    pub viewport: Rect,
    pub rows: Vec<FilesRowHit>,
    pub buttons: [Rect; 4],
    /// Blank row between the tree viewport and the buttons. Not part of the
    /// tree's scroll content; clicks and wheel on it do nothing.
    pub gap: Rect,
    /// The `[↑] [↓] [open] [path]` row.
    pub button_row: Rect,
    /// Rows reserved for the file information frame, drawn or not.
    pub details: Rect,
    /// The drawn information frame, when a file's details are shown.
    pub details_frame: Option<Rect>,
    /// Tree scrollbar column, when the tree overflows.
    pub scrollbar: Option<Rect>,
    /// Agents/Files divider row.
    pub divider: Rect,
    /// Resource footer (CPU/RAM/SSD/SWAP) below the Files section.
    pub resources: Rect,
}

/// Rows reserved under the buttons for the file information frame: top
/// border, four information lines, bottom border.
pub(crate) const FILES_DETAILS_ROWS: u16 = 6;

/// Blank rows between the tree viewport and the button row.
pub(crate) const FILES_GAP_ROWS: u16 = 1;

/// Fixed vertical structure of the Files section. Render and hit-testing both
/// use this, so they always agree on positions.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub(crate) struct FilesSectionLayout {
    pub header: Rect,
    /// Tree rows including the scrollbar column.
    pub tree: Rect,
    /// Blank row between the tree and the buttons (zero height when the
    /// section is too short for it).
    pub gap: Rect,
    pub button_row: Rect,
    pub details: Rect,
}

/// header (1) / tree (rest, ≥1) / gap (1) / buttons (1) / details (6).
/// On a short section the tree row and the buttons come first, then the
/// details rows, and the gap is the first thing dropped. Rects never overlap
/// or leave `area`.
pub(crate) fn files_section_layout(area: Rect) -> FilesSectionLayout {
    if area.width == 0 || area.height < 3 {
        return FilesSectionLayout::default();
    }
    let details_height = (area.height - 3).min(FILES_DETAILS_ROWS);
    let gap_height = (area.height - 3 - details_height).min(FILES_GAP_ROWS);
    let tree_height = area.height - 2 - details_height - gap_height;
    let tree = Rect::new(area.x, area.y + 1, area.width, tree_height);
    let gap = Rect::new(area.x, tree.y + tree_height, area.width, gap_height);
    let button_row = Rect::new(area.x, gap.y + gap_height, area.width, 1);
    FilesSectionLayout {
        header: Rect::new(area.x, area.y, area.width, 1),
        tree,
        gap,
        button_row,
        details: Rect::new(area.x, button_row.y + 1, area.width, details_height),
    }
}

/// Visible tree slice shared by render and hit-testing.
struct FilesTreeView {
    rows: Vec<FileRow>,
    scroll: usize,
    viewport_rows: usize,
    show_scrollbar: bool,
    text_width: u16,
}

fn files_tree_view(state: &FilesPaneState, tree: Rect) -> FilesTreeView {
    let rows = state.visible_rows();
    let viewport_rows = tree.height as usize;
    let scroll = state.scroll.min(rows.len().saturating_sub(viewport_rows));
    let show_scrollbar = rows.len() > viewport_rows && tree.width > 1;
    FilesTreeView {
        text_width: tree.width.saturating_sub(u16::from(show_scrollbar)),
        rows,
        scroll,
        viewport_rows,
        show_scrollbar,
    }
}

/// Width of the information frame: one column short of the section so its
/// right border lines up with tree text and never touches the scrollbar column.
fn details_frame_width(details: Rect) -> u16 {
    details.width.saturating_sub(1)
}

fn details_frame_rect(state: &FilesPaneState, details: Rect) -> Option<Rect> {
    (matches!(state.info, FileInfoState::Ready(_)) && details.height >= 2).then(|| {
        Rect::new(
            details.x,
            details.y,
            details_frame_width(details),
            details.height,
        )
    })
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub(crate) struct MetadataCacheKey {
    pub path: PathBuf,
    pub size: u64,
    pub modified: Option<u128>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct FileInfo {
    pub key: MetadataCacheKey,
    pub title: String,
    pub lines: Vec<String>,
}

#[derive(Debug, Clone, Default)]
pub(crate) enum FileInfoState {
    #[default]
    Hidden,
    Loading,
    Ready(FileInfo),
    Error(String),
}

/// Delay between an explicit directory focus change and its expansion.
pub(crate) const FILES_EXPAND_DELAY: Duration = Duration::from_secs(2);

/// A delayed directory expansion. It is only honored when every identity
/// field still matches the pane's Files state when the timer fires and when
/// the listing completes; otherwise it is discarded as stale.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct FilesExpandRequest {
    pub pane_id: crate::layout::PaneId,
    pub root: PathBuf,
    pub target: PathBuf,
    pub focus_generation: u64,
    pub request_generation: u64,
    pub scheduled_at: Instant,
    pub due_at: Instant,
    /// Explicit user focus changes require Files keyboard focus on the pane.
    /// Pane/cwd initialization of the root listing does not.
    pub requires_files_focus: bool,
    /// The listing task has been started and its result is awaited.
    pub in_flight: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum FilesExpandCheck {
    Idle,
    Waiting,
    Ready,
    Stale(&'static str),
}

#[derive(Debug, Clone, Default)]
pub(crate) struct FilesPaneState {
    pub root: Option<PathBuf>,
    /// Directory the Files cursor is on. Changed only by explicit operations.
    pub focused_dir: Option<PathBuf>,
    /// Deepest open directory. The open set is this path and its ancestors up
    /// to `root`, so only one branch is ever expanded.
    pub expanded_dir: Option<PathBuf>,
    pub selected_file: Option<PathBuf>,
    /// The directory whose files are listed. Only ever the focused directory,
    /// and only after a listing of it completed *for the current focus*; a
    /// cached listing of a directory that is merely open never shows files.
    pub files_shown_for: Option<PathBuf>,
    pub scroll: usize,
    pub message: Option<String>,
    /// Listing cache, pruned to the open branch whenever it changes.
    pub directories: HashMap<PathBuf, Vec<FileEntry>>,
    /// Bumped on every explicit `focused_dir` change.
    pub focus_generation: u64,
    /// Bumped on every delayed-expansion request, invalidating older ones.
    pub request_generation: u64,
    /// Bumped whenever a listing is applied.
    pub listing_generation: u64,
    pub pending_expand: Option<FilesExpandRequest>,
    pub info: FileInfoState,
    pub metadata_generation: u64,
    pub metadata_cache: HashMap<MetadataCacheKey, FileInfo>,
}

impl FilesPaneState {
    pub(crate) fn reset(root: Option<PathBuf>) -> Self {
        Self {
            root: root.clone(),
            focused_dir: root.clone(),
            expanded_dir: root,
            ..Self::default()
        }
    }

    /// Requests the initial root listing after a pane or cwd change.
    pub(crate) fn schedule_initial_listing(
        &mut self,
        pane_id: crate::layout::PaneId,
        now: Instant,
    ) {
        let Some(root) = self.root.clone() else {
            return;
        };
        self.schedule_expand(pane_id, root, now, now, false);
    }

    fn schedule_expand(
        &mut self,
        pane_id: crate::layout::PaneId,
        target: PathBuf,
        now: Instant,
        due_at: Instant,
        requires_files_focus: bool,
    ) {
        let Some(root) = self.root.clone() else {
            return;
        };
        self.request_generation = self.request_generation.wrapping_add(1);
        self.pending_expand = Some(FilesExpandRequest {
            pane_id,
            root,
            target,
            focus_generation: self.focus_generation,
            request_generation: self.request_generation,
            scheduled_at: now,
            due_at,
            requires_files_focus,
            in_flight: false,
        });
    }

    /// Moves `focused_dir` because of an explicit user operation (directory
    /// click, ↑/↓/j/k, `[↑]`/`[↓]`). Closes the previous branch and starts the
    /// delayed expansion of `path`. Never touches the scroll offset.
    /// Returns whether a new delayed expansion was scheduled.
    pub(crate) fn focus_dir_explicit(
        &mut self,
        pane_id: crate::layout::PaneId,
        path: PathBuf,
        now: Instant,
    ) -> bool {
        let Some(root) = self.root.clone() else {
            return false;
        };
        if !path.starts_with(&root) {
            return false;
        }
        if self.focused_dir.as_ref() == Some(&path) {
            // Re-selecting the focused directory restarts its expansion only
            // when it is neither open nor already pending.
            if self.expanded_dir.as_ref() != Some(&path) && self.pending_expand.is_none() {
                self.schedule_expand(pane_id, path, now, now + FILES_EXPAND_DELAY, true);
                return true;
            }
            return false;
        }
        let path_is_open = self
            .expanded_dir
            .as_ref()
            .is_some_and(|expanded| expanded.starts_with(&path));
        self.expanded_dir = if path_is_open {
            Some(path.clone())
        } else {
            path.parent()
                .filter(|parent| parent.starts_with(&root))
                .map(Path::to_path_buf)
                .or(Some(root))
        };
        self.prune_directories();
        self.focused_dir = Some(path.clone());
        // The previous directory's files, selection and details go at once;
        // the new directory's files appear only after its delayed listing.
        self.files_shown_for = None;
        self.selected_file = None;
        self.message = None;
        self.info = FileInfoState::Hidden;
        self.metadata_generation = self.metadata_generation.wrapping_add(1);
        self.focus_generation = self.focus_generation.wrapping_add(1);
        self.schedule_expand(pane_id, path, now, now + FILES_EXPAND_DELAY, true);
        true
    }

    /// Validates the pending expansion against the current state.
    pub(crate) fn check_expand(
        &self,
        active_pane: crate::layout::PaneId,
        files_focus: Option<crate::layout::PaneId>,
        now: Instant,
    ) -> FilesExpandCheck {
        let Some(request) = &self.pending_expand else {
            return FilesExpandCheck::Idle;
        };
        if request.in_flight {
            return FilesExpandCheck::Idle;
        }
        if now < request.due_at {
            return FilesExpandCheck::Waiting;
        }
        if let Some(reason) = self.stale_reason(request, active_pane) {
            return FilesExpandCheck::Stale(reason);
        }
        if request.requires_files_focus && files_focus != Some(request.pane_id) {
            return FilesExpandCheck::Stale("files_focus_mismatch");
        }
        FilesExpandCheck::Ready
    }

    fn stale_reason(
        &self,
        request: &FilesExpandRequest,
        pane_id: crate::layout::PaneId,
    ) -> Option<&'static str> {
        if request.pane_id != pane_id {
            return Some("pane_mismatch");
        }
        if self.root.as_ref() != Some(&request.root) {
            return Some("root_mismatch");
        }
        if self.focused_dir.as_ref() != Some(&request.target) {
            return Some("focused_dir_mismatch");
        }
        if request.focus_generation != self.focus_generation {
            return Some("focus_generation_mismatch");
        }
        if request.request_generation != self.request_generation {
            return Some("request_generation_mismatch");
        }
        if !request.target.starts_with(&request.root) {
            return Some("target_outside_root");
        }
        None
    }

    /// Applies a completed listing for `request`. Rejects it unless the
    /// request is still the in-flight one and the state still matches.
    pub(crate) fn apply_listing(
        &mut self,
        request: &FilesExpandRequest,
        result: Result<Vec<FileEntry>, String>,
    ) -> Result<(), &'static str> {
        let Some(pending) = &self.pending_expand else {
            return Err("no_pending_request");
        };
        if pending != request || !pending.in_flight {
            return Err("request_superseded");
        }
        if let Some(reason) = self.stale_reason(request, request.pane_id) {
            self.pending_expand = None;
            return Err(reason);
        }
        self.pending_expand = None;
        match result {
            Ok(entries) => {
                self.directories.insert(request.target.clone(), entries);
                self.expanded_dir = Some(request.target.clone());
                // stale_reason() guaranteed target == focused_dir.
                self.files_shown_for = Some(request.target.clone());
                self.prune_directories();
                self.message = None;
                self.listing_generation = self.listing_generation.wrapping_add(1);
                Ok(())
            }
            Err(message) => {
                self.message = Some(message);
                Ok(())
            }
        }
    }

    /// Whether a metadata result for `path` from request `generation` still
    /// belongs to the current state: same generation, still the selected
    /// file, and a direct child of the focused directory whose files are
    /// shown.
    pub(crate) fn accepts_metadata(&self, generation: u64, path: &Path) -> bool {
        self.metadata_generation == generation
            && self.selected_file.as_deref() == Some(path)
            && self.file_listing_dir().map(PathBuf::as_path) == path.parent()
    }

    /// Keeps only listings of the open branch so closed paths do not linger.
    fn prune_directories(&mut self) {
        let Some(expanded) = self.expanded_dir.clone() else {
            self.directories.clear();
            return;
        };
        self.directories.retain(|dir, _| expanded.starts_with(dir));
    }

    /// Scrolls the minimum amount needed to show `path`.
    pub(crate) fn ensure_visible(&mut self, path: &Path, viewport: usize) -> bool {
        let Some(row) = self
            .visible_rows()
            .iter()
            .position(|item| item.path == path)
        else {
            return false;
        };
        let before = self.scroll;
        if row < self.scroll {
            self.scroll = row;
        } else if viewport > 0 && row >= self.scroll.saturating_add(viewport) {
            self.scroll = row.saturating_sub(viewport.saturating_sub(1));
        }
        self.scroll != before
    }

    /// `scroll = min(scroll, max_scroll)`; never resets to zero otherwise.
    pub(crate) fn clamp_scroll(&mut self, viewport: usize) -> bool {
        let max_scroll = self.visible_rows().len().saturating_sub(viewport);
        if self.scroll > max_scroll {
            self.scroll = max_scroll;
            return true;
        }
        false
    }

    /// Seeds a listing directly; tests use this in place of a disk scan.
    #[cfg(test)]
    pub(crate) fn accept_listing(&mut self, dir: &Path, entries: Vec<FileEntry>) {
        self.directories.insert(dir.to_path_buf(), entries);
        if self.focused_dir.as_deref() == Some(dir) {
            self.expanded_dir = Some(dir.to_path_buf());
            self.files_shown_for = Some(dir.to_path_buf());
        }
        self.listing_generation = self.listing_generation.wrapping_add(1);
    }

    /// The open branch from `root` to `expanded_dir`, root first.
    fn open_chain(&self) -> Vec<PathBuf> {
        let Some(root) = &self.root else {
            return Vec::new();
        };
        let mut chain = Vec::new();
        let mut current = self
            .expanded_dir
            .clone()
            .filter(|dir| dir.starts_with(root))
            .unwrap_or_else(|| root.clone());
        loop {
            chain.push(current.clone());
            if current == *root {
                break;
            }
            match current.parent() {
                Some(parent) if parent.starts_with(root) => current = parent.to_path_buf(),
                _ => break,
            }
        }
        chain.reverse();
        if chain.first() != Some(root) {
            return vec![root.clone()];
        }
        chain
    }

    /// Tree rows. Subdirectories of every open directory are placed directly
    /// below it; file rows appear only for the focused directory (once its
    /// listing for the current focus is in), right after its subdirectories.
    pub(crate) fn visible_rows(&self) -> Vec<FileRow> {
        let Some(root) = &self.root else {
            return Vec::new();
        };
        let mut rows = vec![FileRow {
            path: root.clone(),
            name: root_name(root),
            kind: FileRowKind::Directory,
            is_dir: true,
            parent: None,
            depth: 0,
            is_last_directory_sibling: true,
            ancestor_has_later_directory_sibling: Vec::new(),
        }];
        let chain = self.open_chain();
        self.push_children(&chain, 0, &[], &mut rows);
        rows
    }

    /// The directory whose direct files are drawn, if any.
    fn file_listing_dir(&self) -> Option<&PathBuf> {
        self.files_shown_for
            .as_ref()
            .filter(|dir| self.focused_dir.as_ref() == Some(*dir))
    }

    fn push_children(
        &self,
        chain: &[PathBuf],
        index: usize,
        ancestors: &[bool],
        rows: &mut Vec<FileRow>,
    ) {
        let Some(dir) = chain.get(index) else {
            return;
        };
        let Some(entries) = self.directories.get(dir) else {
            return;
        };
        let depth = index.saturating_add(1).min(u16::MAX as usize) as u16;
        let directory_count = entries.iter().filter(|entry| entry.is_dir).count();
        let mut directory_index = 0;
        for entry in entries.iter().filter(|entry| entry.is_dir) {
            directory_index += 1;
            let is_last = directory_index == directory_count;
            rows.push(FileRow {
                path: entry.path.clone(),
                name: entry.name.clone(),
                kind: FileRowKind::Directory,
                is_dir: true,
                parent: Some(dir.clone()),
                depth,
                is_last_directory_sibling: is_last,
                ancestor_has_later_directory_sibling: ancestors.to_vec(),
            });
            if chain.get(index + 1) == Some(&entry.path) {
                let mut child_ancestors = ancestors.to_vec();
                child_ancestors.push(!is_last);
                self.push_children(chain, index + 1, &child_ancestors, rows);
            }
        }
        if self.file_listing_dir() != Some(dir) {
            return;
        }
        // The focused directory's files follow its subdirectories directly,
        // with no blank row in between.
        let files = entries.iter().filter(|entry| !entry.is_dir);
        for entry in files {
            rows.push(FileRow {
                path: entry.path.clone(),
                name: entry.name.clone(),
                kind: FileRowKind::File,
                is_dir: false,
                parent: Some(dir.clone()),
                depth,
                is_last_directory_sibling: false,
                ancestor_has_later_directory_sibling: ancestors.to_vec(),
            });
        }
    }

    pub(crate) fn focusable_directories(&self) -> Vec<PathBuf> {
        self.visible_rows()
            .into_iter()
            .filter(|row| row.kind == FileRowKind::Directory)
            .map(|row| row.path)
            .collect()
    }

    /// One-line state summary for `HERDR_FILES_TRACE_PATH` diagnostics.
    pub(crate) fn trace_summary(&self) -> String {
        format!(
            "focused_dir={:?} expanded={:?} files_shown_for={:?} scroll={} selected_file={:?} delayed_target={:?} focus_gen={} request_gen={} listing_gen={}",
            self.focused_dir,
            self.expanded_dir,
            self.files_shown_for,
            self.scroll,
            self.selected_file,
            self.pending_expand
                .as_ref()
                .map(|request| (&request.target, request.in_flight)),
            self.focus_generation,
            self.request_generation,
            self.listing_generation,
        )
    }
}

fn root_name(root: &Path) -> String {
    root.file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("workdirectory")
        .to_string()
}

pub(crate) fn list_directory(root: &Path, dir: &Path) -> Result<Vec<FileEntry>, String> {
    let root = root
        .canonicalize()
        .map_err(|err| format!("cwd unavailable: {err}"))?;
    let canonical_dir = dir.canonicalize().map_err(|err| err.to_string())?;
    if !canonical_dir.starts_with(&root) {
        return Err("outside cwd".to_string());
    }

    // Entry paths stay in the caller's (cwd-relative, non-canonical) path
    // space so they compare equal to the focused/expanded paths.
    let mut entries = std::fs::read_dir(&canonical_dir)
        .map_err(|err| err.to_string())?
        .filter_map(Result::ok)
        .filter_map(|entry| {
            let path = dir.join(entry.file_name());
            let metadata = std::fs::symlink_metadata(&path).ok()?;
            let is_dir = metadata.is_dir();
            let name = entry.file_name().to_string_lossy().into_owned();
            Some(FileEntry { path, name, is_dir })
        })
        .collect::<Vec<_>>();
    entries.sort_by(|left, right| {
        right
            .is_dir
            .cmp(&left.is_dir)
            .then_with(|| left.name.to_lowercase().cmp(&right.name.to_lowercase()))
            .then_with(|| left.name.cmp(&right.name))
    });
    Ok(entries)
}

pub(crate) fn metadata_key(path: &Path) -> Result<MetadataCacheKey, String> {
    let metadata = std::fs::metadata(path).map_err(|err| err.to_string())?;
    let modified = metadata
        .modified()
        .ok()
        .and_then(|time| time.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|duration| duration.as_nanos());
    Ok(MetadataCacheKey {
        path: path.to_path_buf(),
        size: metadata.len(),
        modified,
    })
}

/// [`collect_file_metadata`] for a file shown under the Files `root`. A
/// symlink whose target resolves outside the root is never opened or
/// followed: it gets the common four lines with `-` values, so selecting it
/// cannot read files outside the tree.
pub(crate) fn collect_file_metadata_within(
    path: &Path,
    root: Option<&Path>,
) -> Result<FileInfo, String> {
    let link = std::fs::symlink_metadata(path).map_err(|err| err.to_string())?;
    if link.file_type().is_symlink() {
        let inside = match (root.map(Path::canonicalize), path.canonicalize()) {
            (Some(Ok(root)), Ok(target)) => target.starts_with(root),
            _ => false,
        };
        if !inside {
            return Ok(FileInfo {
                key: MetadataCacheKey {
                    path: path.to_path_buf(),
                    size: 0,
                    modified: None,
                },
                title: path
                    .file_name()
                    .and_then(|name| name.to_str())
                    .unwrap_or("file")
                    .to_string(),
                lines: vec![
                    "LINK".to_string(),
                    "-".to_string(),
                    created_line(None),
                    modified_line(None),
                ],
            });
        }
    }
    collect_file_metadata(path)
}

/// File details for the reserved details frame: always exactly four lines
/// (kind/format, size[/duration], created, modified). Runs in the blocking
/// metadata task; format probes read headers only.
pub(crate) fn collect_file_metadata(path: &Path) -> Result<FileInfo, String> {
    let key = metadata_key(path)?;
    let extension = path
        .extension()
        .and_then(|value| value.to_str())
        .unwrap_or("")
        .to_ascii_lowercase();
    let summary = super::file_metadata::probe(path, &extension, key.size);
    let kind = summary.as_ref().map_or_else(
        || {
            if extension.is_empty() {
                "file".to_string()
            } else {
                extension.to_ascii_uppercase()
            }
        },
        |summary| summary.kind.clone(),
    );
    let size = super::file_metadata::format_binary_size(key.size);
    let size_line = match summary.and_then(|summary| summary.duration) {
        Some(duration) => format!("{size} · {duration}"),
        None => size,
    };
    let metadata = std::fs::metadata(path).ok();
    let lines = vec![
        kind,
        size_line,
        created_line(
            metadata
                .as_ref()
                .and_then(|metadata| metadata.created().ok())
                .and_then(local_file_datetime),
        ),
        modified_line(
            metadata
                .as_ref()
                .and_then(|metadata| metadata.modified().ok())
                .and_then(local_file_datetime),
        ),
    ];
    Ok(FileInfo {
        key,
        title: path
            .file_name()
            .and_then(|name| name.to_str())
            .unwrap_or("file")
            .to_string(),
        lines,
    })
}

/// Whole Unix seconds for a `SystemTime`, flooring times before the epoch.
/// `None` only when the value does not fit in `i64`.
pub(crate) fn unix_seconds(time: std::time::SystemTime) -> Option<i64> {
    match time.duration_since(std::time::UNIX_EPOCH) {
        Ok(after) => i64::try_from(after.as_secs()).ok(),
        Err(before) => {
            let before = before.duration();
            let whole = i64::try_from(before.as_secs()).ok()?;
            let partial = i64::from(before.subsec_nanos() > 0);
            whole.checked_add(partial).map(|seconds| -seconds)
        }
    }
}

/// Converts a file timestamp to the OS local time zone.
fn local_file_datetime(time: std::time::SystemTime) -> Option<time::PrimitiveDateTime> {
    crate::platform::local_datetime_at(unix_seconds(time)?)
}

/// `YYYY/MM/DD HH:mm` (no seconds), or `-` when unavailable.
pub(crate) fn format_file_datetime(datetime: Option<time::PrimitiveDateTime>) -> String {
    let Some(datetime) = datetime else {
        return "-".to_string();
    };
    let year = datetime.year();
    if !(0..=9999).contains(&year) {
        return "-".to_string();
    }
    format!(
        "{year:04}/{:02}/{:02} {:02}:{:02}",
        u8::from(datetime.month()),
        datetime.day(),
        datetime.hour(),
        datetime.minute(),
    )
}

pub(crate) fn created_line(datetime: Option<time::PrimitiveDateTime>) -> String {
    format!("作成 {}", format_file_datetime(datetime))
}

pub(crate) fn modified_line(datetime: Option<time::PrimitiveDateTime>) -> String {
    format!("更新 {}", format_file_datetime(datetime))
}

/// Middle-elide while preserving the final extension and Unicode display width.
pub(crate) fn elide_filename(name: &str, width: usize) -> String {
    if UnicodeWidthStr::width(name) <= width {
        return name.to_string();
    }
    if width == 0 {
        return String::new();
    }
    if width == 1 {
        return "…".to_string();
    }

    let compound_extension = [".tar.gz", ".tar.bz2", ".tar.xz", ".tar.zst"]
        .iter()
        .find(|extension| name.ends_with(**extension))
        .copied();
    let (stem, extension) = if let Some(extension) = compound_extension {
        (
            &name[..name.len().saturating_sub(extension.len())],
            extension.to_string(),
        )
    } else {
        name.rsplit_once('.')
            .filter(|(stem, _)| !stem.is_empty())
            .map(|(stem, ext)| (stem, format!(".{ext}")))
            .unwrap_or((name, String::new()))
    };
    if extension.is_empty() {
        return super::text::middle_elide(name, width);
    }
    let extension_width = UnicodeWidthStr::width(extension.as_str());
    let available = width.saturating_sub(extension_width + 1);
    if available == 0 {
        return truncate_width(&extension, width);
    }

    let head = take_width(stem, available.div_ceil(2), false);
    let tail = take_width(stem, available / 2, true);
    truncate_width(&format!("{head}…{tail}{extension}"), width)
}

fn take_width(text: &str, width: usize, from_end: bool) -> String {
    let mut result = String::new();
    let mut used = 0;
    let chars = if from_end {
        text.chars().rev().collect::<Vec<_>>()
    } else {
        text.chars().collect::<Vec<_>>()
    };
    for ch in chars {
        let char_width = UnicodeWidthChar::width(ch).unwrap_or(0);
        if used + char_width > width {
            break;
        }
        if from_end {
            result.insert(0, ch);
        } else {
            result.push(ch);
        }
        used += char_width;
    }
    result
}

fn truncate_width(text: &str, width: usize) -> String {
    take_width(text, width, false)
}

pub(crate) fn render_files(app: &AppState, frame: &mut Frame, area: Rect) {
    let layout = files_section_layout(area);
    if layout.tree.height == 0 {
        return;
    }
    let Some(active_pane) = app
        .active
        .and_then(|idx| app.workspaces.get(idx)?.focused_pane_id())
    else {
        return;
    };
    let pane_id = active_pane;
    let Some(state) = app.files_by_pane.get(&pane_id) else {
        return;
    };
    let files_active = app.files_focus == Some(pane_id);
    let p = &app.palette;
    frame.render_widget(
        Paragraph::new(Span::styled("files", Style::default().fg(p.overlay1))),
        layout.header,
    );
    let enabled = files_buttons_enabled(state);
    let pressed = app
        .files_button_press
        .filter(|press| press.pane_id == pane_id)
        .map(|press| press.index);
    let mut button_spans = Vec::with_capacity(FILES_BUTTON_LABELS.len() * 2);
    for (index, label) in FILES_BUTTON_LABELS.iter().enumerate() {
        if index > 0 {
            button_spans.push(Span::raw(" "));
        }
        button_spans.push(Span::styled(
            *label,
            files_button_style(p, enabled[index], enabled[index] && pressed == Some(index)),
        ));
    }
    frame.render_widget(Paragraph::new(Line::from(button_spans)), layout.button_row);
    render_file_details(state, frame, layout.details, p, files_active);
    let tree = layout.tree;
    if let Some(message) = &state.message {
        frame.render_widget(
            Paragraph::new(Span::styled(message, Style::default().fg(p.red))),
            tree,
        );
        return;
    }
    if state.root.is_none() {
        return;
    }
    let view = files_tree_view(state, tree);
    let mut lines = Vec::new();
    let mut focused_row = None;
    for (offset, row) in view
        .rows
        .iter()
        .skip(view.scroll)
        .take(view.viewport_rows)
        .enumerate()
    {
        let colors = files_row_colors(
            p,
            row,
            state.focused_dir.as_deref(),
            state.expanded_dir.as_deref(),
            state.selected_file.as_deref(),
            files_active,
        );
        // The focused directory is identified by exact path, never by row index.
        if let Some(bg) = colors.row_bg {
            focused_row = Some((offset as u16, bg));
        }
        let mut spans = tree_prefix_segments(row)
            .into_iter()
            .zip(colors.segments)
            .map(|(segment, color)| Span::styled(segment, Style::default().fg(color)))
            .collect::<Vec<_>>();
        let name = elide_filename(
            &row.name,
            (view.text_width as usize).saturating_sub(tree_prefix_width(row)),
        );
        spans.push(Span::styled(name, colors.name));
        lines.push(Line::from(spans));
    }
    frame.render_widget(
        Paragraph::new(lines),
        Rect::new(tree.x, tree.y, view.text_width, tree.height),
    );
    if let Some((offset, bg)) = focused_row {
        // Whole tree row (indent, branch marker, name), excluding the
        // scrollbar. Only the background: the row's own colors already
        // account for it.
        let row_rect = Rect::new(tree.x, tree.y + offset, view.text_width, 1);
        frame
            .buffer_mut()
            .set_style(row_rect, Style::default().bg(bg));
    }
    if view.show_scrollbar {
        let max_scroll = view.rows.len().saturating_sub(view.viewport_rows);
        let track = Rect::new(tree.x + tree.width - 1, tree.y, 1, tree.height);
        let metrics = crate::pane::ScrollMetrics {
            offset_from_bottom: max_scroll.saturating_sub(view.scroll),
            max_offset_from_bottom: max_scroll,
            viewport_rows: view.viewport_rows,
        };
        if should_show_scrollbar(metrics) {
            render_scrollbar(frame, metrics, track, p.surface_dim, p.overlay0, "▕");
        }
    }
}

/// Draws the file information frame inside the reserved details rows. Empty
/// rows stay empty, so selecting or clearing a file never moves anything.
fn render_file_details(
    state: &FilesPaneState,
    frame: &mut Frame,
    details: Rect,
    p: &crate::app::state::Palette,
    files_active: bool,
) {
    if details.height == 0 {
        return;
    }
    match &state.info {
        FileInfoState::Ready(info) => {
            let Some(frame_rect) = details_frame_rect(state, details) else {
                return;
            };
            // Darker lime border relates the frame to the selected file;
            // the body stays neutral and the frame gets no background.
            let border = Style::default().fg(files_details_border_color(p));
            let title_style = files_selected_file_style(p, files_active);
            let body_style = Style::default().fg(p.overlay1);
            let body_rows = (frame_rect.height as usize).saturating_sub(2);
            let body = &info.lines[..info.lines.len().min(body_rows)];
            let info_lines = info_frame_rows(&info.title, body, frame_rect.width as usize)
                .into_iter()
                .map(|parts| {
                    Line::from(
                        parts
                            .into_iter()
                            .map(|(text, part)| {
                                let style = match part {
                                    InfoFramePart::Border => border,
                                    InfoFramePart::Title => title_style,
                                    InfoFramePart::Body => body_style,
                                };
                                Span::styled(text, style)
                            })
                            .collect::<Vec<_>>(),
                    )
                })
                .collect::<Vec<_>>();
            frame.render_widget(Paragraph::new(info_lines), frame_rect);
        }
        FileInfoState::Loading | FileInfoState::Error(_) => {
            let (message, style) = match &state.info {
                FileInfoState::Error(message) => (message.as_str(), Style::default().fg(p.red)),
                _ => ("loading…", Style::default().fg(p.overlay1)),
            };
            let width = details_frame_width(details) as usize;
            frame.render_widget(
                Paragraph::new(Span::styled(
                    super::text::truncate_end(message, width),
                    style,
                )),
                Rect::new(details.x, details.y, details_frame_width(details), 1),
            );
        }
        FileInfoState::Hidden => {}
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum InfoFramePart {
    Border,
    Title,
    Body,
}

/// Builds the file information frame as rows of styled parts. Every row has
/// exactly `width` terminal columns (measured with `unicode-width`); corners
/// and side borders take priority over the title, which is middle-elided with
/// its extension kept and dropped entirely when no room remains.
pub(crate) fn info_frame_rows(
    title: &str,
    body: &[String],
    width: usize,
) -> Vec<Vec<(String, InfoFramePart)>> {
    use InfoFramePart::{Body, Border, Title};
    if width < 2 {
        return Vec::new();
    }
    let inner = width - 2;
    // "┏" + " " + title + " " + "━"(≥1) + "┓"
    let title_room = width.saturating_sub(5);
    let title = if title_room > 0 {
        elide_filename(title, title_room)
    } else {
        String::new()
    };
    let title_width = UnicodeWidthStr::width(title.as_str());
    let top = if title.is_empty() || title_width > title_room {
        vec![
            ("┏".to_string(), Border),
            ("━".repeat(inner), Border),
            ("┓".to_string(), Border),
        ]
    } else {
        vec![
            ("┏ ".to_string(), Border),
            (title, Title),
            (format!(" {}┓", "━".repeat(width - 4 - title_width)), Border),
        ]
    };
    let mut rows = vec![top];
    for line in body {
        let text = super::text::truncate_end(line, inner);
        let pad = inner.saturating_sub(UnicodeWidthStr::width(text.as_str()));
        rows.push(vec![
            ("┃".to_string(), Border),
            (format!("{text}{}", " ".repeat(pad)), Body),
            ("┃".to_string(), Border),
        ]);
    }
    rows.push(vec![
        ("┗".to_string(), Border),
        ("━".repeat(inner), Border),
        ("┛".to_string(), Border),
    ]);
    rows
}

/// How much of the theme accent is blended into the panel background for the
/// focused directory while Files has keyboard focus. The result reads as a
/// translucent neon-blue band without adding a palette entry.
const FILES_FOCUS_ACCENT_MIX: f32 = 0.42;

/// Focused directory row.
/// - Files has keyboard focus: the theme accent (neon blue in the default
///   theme) blended into the panel background.
/// - PTY has focus: the existing sidebar selection background, which is darker
///   and neutral, so the two states are distinguishable.
///
/// The foreground is the theme text color unless it lacks contrast on the
/// chosen background, in which case black or white is used.
#[cfg(test)]
pub(crate) fn files_focused_dir_style(p: &crate::app::state::Palette, files_active: bool) -> Style {
    let bg = files_focus_bg(p, files_active);
    Style::default().fg(files_contrast_fg(p, bg)).bg(bg)
}

/// Background of the focused directory row (see [`files_focused_dir_style`]).
pub(crate) fn files_focus_bg(
    p: &crate::app::state::Palette,
    files_active: bool,
) -> ratatui::style::Color {
    if files_active {
        files_active_focus_bg(p)
    } else {
        super::sidebar::workspace_selection_background(p, true)
    }
}

/// The sidebar's own background as RGB, for blends and contrast checks.
fn files_base_rgb(p: &crate::app::state::Palette) -> Option<(u8, u8, u8)> {
    [p.panel_bg, p.surface_dim, p.active_row_bg]
        .into_iter()
        .find_map(super::panes::color_to_rgb)
}

fn files_active_focus_bg(p: &crate::app::state::Palette) -> ratatui::style::Color {
    let base = files_base_rgb(p);
    match (base, super::panes::color_to_rgb(p.accent)) {
        (Some(base), Some(accent)) => {
            let (r, g, b) = super::panes::mix_rgb(base, accent, FILES_FOCUS_ACCENT_MIX);
            ratatui::style::Color::Rgb(r, g, b)
        }
        _ => p.accent,
    }
}

fn files_contrast_fg(
    p: &crate::app::state::Palette,
    bg: ratatui::style::Color,
) -> ratatui::style::Color {
    let Some(bg_rgb) = super::panes::color_to_rgb(bg) else {
        return p.text;
    };
    let bg_luminance = super::panes::relative_luminance(bg_rgb);
    if let Some(text) = super::panes::color_to_rgb(p.text) {
        if contrast_ratio(super::panes::relative_luminance(text), bg_luminance) >= 4.5 {
            return p.text;
        }
    }
    if contrast_ratio(1.0, bg_luminance) >= contrast_ratio(0.0, bg_luminance) {
        ratatui::style::Color::White
    } else {
        ratatui::style::Color::Black
    }
}

fn contrast_ratio(a: f32, b: f32) -> f32 {
    let (light, dark) = if a >= b { (a, b) } else { (b, a) };
    (light + 0.05) / (dark + 0.05)
}

/// Selected file name token: pale fluorescent lime (green leaning) on a
/// faint lime background blended into the sidebar background. Slightly
/// dimmer when the PTY has keyboard focus, so the selection stays visible.
/// Only the name token uses this; guides keep the folder-hierarchy blue.
pub(crate) fn files_selected_file_style(
    p: &crate::app::state::Palette,
    files_active: bool,
) -> Style {
    let (Some(lime), Some(base)) = (lime_rgb(p, SELECTED_LIME_YELLOW), files_base_rgb(p)) else {
        // Named-color theme: no blends available.
        return Style::default()
            .fg(if files_active {
                ratatui::style::Color::LightGreen
            } else {
                ratatui::style::Color::Green
            })
            .bg(p.selection_bg);
    };
    let bg = rgb_color(super::panes::mix_rgb(
        base,
        lime,
        if files_active { 0.24 } else { 0.15 },
    ));
    let seed = rgb_color(super::panes::mix_rgb(
        lime,
        super::panes::color_to_rgb(p.text).unwrap_or(lime),
        0.2,
    ));
    let ease: &[f32] = if files_active {
        &[0.0]
    } else {
        &[0.2, 0.1, 0.0]
    };
    Style::default()
        .fg(theme_color(p, seed, ease, &[0.2, 0.35, 0.5, 0.7], bg))
        .bg(bg)
}

/// How far the theme green is pulled toward the theme yellow for the
/// selected file (green-leaning lime) and for the buttons (yellow-leaning
/// yellow-green). Same hue family, distinguishable roles.
const SELECTED_LIME_YELLOW: f32 = 0.3;
const BUTTON_LIME_YELLOW: f32 = 0.62;

/// The theme green blended toward the theme yellow by `toward_yellow`.
fn lime_rgb(p: &crate::app::state::Palette, toward_yellow: f32) -> Option<(u8, u8, u8)> {
    let green = super::panes::color_to_rgb(p.green)?;
    let yellow = super::panes::color_to_rgb(p.yellow)?;
    Some(super::panes::mix_rgb(green, yellow, toward_yellow))
}

fn rgb_color((r, g, b): (u8, u8, u8)) -> ratatui::style::Color {
    ratatui::style::Color::Rgb(r, g, b)
}

/// Border of the file information frame: the selected-file lime, darker
/// (eased toward the background) so it reads as related but secondary.
pub(crate) fn files_details_border_color(p: &crate::app::state::Palette) -> ratatui::style::Color {
    let Some(lime) = lime_rgb(p, SELECTED_LIME_YELLOW) else {
        return ratatui::style::Color::Green;
    };
    theme_color(
        p,
        rgb_color(lime),
        &[0.35, 0.25, 0.15, 0.0],
        &[0.2, 0.4],
        p.panel_bg,
    )
}

/// Contrast every tree and button color must keep against its background.
pub(crate) const FILES_MIN_CONTRAST: f32 = 3.0;

/// Which highlight a piece of a tree row gets.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum FilesTone {
    /// The existing colors: ancestors of the open directory, other branches.
    Normal,
    /// The open (expanded, listing applied) directory: bright neon blue.
    Expanded,
    /// Inside the open directory: a paler blue.
    Descendant,
    /// The selected file name: pale fluorescent green.
    SelectedFile,
}

/// Tones of one tree row: its name and each two-column prefix cell (one per
/// depth level, as [`tree_prefix_segments`]).
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct FilesRowTones {
    pub name: FilesTone,
    pub segments: Vec<FilesTone>,
}

/// How many levels `path` is below `ancestor` (0 = same path), by path
/// components, never by string prefix (`/a/bc` is not below `/a/b`).
fn levels_below(path: &Path, ancestor: &Path) -> Option<usize> {
    path.ancestors().position(|candidate| candidate == ancestor)
}

/// Pure tone decision for a tree row. Name and prefix cells are decided
/// separately.
///
/// File names: 1. selected file (lime), 2. normal (neutral). A file name is
/// never blue, even inside the expanded directory.
///
/// Folder names: 1. expanded directory (bright blue), 2. inside it (paler
/// blue), 3. ancestors and other branches (normal). The focused-row
/// background is a separate layer (see [`files_row_colors`]).
///
/// Prefix cells: the expanded directory's own branch glyph is bright blue;
/// cells of levels inside it are paler blue on every row below it (file rows
/// included, so their ancestor `│` reads as hierarchy); the rest is normal.
pub(crate) fn files_row_tones(
    row: &FileRow,
    expanded_dir: Option<&Path>,
    selected_file: Option<&Path>,
) -> FilesRowTones {
    let depth = usize::from(row.depth);
    let below = expanded_dir.and_then(|expanded| levels_below(&row.path, expanded));
    let name = match row.kind {
        FileRowKind::File if selected_file == Some(row.path.as_path()) => FilesTone::SelectedFile,
        FileRowKind::File => FilesTone::Normal,
        FileRowKind::Directory => match below {
            Some(0) => FilesTone::Expanded,
            Some(_) => FilesTone::Descendant,
            None => FilesTone::Normal,
        },
    };
    let segments = (1..=depth)
        .map(|level| match below {
            Some(0) if level == depth => FilesTone::Expanded,
            // Levels deeper than the expanded directory belong to its subtree.
            Some(levels) if levels > 0 && level > depth.saturating_sub(levels) => {
                FilesTone::Descendant
            }
            _ => FilesTone::Normal,
        })
        .collect();
    FilesRowTones { name, segments }
}

/// Concrete colors of one tree row, with the three roles kept apart:
/// the whole-row background, the name token style, and the prefix cells.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct FilesRowColors {
    pub tones: FilesRowTones,
    /// Whole-row background: only the focused directory has one.
    pub row_bg: Option<ratatui::style::Color>,
    /// Name token style (foreground; plus the token background for a
    /// selected file name).
    pub name: Style,
    /// Foreground of each prefix cell.
    pub segments: Vec<ratatui::style::Color>,
    /// The row is the focused directory and gets the focus-row background.
    pub focused: bool,
}

/// Pure color decision for a tree row from its path/kind and the Files state.
/// Uses the theme palette only (blue, green, text, overlays) and blends with
/// the sidebar background; every color keeps [`FILES_MIN_CONTRAST`] against
/// the background it is drawn on, including the focused-row background
/// (which depends on whether Files has keyboard focus).
pub(crate) fn files_row_colors(
    p: &crate::app::state::Palette,
    row: &FileRow,
    focused_dir: Option<&Path>,
    expanded_dir: Option<&Path>,
    selected_file: Option<&Path>,
    files_active: bool,
) -> FilesRowColors {
    let tones = files_row_tones(row, expanded_dir, selected_file);
    // Layer 1: the row background (focused directory only).
    let focused = row.kind == FileRowKind::Directory && focused_dir == Some(row.path.as_path());
    let focus_bg = focused.then(|| files_focus_bg(p, files_active));
    let row_bg = focus_bg.unwrap_or(p.panel_bg);
    let normal_name = if row.depth == 0 {
        p.accent
    } else if row.kind == FileRowKind::Directory {
        p.text
    } else {
        p.overlay1
    };
    let readable = |color| {
        if focused {
            files_readable_on(p, color, row_bg)
        } else {
            color
        }
    };
    // Layer 2: the name token; layer 3: the prefix cells.
    let name = match tones.name {
        FilesTone::SelectedFile => files_selected_file_style(p, files_active),
        FilesTone::Normal => Style::default().fg(readable(normal_name)),
        tone => Style::default().fg(readable(files_tone_color(p, tone, row_bg))),
    };
    let segments = tones
        .segments
        .iter()
        .map(|tone| match tone {
            FilesTone::Normal => readable(p.overlay0),
            tone => readable(files_tone_color(p, *tone, row_bg)),
        })
        .collect();
    FilesRowColors {
        tones,
        row_bg: focus_bg,
        name,
        segments,
        focused,
    }
}

/// `color`, or the readable text color when it lacks contrast on `bg`.
fn files_readable_on(
    p: &crate::app::state::Palette,
    color: ratatui::style::Color,
    bg: ratatui::style::Color,
) -> ratatui::style::Color {
    match (
        super::panes::color_to_rgb(color),
        super::panes::color_to_rgb(bg),
    ) {
        (Some(fg), Some(bg_rgb)) if rgb_contrast(fg, bg_rgb) < FILES_MIN_CONTRAST => {
            files_contrast_fg(p, bg)
        }
        _ => color,
    }
}

fn rgb_contrast(a: (u8, u8, u8), b: (u8, u8, u8)) -> f32 {
    contrast_ratio(
        super::panes::relative_luminance(a),
        super::panes::relative_luminance(b),
    )
}

/// The first blend of `color` toward `target` (in `amounts` order) that
/// keeps [`FILES_MIN_CONTRAST`] on `bg`; `None` when none does (or the colors
/// are not RGB).
fn readable_blend(
    color: ratatui::style::Color,
    target: Option<(u8, u8, u8)>,
    amounts: &[f32],
    bg: ratatui::style::Color,
) -> Option<ratatui::style::Color> {
    let (Some(base), Some(target), Some(bg)) = (
        super::panes::color_to_rgb(color),
        target,
        super::panes::color_to_rgb(bg),
    ) else {
        return None;
    };
    amounts
        .iter()
        .map(|amount| super::panes::mix_rgb(base, target, *amount))
        .find(|candidate| rgb_contrast(*candidate, bg) >= FILES_MIN_CONTRAST)
        .map(|(r, g, b)| ratatui::style::Color::Rgb(r, g, b))
}

/// A theme color made readable on `bg` (all blends of palette colors, no
/// fixed hues). Tried in order, first readable wins:
/// 1. eased toward the sidebar background by `ease` (paler);
/// 2. lifted toward the text color by `lift`;
/// 3. pushed toward black/white, whichever contrasts with `bg`.
///
/// Non-RGB palettes (the `terminal` theme) keep the named color.
fn theme_color(
    p: &crate::app::state::Palette,
    color: ratatui::style::Color,
    ease: &[f32],
    lift: &[f32],
    bg: ratatui::style::Color,
) -> ratatui::style::Color {
    let extreme = super::panes::color_to_rgb(bg).map(|bg| {
        if super::panes::relative_luminance(bg) > 0.4 {
            (0, 0, 0)
        } else {
            (255, 255, 255)
        }
    });
    readable_blend(color, files_base_rgb(p), ease, bg)
        .or_else(|| readable_blend(color, super::panes::color_to_rgb(p.text), lift, bg))
        .or_else(|| readable_blend(color, extreme, &[0.2, 0.4, 0.6, 0.8], bg))
        .unwrap_or(color)
}

/// The bright variant of a named (non-RGB) terminal color, for the
/// `terminal` theme where blends are impossible.
fn brighter_named(color: ratatui::style::Color) -> ratatui::style::Color {
    use ratatui::style::Color;
    match color {
        Color::Yellow => Color::LightYellow,
        Color::Blue => Color::LightBlue,
        Color::Green => Color::LightGreen,
        Color::Red => Color::LightRed,
        Color::Cyan => Color::LightCyan,
        Color::Magenta => Color::LightMagenta,
        other => other,
    }
}

/// Theme-derived color of a highlight tone drawn on `bg`.
/// - Expanded: the theme blue lifted toward the text color (brighter neon).
/// - Descendant: the theme blue eased toward the background (paler).
/// - SelectedFile: the selected-file lime (see [`files_selected_file_style`]).
pub(crate) fn files_tone_color(
    p: &crate::app::state::Palette,
    tone: FilesTone,
    bg: ratatui::style::Color,
) -> ratatui::style::Color {
    match tone {
        FilesTone::Normal => p.text,
        // No RGB to blend with (terminal theme, default background): use
        // the bright named variant so it still differs from descendants.
        FilesTone::Expanded
            if super::panes::color_to_rgb(p.blue).is_none()
                || super::panes::color_to_rgb(bg).is_none() =>
        {
            brighter_named(p.blue)
        }
        FilesTone::Expanded => theme_color(p, p.blue, &[], &[0.2, 0.35, 0.5], bg),
        FilesTone::Descendant => {
            theme_color(p, p.blue, &[0.3, 0.2, 0.1, 0.0], &[0.2, 0.35, 0.5, 0.7], bg)
        }
        FilesTone::SelectedFile => files_selected_file_style(p, true).fg.unwrap_or(p.green),
    }
}

/// Labels of the Files action row, in hit-test order.
pub(crate) const FILES_BUTTON_LABELS: [&str; 4] = ["[↑]", "[↓]", "[open]", "[path]"];

/// How long a pressed button keeps its pressed look at minimum, so a quick
/// click (or a key press, which has no release) is still visible.
pub(crate) const FILES_BUTTON_PRESS_MIN: Duration = Duration::from_millis(140);

/// A pressed Files button. Cleared by the runtime tick once released and
/// past `visible_until`; the UI thread never sleeps for it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct FilesButtonPress {
    pub pane_id: crate::layout::PaneId,
    pub index: usize,
    /// The mouse button is still down on it.
    pub held: bool,
    pub visible_until: Instant,
}

impl FilesButtonPress {
    pub(crate) fn new(
        pane_id: crate::layout::PaneId,
        index: usize,
        held: bool,
        now: Instant,
    ) -> Self {
        Self {
            pane_id,
            index,
            held,
            visible_until: now + FILES_BUTTON_PRESS_MIN,
        }
    }

    pub(crate) fn expired(&self, now: Instant) -> bool {
        !self.held && now >= self.visible_until
    }
}

/// Which of `[↑] [↓] [open] [path]` can act: ↑/↓ need a directory to move
/// to, open/path need a focused directory.
pub(crate) fn files_buttons_enabled(state: &FilesPaneState) -> [bool; 4] {
    let dirs = state.focusable_directories();
    let position = state
        .focused_dir
        .as_ref()
        .and_then(|focused| dirs.iter().position(|dir| dir == focused));
    let has_focus = state.focused_dir.is_some();
    [
        position.is_some_and(|index| index > 0),
        position.is_some_and(|index| index + 1 < dirs.len()),
        has_focus,
        has_focus,
    ]
}

/// Button look, in the yellow-green family of the selected file but
/// leaning further toward the theme yellow, with no background at rest.
/// Priority: 1. pressed, 2. enabled, 3. disabled.
/// - pressed: brighter fluorescent yellow-green, bold, on a faint
///   yellow-green glow background blended into the sidebar background;
/// - enabled: pale yellow-green, no background;
/// - disabled: the same hue desaturated (toward the overlay gray) and dimmed
///   (toward the background); never shown pressed.
pub(crate) fn files_button_style(
    p: &crate::app::state::Palette,
    enabled: bool,
    pressed: bool,
) -> Style {
    let (Some(lime), Some(base)) = (lime_rgb(p, BUTTON_LIME_YELLOW), files_base_rgb(p)) else {
        // Named-color theme.
        use ratatui::style::Color;
        return match (enabled, pressed) {
            (true, true) => Style::default()
                .fg(Color::LightYellow)
                .bg(p.surface1)
                .add_modifier(ratatui::style::Modifier::BOLD),
            (true, false) => Style::default().fg(Color::Yellow),
            (false, _) => Style::default().fg(Color::DarkGray),
        };
    };
    let text = super::panes::color_to_rgb(p.text).unwrap_or(lime);
    if enabled && pressed {
        let glow = rgb_color(super::panes::mix_rgb(base, lime, 0.3));
        // Brighter: toward white on dark backgrounds; on light ones the
        // glow carries the press and the text lift keeps it vivid.
        let bright = if super::panes::relative_luminance(base) < 0.4 {
            rgb_color(super::panes::mix_rgb(lime, (255, 255, 255), 0.4))
        } else {
            rgb_color(super::panes::mix_rgb(lime, text, 0.25))
        };
        return Style::default()
            .fg(theme_color(p, bright, &[], &[0.2, 0.4, 0.6, 0.8], glow))
            .bg(glow)
            .add_modifier(ratatui::style::Modifier::BOLD);
    }
    if enabled {
        let pale = rgb_color(super::panes::mix_rgb(lime, text, 0.12));
        return Style::default().fg(theme_color(p, pale, &[0.0], &[0.2, 0.35, 0.5], p.panel_bg));
    }
    let gray = super::panes::color_to_rgb(p.overlay0).unwrap_or(base);
    let muted = super::panes::mix_rgb(lime, gray, 0.5);
    Style::default().fg(rgb_color(super::panes::mix_rgb(muted, base, 0.45)))
}

/// 描画と入力が同じ行位置・文字幅を使うためのFiles実座標を生成する。
pub(crate) fn files_hit_geometry(
    app: &AppState,
    area: Rect,
    divider: Rect,
    resources: Rect,
) -> Option<FilesHitGeometry> {
    let layout = files_section_layout(area);
    if layout.tree.height == 0 {
        return None;
    }
    let pane_id = app
        .active
        .and_then(|idx| app.workspaces.get(idx)?.focused_pane_id())?;
    let state = app.files_by_pane.get(&pane_id)?;
    let tree = layout.tree;
    let view = files_tree_view(state, tree);
    let row_hits = view
        .rows
        .iter()
        .skip(view.scroll)
        .take(view.viewport_rows)
        .enumerate()
        .map(|(offset, row)| {
            let prefix_width = tree_prefix_width(row);
            let name_width = (view.text_width as usize).saturating_sub(prefix_width);
            let name = elide_filename(&row.name, name_width);
            let name_x = tree
                .x
                .saturating_add(prefix_width.min(u16::MAX as usize) as u16);
            FilesRowHit {
                row: Rect::new(tree.x, tree.y + offset as u16, view.text_width, 1),
                name: Rect::new(
                    name_x,
                    tree.y + offset as u16,
                    UnicodeWidthStr::width(name.as_str()).min(u16::MAX as usize) as u16,
                    1,
                ),
                path: row.path.clone(),
                is_dir: row.is_dir,
            }
        })
        .collect();
    let button_row = layout.button_row;
    let mut x = button_row.x;
    let mut buttons = [Rect::default(); 4];
    for (index, label) in FILES_BUTTON_LABELS.iter().enumerate() {
        let width = UnicodeWidthStr::width(*label) as u16;
        buttons[index] = Rect::new(x, button_row.y, width, button_row.height);
        x = x.saturating_add(width).saturating_add(1);
    }
    Some(FilesHitGeometry {
        pane_id,
        generation: state.focus_generation,
        area,
        viewport: Rect::new(tree.x, tree.y, view.text_width, tree.height),
        rows: row_hits,
        buttons,
        gap: layout.gap,
        button_row,
        details: layout.details,
        details_frame: details_frame_rect(state, layout.details),
        scrollbar: view
            .show_scrollbar
            .then(|| Rect::new(tree.x + tree.width - 1, tree.y, 1, tree.height)),
        divider,
        resources,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn filename_elision_keeps_extension() {
        let value = elide_filename("123456789.png", 8);
        assert!(value.ends_with(".png"));
        assert!(value.contains('…'));
    }

    #[test]
    fn listing_sorts_directories_before_files() {
        let mut entries = [
            FileEntry {
                path: PathBuf::from("b"),
                name: "b".into(),
                is_dir: false,
            },
            FileEntry {
                path: PathBuf::from("a"),
                name: "a".into(),
                is_dir: true,
            },
        ];
        entries.sort_by_key(|entry| std::cmp::Reverse(entry.is_dir));
        assert!(entries[0].is_dir);
    }

    fn dir(path: &Path) -> FileEntry {
        FileEntry {
            path: path.to_path_buf(),
            name: path.file_name().unwrap().to_string_lossy().into_owned(),
            is_dir: true,
        }
    }

    fn file(path: &Path) -> FileEntry {
        FileEntry {
            is_dir: false,
            ..dir(path)
        }
    }

    fn pane() -> crate::layout::PaneId {
        crate::layout::PaneId::from_raw(1)
    }

    /// Completes the pending request the way the runtime does.
    fn complete(state: &mut FilesPaneState, entries: Vec<FileEntry>) -> Result<(), &'static str> {
        let request = state.pending_expand.as_mut().expect("pending request");
        request.in_flight = true;
        let request = request.clone();
        state.apply_listing(&request, Ok(entries))
    }

    #[test]
    fn pane_states_are_independent_and_directory_focus_is_delayed() {
        let root = PathBuf::from("/tmp/files-root");
        let mut first = FilesPaneState::reset(Some(root.clone()));
        let second = FilesPaneState::reset(Some(root.clone()));
        let now = Instant::now();
        assert!(first.focus_dir_explicit(pane(), root.join("child"), now));
        let request = first
            .pending_expand
            .clone()
            .expect("expansion is scheduled");
        assert_eq!(request.target, root.join("child"));
        assert_eq!(request.due_at, now + FILES_EXPAND_DELAY);
        assert_eq!(
            first.check_expand(pane(), Some(pane()), now + Duration::from_millis(1999)),
            FilesExpandCheck::Waiting
        );
        assert_eq!(
            first.check_expand(pane(), Some(pane()), now + FILES_EXPAND_DELAY),
            FilesExpandCheck::Ready
        );
        assert_ne!(first.focus_generation, second.focus_generation);
        assert_eq!(second.focused_dir.as_ref(), Some(&root));
        assert!(second.pending_expand.is_none());
    }

    #[test]
    fn stale_listing_is_rejected_after_focus_changes() {
        let root = PathBuf::from("/tmp/root");
        let mut state = FilesPaneState::reset(Some(root.clone()));
        let now = Instant::now();
        state.focus_dir_explicit(pane(), root.join("a"), now);
        let mut stale = state.pending_expand.clone().unwrap();
        stale.in_flight = true;
        state.focus_dir_explicit(pane(), root.join("b"), now);
        assert_eq!(
            state.apply_listing(&stale, Ok(vec![file(&root.join("a/file"))])),
            Err("request_superseded")
        );
        assert!(!state.directories.contains_key(&root.join("a")));
        assert_eq!(state.expanded_dir.as_ref(), Some(&root));
    }

    #[test]
    fn stale_timer_is_rejected_when_any_identity_field_changes() {
        let root = PathBuf::from("/tmp/root");
        let now = Instant::now();
        let due = now + FILES_EXPAND_DELAY;
        let mut state = FilesPaneState::reset(Some(root.clone()));
        state.focus_dir_explicit(pane(), root.join("a"), now);
        let other = crate::layout::PaneId::from_raw(2);
        assert_eq!(
            state.check_expand(other, Some(other), due),
            FilesExpandCheck::Stale("pane_mismatch")
        );
        assert_eq!(
            state.check_expand(pane(), None, due),
            FilesExpandCheck::Stale("files_focus_mismatch")
        );
        let mut moved = state.clone();
        moved.focused_dir = Some(root.join("b"));
        assert_eq!(
            moved.check_expand(pane(), Some(pane()), due),
            FilesExpandCheck::Stale("focused_dir_mismatch")
        );
        let mut regenerated = state.clone();
        regenerated.focus_generation += 1;
        assert_eq!(
            regenerated.check_expand(pane(), Some(pane()), due),
            FilesExpandCheck::Stale("focus_generation_mismatch")
        );
        let mut rerequested = state.clone();
        rerequested.request_generation += 1;
        assert_eq!(
            rerequested.check_expand(pane(), Some(pane()), due),
            FilesExpandCheck::Stale("request_generation_mismatch")
        );
        let mut rerooted = state.clone();
        rerooted.root = Some(PathBuf::from("/tmp/other"));
        assert_eq!(
            rerooted.check_expand(pane(), Some(pane()), due),
            FilesExpandCheck::Stale("root_mismatch")
        );
        assert_eq!(
            state.check_expand(pane(), Some(pane()), due),
            FilesExpandCheck::Ready
        );
    }

    #[test]
    fn expanded_children_are_nested_under_their_parent_not_the_last_row() {
        let root = PathBuf::from("/tmp/root");
        let mut state = FilesPaneState::reset(Some(root.clone()));
        let (a, b, z) = (root.join("a"), root.join("b"), root.join("z"));
        state.accept_listing(&root, vec![dir(&a), dir(&b), dir(&z)]);
        state.focus_dir_explicit(pane(), a.clone(), Instant::now());
        complete(&mut state, vec![file(&a.join("inner"))]).unwrap();
        let paths = state
            .visible_rows()
            .into_iter()
            .map(|row| (row.path, row.depth))
            .collect::<Vec<_>>();
        assert_eq!(
            paths,
            vec![
                (root.clone(), 0),
                (a.clone(), 1),
                (a.join("inner"), 2),
                (b, 1),
                (z, 1),
            ]
        );
    }

    #[test]
    fn visible_rows_keep_one_active_branch() {
        let root = PathBuf::from("/tmp/root");
        let (a, a1, b) = (root.join("a"), root.join("a/a1"), root.join("b"));
        let mut state = FilesPaneState::reset(Some(root.clone()));
        state.accept_listing(&root, vec![dir(&a), dir(&b)]);
        state.focus_dir_explicit(pane(), a.clone(), Instant::now());
        complete(&mut state, vec![dir(&a1)]).unwrap();
        state.focus_dir_explicit(pane(), a1.clone(), Instant::now());
        complete(&mut state, vec![file(&a1.join("file"))]).unwrap();
        assert!(state
            .visible_rows()
            .iter()
            .any(|row| row.path == a1.join("file")));

        // Moving to another branch closes a/a1 immediately and forgets them.
        state.focus_dir_explicit(pane(), b.clone(), Instant::now());
        let rows = state.visible_rows();
        assert!(!rows
            .iter()
            .any(|row| row.path == a1 || row.path == a1.join("file")));
        assert_eq!(state.expanded_dir.as_ref(), Some(&root));
        assert_eq!(state.directories.keys().collect::<Vec<_>>(), vec![&root]);
    }

    #[test]
    fn focusing_an_open_ancestor_keeps_it_open_and_closes_below() {
        let root = PathBuf::from("/tmp/root");
        let (a, a1) = (root.join("a"), root.join("a/a1"));
        let mut state = FilesPaneState::reset(Some(root.clone()));
        state.accept_listing(&root, vec![dir(&a)]);
        state.focus_dir_explicit(pane(), a.clone(), Instant::now());
        complete(&mut state, vec![dir(&a1)]).unwrap();
        state.focus_dir_explicit(pane(), a1.clone(), Instant::now());
        complete(&mut state, vec![file(&a1.join("file"))]).unwrap();
        state.focus_dir_explicit(pane(), a.clone(), Instant::now());
        assert_eq!(state.expanded_dir.as_ref(), Some(&a));
        let rows = state.visible_rows();
        assert!(rows.iter().any(|row| row.path == a1));
        assert!(!rows.iter().any(|row| row.path == a1.join("file")));
    }

    #[test]
    fn focus_change_and_listing_never_reset_scroll() {
        let root = PathBuf::from("/tmp/root");
        let dirs = (0..20)
            .map(|idx| root.join(format!("d{idx:02}")))
            .collect::<Vec<_>>();
        let mut state = FilesPaneState::reset(Some(root.clone()));
        state.accept_listing(&root, dirs.iter().map(|path| dir(path)).collect());
        state.scroll = 12;
        state.focus_dir_explicit(pane(), dirs[15].clone(), Instant::now());
        assert_eq!(state.scroll, 12);
        complete(&mut state, vec![file(&dirs[15].join("x"))]).unwrap();
        assert_eq!(state.scroll, 12);
        assert!(!state.clamp_scroll(5));
        assert_eq!(state.scroll, 12);
        assert!(state.clamp_scroll(15));
        assert_eq!(state.scroll, state.visible_rows().len() - 15);
    }

    #[test]
    fn reselecting_the_focused_directory_does_not_restart_a_pending_timer() {
        let root = PathBuf::from("/tmp/root");
        let mut state = FilesPaneState::reset(Some(root.clone()));
        let now = Instant::now();
        state.focus_dir_explicit(pane(), root.join("a"), now);
        let first = state.pending_expand.clone();
        assert!(!state.focus_dir_explicit(pane(), root.join("a"), now + Duration::from_secs(1)));
        assert_eq!(state.pending_expand, first);
    }

    /// AppState with one plain pane whose Files tree has `count` directories
    /// under `/tmp/render-root`, laid out at the real sidebar geometry.
    fn render_fixture(count: usize) -> (AppState, crate::layout::PaneId, PathBuf, Vec<PathBuf>) {
        let mut app = AppState::test_new();
        let ws = crate::workspace::Workspace::test_new("render");
        let pane_id = ws.tabs[0].root_pane;
        app.workspaces = vec![ws];
        app.active = Some(0);
        app.mode = crate::app::Mode::Terminal;
        let root = PathBuf::from("/tmp/render-root");
        let dirs = (0..count)
            .map(|idx| root.join(format!("d{idx:02}")))
            .collect::<Vec<_>>();
        let mut state = FilesPaneState::reset(Some(root.clone()));
        state.accept_listing(&root, dirs.iter().map(|path| dir(path)).collect());
        app.files_by_pane.insert(pane_id, state);
        (app, pane_id, root, dirs)
    }

    // Tall enough that the tree keeps several rows above the fixed button
    // row and the six reserved detail rows.
    const VIEW: Rect = Rect {
        x: 0,
        y: 0,
        width: 106,
        height: 60,
    };

    fn draw(app: &mut AppState) -> (ratatui::buffer::Buffer, FilesHitGeometry) {
        crate::ui::compute_view(app, VIEW);
        let geometry = app.view.files_geometry.clone().expect("files geometry");
        let mut terminal =
            ratatui::Terminal::new(ratatui::backend::TestBackend::new(VIEW.width, VIEW.height))
                .expect("test terminal");
        terminal
            .draw(|frame| render_files(app, frame, geometry.area))
            .expect("draw");
        (terminal.backend().buffer().clone(), geometry)
    }

    fn row_of(geometry: &FilesHitGeometry, path: &Path) -> Rect {
        geometry
            .rows
            .iter()
            .find(|hit| hit.path == path)
            .map(|hit| hit.row)
            .unwrap_or_else(|| panic!("{path:?} not rendered"))
    }

    fn row_bgs(buffer: &ratatui::buffer::Buffer, row: Rect) -> Vec<ratatui::style::Color> {
        (row.x..row.x + row.width)
            .map(|x| buffer[(x, row.y)].bg)
            .collect()
    }

    #[test]
    fn focused_directory_row_is_highlighted_across_the_row_by_path() {
        let (mut app, pane_id, _root, dirs) = render_fixture(20);
        app.files_by_pane
            .get_mut(&pane_id)
            .unwrap()
            .focus_dir_explicit(pane_id, dirs[1].clone(), Instant::now());
        app.files_focus = Some(pane_id);
        let (buffer, geometry) = draw(&mut app);
        let active = files_focused_dir_style(&app.palette, true).bg.unwrap();
        let focused = row_of(&geometry, &dirs[1]);
        assert!(row_bgs(&buffer, focused).iter().all(|bg| *bg == active));
        // Indent and branch marker cells are part of the highlighted row.
        assert_eq!(buffer[(focused.x, focused.y)].bg, active);
        // The scrollbar column stays outside the highlight.
        assert!(geometry.viewport.width < geometry.area.width);
        let scrollbar_x = geometry.viewport.x + geometry.viewport.width;
        assert_ne!(buffer[(scrollbar_x, focused.y)].bg, active);
        // No other directory row is highlighted.
        for other in [&dirs[0], &dirs[2], &dirs[3]] {
            let rect = row_of(&geometry, other);
            assert!(row_bgs(&buffer, rect).iter().all(|bg| *bg != active));
        }

        // PTY focus keeps the selection visible with the inactive style.
        app.files_focus = None;
        let (buffer, geometry) = draw(&mut app);
        let inactive = files_focused_dir_style(&app.palette, false).bg.unwrap();
        assert_ne!(active, inactive);
        let focused = row_of(&geometry, &dirs[1]);
        assert!(row_bgs(&buffer, focused).iter().all(|bg| *bg == inactive));
    }

    #[test]
    fn focused_highlight_follows_the_path_after_scroll_and_listing_updates() {
        let (mut app, pane_id, root, dirs) = render_fixture(20);
        app.files_focus = Some(pane_id);
        let state = app.files_by_pane.get_mut(&pane_id).unwrap();
        state.focus_dir_explicit(pane_id, dirs[9].clone(), Instant::now());
        state.scroll = 6;
        let (buffer, geometry) = draw(&mut app);
        let active = files_focused_dir_style(&app.palette, true).bg.unwrap();
        let highlighted = geometry
            .rows
            .iter()
            .filter(|hit| buffer[(hit.row.x, hit.row.y)].bg == active)
            .map(|hit| hit.path.clone())
            .collect::<Vec<_>>();
        assert_eq!(highlighted, vec![dirs[9].clone()]);

        // A refreshed root listing with an extra entry shifts rows; the
        // highlight still lands on the same path, not the same index.
        let mut entries = vec![dir(&root.join("a-new"))];
        entries.extend(dirs.iter().map(|path| dir(path)));
        app.files_by_pane
            .get_mut(&pane_id)
            .unwrap()
            .directories
            .insert(root.clone(), entries);
        let (buffer, geometry) = draw(&mut app);
        let highlighted = geometry
            .rows
            .iter()
            .filter(|hit| buffer[(hit.row.x, hit.row.y)].bg == active)
            .map(|hit| hit.path.clone())
            .collect::<Vec<_>>();
        assert_eq!(highlighted, vec![dirs[9].clone()]);
    }

    #[test]
    fn selected_file_highlights_only_its_name_token() {
        let (mut app, pane_id, root, dirs) = render_fixture(3);
        let file_path = root.join("readme.md");
        let state = app.files_by_pane.get_mut(&pane_id).unwrap();
        let mut entries = dirs.iter().map(|path| dir(path)).collect::<Vec<_>>();
        entries.push(file(&file_path));
        state.directories.insert(root.clone(), entries);
        state.selected_file = Some(file_path.clone());
        app.files_focus = Some(pane_id);
        let (buffer, geometry) = draw(&mut app);
        let hit = geometry
            .rows
            .iter()
            .find(|hit| hit.path == file_path)
            .expect("file row");
        let selected = files_selected_file_style(&app.palette, true).bg.unwrap();
        for x in hit.row.x..hit.row.x + hit.row.width {
            let in_name = x >= hit.name.x && x < hit.name.x + hit.name.width;
            assert_eq!(buffer[(x, hit.row.y)].bg == selected, in_name, "x={x}");
        }
        // The focused directory (root) keeps its own row highlight.
        let focused = files_focused_dir_style(&app.palette, true).bg.unwrap();
        let root_row = row_of(&geometry, &root);
        assert!(row_bgs(&buffer, root_row).iter().all(|bg| *bg == focused));
    }

    fn rgb(color: ratatui::style::Color) -> (u8, u8, u8) {
        crate::ui::panes::color_to_rgb(color).expect("rgb color")
    }

    #[test]
    fn active_focus_color_is_a_brighter_accent_tint_than_the_old_selection() {
        let p = crate::app::state::Palette::catppuccin();
        let active = files_focused_dir_style(&p, true);
        let inactive = files_focused_dir_style(&p, false);
        let (active_bg, inactive_bg) = (active.bg.unwrap(), inactive.bg.unwrap());
        // Previously the active row used selection_bg; it is now the inactive one.
        assert_eq!(inactive_bg, p.selection_bg);
        assert_ne!(active_bg, inactive_bg);
        let lum = |color| crate::ui::panes::relative_luminance(rgb(color));
        assert!(lum(active_bg) > lum(p.selection_bg), "brighter than before");
        assert!(lum(inactive_bg) < lum(active_bg), "inactive is darker");
        // Derived from the theme accent (neon blue): blue dominates.
        let (r, g, b) = rgb(active_bg);
        assert!(b > r && b > g);
        // A tint between the panel background and the accent, not the accent.
        assert_ne!(active_bg, p.accent);
    }

    #[test]
    fn focus_colors_keep_text_readable_across_themes() {
        use crate::app::state::Palette;
        for p in [
            Palette::catppuccin(),
            Palette::catppuccin_latte(),
            Palette::tokyo_night(),
            Palette::tokyo_night_day(),
            Palette::dracula(),
            Palette::nord(),
            Palette::gruvbox(),
            Palette::gruvbox_light(),
            Palette::one_dark(),
            Palette::one_light(),
            Palette::solarized(),
            Palette::solarized_light(),
            Palette::kanagawa(),
            Palette::rose_pine(),
            Palette::vesper(),
            Palette::terminal(),
        ] {
            for active in [true, false] {
                let style = files_focused_dir_style(&p, active);
                let (Some(fg), Some(bg)) = (style.fg, style.bg) else {
                    panic!("focus style must set fg and bg");
                };
                assert_ne!(bg, ratatui::style::Color::Reset, "must be visible");
                if let (Some(fg), Some(bg)) = (
                    crate::ui::panes::color_to_rgb(fg),
                    crate::ui::panes::color_to_rgb(bg),
                ) {
                    let ratio = contrast_ratio(
                        crate::ui::panes::relative_luminance(fg),
                        crate::ui::panes::relative_luminance(bg),
                    );
                    assert!(ratio >= 3.0, "{p:?} active={active} ratio={ratio}");
                }
            }
            assert_ne!(
                files_focused_dir_style(&p, true).bg,
                files_focused_dir_style(&p, false).bg
            );
        }
    }

    fn fixed(y: i32, mo: u8, d: u8, h: u8, mi: u8, sec: u8) -> time::PrimitiveDateTime {
        time::PrimitiveDateTime::new(
            time::Date::from_calendar_date(y, time::Month::try_from(mo).unwrap(), d).unwrap(),
            time::Time::from_hms(h, mi, sec).unwrap(),
        )
    }

    #[test]
    fn file_datetime_is_slash_separated_zero_padded_without_seconds() {
        let value = format_file_datetime(Some(fixed(2026, 1, 5, 3, 7, 59)));
        assert_eq!(value, "2026/01/05 03:07");
        assert!(!value.contains('-'));
        assert!(!value.contains(":59"));
        assert_eq!(
            format_file_datetime(Some(fixed(2026, 11, 20, 14, 23, 1))),
            "2026/11/20 14:23"
        );
        assert_eq!(
            created_line(Some(fixed(2026, 11, 20, 14, 23, 0))),
            "作成 2026/11/20 14:23"
        );
        assert_eq!(
            modified_line(Some(fixed(2026, 11, 20, 14, 24, 0))),
            "更新 2026/11/20 14:24"
        );
    }

    #[test]
    fn unavailable_file_datetimes_show_a_dash() {
        assert_eq!(created_line(None), "作成 -");
        assert_eq!(modified_line(None), "更新 -");
        assert_eq!(format_file_datetime(Some(fixed(-5, 1, 1, 0, 0, 0))), "-");
    }

    #[test]
    fn unix_seconds_handles_pre_epoch_and_extreme_times_without_panicking() {
        let epoch = std::time::UNIX_EPOCH;
        assert_eq!(unix_seconds(epoch), Some(0));
        assert_eq!(unix_seconds(epoch + Duration::from_millis(1500)), Some(1));
        assert_eq!(unix_seconds(epoch - Duration::from_millis(1500)), Some(-2));
        for seconds in [u64::MAX / 4, u64::MAX / 2, i64::MAX as u64] {
            if let Some(time) = epoch.checked_add(Duration::from_secs(seconds)) {
                let _ = unix_seconds(time);
                let _ = local_file_datetime(time);
            }
            if let Some(time) = epoch.checked_sub(Duration::from_secs(seconds)) {
                let _ = unix_seconds(time);
                let _ = local_file_datetime(time);
            }
        }
    }

    fn is_local_datetime_text(value: &str) -> bool {
        let bytes = value.as_bytes();
        value.len() == 16
            && bytes.iter().enumerate().all(|(index, byte)| match index {
                4 | 7 => *byte == b'/',
                10 => *byte == b' ',
                13 => *byte == b':',
                _ => byte.is_ascii_digit(),
            })
    }

    #[cfg(unix)]
    #[test]
    fn local_time_conversion_yields_minutes_in_the_os_time_zone() {
        // 2026-11-20T05:23:45Z; the local wall clock depends on TZ, so only
        // the shape and the minute (all real zones are whole quarter hours
        // apart from UTC, so :23 or :08/:38/:53) are asserted.
        let time = std::time::UNIX_EPOCH + Duration::from_secs(1_795_152_225);
        let value = format_file_datetime(local_file_datetime(time));
        assert!(is_local_datetime_text(&value), "{value}");
        assert!(!value.contains("1795152225"));
        assert!(["23", "08", "38", "53"].contains(&&value[14..]), "{value}");
    }

    #[cfg(unix)]
    #[test]
    fn file_metadata_shows_local_dates_not_raw_integers() {
        let path =
            std::env::temp_dir().join(format!("herdr-files-dates-{}.txt", std::process::id()));
        std::fs::write(&path, "hello\nworld\n").unwrap();
        let info = collect_file_metadata(&path).unwrap();
        let _ = std::fs::remove_file(&path);
        let modified = info
            .lines
            .iter()
            .find(|line| line.starts_with("更新 "))
            .expect("modified line");
        assert!(
            is_local_datetime_text(&modified["更新 ".len()..]),
            "{modified}"
        );
        let created = info
            .lines
            .iter()
            .find(|line| line.starts_with("作成 "))
            .expect("created line");
        let created_value = &created["作成 ".len()..];
        assert!(created_value == "-" || is_local_datetime_text(created_value));
        for line in &info.lines {
            assert!(
                !line
                    .split(' ')
                    .any(|word| word.len() >= 10 && word.chars().all(|ch| ch.is_ascii_digit())),
                "raw integer in {line}"
            );
        }
    }

    fn frame_text(rows: &[Vec<(String, InfoFramePart)>]) -> Vec<String> {
        rows.iter()
            .map(|parts| parts.iter().map(|(text, _)| text.as_str()).collect())
            .collect()
    }

    fn assert_framed(rows: &[String], width: usize) {
        assert!(rows.len() >= 2);
        for row in rows {
            assert_eq!(UnicodeWidthStr::width(row.as_str()), width, "{row:?}");
        }
        let top = &rows[0];
        assert!(top.starts_with('┏') && top.ends_with('┓'), "{top}");
        let bottom = rows.last().unwrap();
        assert!(bottom.starts_with('┗') && bottom.ends_with('┛'), "{bottom}");
        assert!(bottom[3..bottom.len() - 3].chars().all(|ch| ch == '━'));
        for row in &rows[1..rows.len() - 1] {
            assert!(row.starts_with('┃') && row.ends_with('┃'), "{row}");
        }
    }

    fn frame_title(rows: &[Vec<(String, InfoFramePart)>]) -> Option<String> {
        rows[0]
            .iter()
            .find(|(_, part)| *part == InfoFramePart::Title)
            .map(|(text, _)| text.clone())
    }

    fn body_lines() -> Vec<String> {
        vec![
            "PDF · 553.8 KB".to_string(),
            "作成 2026/11/20 14:23".to_string(),
            "更新 2026/11/20 14:24".to_string(),
        ]
    }

    #[test]
    fn info_frame_keeps_all_corners_and_sides_for_short_and_long_titles() {
        for (title, extension) in [
            ("a.txt", ".txt"),
            ("very-long-ascii-file-name-for-testing.png", ".png"),
            (
                "1222_スマートフォン新機種キャンペーンのご案内チラシ.pdf",
                ".pdf",
            ),
            ("backup-of-the-whole-project-directory.tar.gz", ".tar.gz"),
            (
                "絵文字😀を含むとても長いファイル名のドキュメント.png",
                ".png",
            ),
        ] {
            for width in [24, 25, 30, 40] {
                let rows = info_frame_rows(title, &body_lines(), width);
                assert_framed(&frame_text(&rows), width);
                let shown = frame_title(&rows).expect("title fits");
                assert!(shown.ends_with(extension), "{shown} keeps {extension}");
                if UnicodeWidthStr::width(title) > width - 5 {
                    assert!(shown.contains('…'), "{shown} is middle-elided");
                }
                // Body lines are padded up to the right border.
                let body = frame_text(&rows)[1].clone();
                assert!(body.starts_with("┃PDF · 553.8 KB "));
            }
        }
    }

    #[test]
    fn info_frame_uses_terminal_width_for_wide_characters() {
        let title = "日本語日本語日本語日本語.pdf";
        let rows = info_frame_rows(title, &["全角テキスト全角テキスト全角".to_string()], 21);
        let text = frame_text(&rows);
        assert_framed(&text, 21);
        // Byte length differs from display width for the CJK rows.
        assert_ne!(text[1].len(), 21);
        assert!(frame_title(&rows).unwrap().ends_with(".pdf"));
    }

    #[test]
    fn info_frame_degrades_to_borders_only_and_never_panics_when_narrow() {
        for width in 0..=12 {
            let rows = info_frame_rows("1222_スマートフォン.pdf", &body_lines(), width);
            if width < 2 {
                assert!(rows.is_empty());
                continue;
            }
            assert_framed(&frame_text(&rows), width);
        }
        // Too narrow for any title: just the frame.
        let rows = info_frame_rows("long-name.pdf", &body_lines(), 5);
        assert_eq!(frame_title(&rows), None);
        assert_eq!(frame_text(&rows)[0], "┏━━━┓");
    }

    #[test]
    fn rendered_info_frame_stays_left_of_the_scrollbar_column() {
        let (mut app, pane_id, root, dirs) = render_fixture(20);
        app.files_focus = Some(pane_id);
        let file_path = root.join("1222_スマートフォン新機種キャンペーンのご案内チラシ.pdf");
        {
            let state = app.files_by_pane.get_mut(&pane_id).unwrap();
            let mut entries = dirs.iter().map(|path| dir(path)).collect::<Vec<_>>();
            entries.push(file(&file_path));
            state.directories.insert(root.clone(), entries);
            state.selected_file = Some(file_path.clone());
            state.info = FileInfoState::Ready(FileInfo {
                key: MetadataCacheKey {
                    path: file_path.clone(),
                    size: 1,
                    modified: None,
                },
                title: "1222_スマートフォン新機種キャンペーンのご案内チラシ.pdf".into(),
                lines: vec![
                    "PDF · 1240×1754 px · 3p".to_string(),
                    "553.8 KiB".to_string(),
                    "作成 2026/11/20 14:23".to_string(),
                    "更新 2026/11/20 14:24".to_string(),
                ],
            });
        }
        let (buffer, geometry) = draw(&mut app);
        // The frame fills the reserved details rows under the button row and
        // keeps the section's last column (the tree scrollbar column) free.
        let frame = geometry.details_frame.expect("details frame");
        assert_eq!(
            frame,
            Rect::new(
                geometry.details.x,
                geometry.details.y,
                geometry.details.width - 1,
                6
            )
        );
        assert_eq!(frame.y, geometry.button_row.y + 1);
        let scrollbar_x = geometry.scrollbar.expect("tree overflows").x;
        assert_eq!(scrollbar_x, frame.x + frame.width);
        let row_text = |y: u16| -> String {
            (frame.x..frame.x + frame.width)
                .map(|x| buffer[(x, y)].symbol().to_string())
                .collect()
        };
        let top = row_text(frame.y);
        assert!(top.starts_with('┏') && top.ends_with('┓'), "{top}");
        assert!(top.contains(".pdf"), "{top}");
        let bottom = row_text(frame.y + frame.height - 1);
        assert!(bottom.starts_with('┗') && bottom.ends_with('┛'), "{bottom}");
        for y in frame.y..frame.y + frame.height {
            assert!(
                !"┏┓┃┗┛━".contains(buffer[(scrollbar_x, y)].symbol()),
                "frame leaked into the scrollbar column at y={y}"
            );
        }
        for y in frame.y + 1..frame.y + frame.height - 1 {
            let row = row_text(y);
            assert!(row.starts_with('┃') && row.ends_with('┃'), "{row}");
        }
        // The tree rows are untouched by the frame.
        assert!(geometry.viewport.y + geometry.viewport.height <= geometry.button_row.y);
    }

    #[cfg(unix)]
    #[test]
    fn japanese_pdf_details_are_four_lines_with_px_pages_and_binary_size() {
        let dir = std::env::temp_dir().join(format!("herdr-pdf-details-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("1222_スマートフォン新機種キャンペーンのご案内チラシ.pdf");
        let mut body = b"%PDF-1.7\n1 0 obj\n<< /Type /Catalog /Pages 2 0 R >>\nendobj\n".to_vec();
        body.extend_from_slice(
            b"2 0 obj\n<< /Type /Pages /Kids [3 0 R 4 0 R 5 0 R] /Count 3 >>\nendobj\n",
        );
        body.extend_from_slice(b"3 0 obj\n<< /Type /Page /MediaBox [0 0 930 1315.5] >>\nendobj\n");
        body.extend_from_slice(b"4 0 obj\n<< /Type /Page /MediaBox [0 0 10 10] >>\nendobj\n");
        body.extend_from_slice(b"5 0 obj\n<< /Type /Page /MediaBox [0 0 10 10] >>\nendobj\n");
        body.resize(4096, b' ');
        std::fs::write(&path, &body).unwrap();
        let info = collect_file_metadata(&path).unwrap();
        let _ = std::fs::remove_dir_all(&dir);
        assert_eq!(info.lines.len(), 4);
        assert_eq!(info.lines[0], "PDF · 1240×1754 px · 3p");
        assert_eq!(info.lines[1], "4.0 KiB");
        assert!(info.lines[2].starts_with("作成 "));
        assert!(info.lines[3].starts_with("更新 "));
        let rows = info_frame_rows(&info.title, &info.lines, 24);
        assert_framed(&frame_text(&rows), 24);
        let title = frame_title(&rows).unwrap();
        assert!(title.contains('…') && title.ends_with(".pdf"), "{title}");
    }

    #[cfg(unix)]
    #[test]
    fn unsupported_and_unreadable_files_still_get_common_details() {
        let dir = std::env::temp_dir().join(format!("herdr-common-details-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let archive = dir.join("archive.zip");
        std::fs::write(&archive, vec![0u8; 3000]).unwrap();
        let broken_movie = dir.join("movie.mp4");
        std::fs::write(&broken_movie, b"not really a movie").unwrap();
        let zip = collect_file_metadata(&archive).unwrap();
        let movie = collect_file_metadata(&broken_movie).unwrap();
        assert!(collect_file_metadata(&dir.join("gone.txt")).is_err());
        let _ = std::fs::remove_dir_all(&dir);
        assert_eq!(zip.lines[0], "ZIP");
        assert_eq!(zip.lines[1], "2.9 KiB");
        assert_eq!(movie.lines[0], "MP4");
        assert_eq!(movie.lines[1], "18 B · -");
        for info in [zip, movie] {
            assert_eq!(info.lines.len(), 4);
        }
    }

    fn prefixes(state: &FilesPaneState) -> Vec<(String, String)> {
        state
            .visible_rows()
            .iter()
            .map(|row| (tree_prefix(row), row.name.clone()))
            .collect()
    }

    /// `prefix + name` per row, trailing blanks trimmed: what the tree shows.
    fn tree_text(state: &FilesPaneState) -> Vec<String> {
        state
            .visible_rows()
            .iter()
            .skip(1)
            .map(|row| {
                format!("{}{}", tree_prefix(row), row.name)
                    .trim_end()
                    .to_string()
            })
            .collect()
    }

    fn expanded(state: &mut FilesPaneState, dir: &Path, entries: Vec<FileEntry>) {
        state.focus_dir_explicit(pane(), dir.to_path_buf(), Instant::now());
        complete(state, entries).unwrap();
    }

    /// The example tree from the request: dir2 and dir22 open, dir22 focused
    /// with two files.
    fn example_tree() -> (FilesPaneState, PathBuf) {
        let root = PathBuf::from("/tmp/tree");
        let dir2 = root.join("dir2");
        let dir22 = dir2.join("dir22");
        let mut state = FilesPaneState::reset(Some(root.clone()));
        state.accept_listing(
            &root,
            vec![
                dir(&root.join("dir1")),
                dir(&dir2),
                dir(&root.join("dir3")),
                dir(&root.join("dir4")),
                file(&root.join("root-file.txt")),
            ],
        );
        expanded(
            &mut state,
            &dir2,
            vec![
                dir(&dir2.join("dir21")),
                dir(&dir22),
                dir(&dir2.join("dir23")),
                file(&dir2.join("dir2-file.md")),
            ],
        );
        expanded(
            &mut state,
            &dir22,
            vec![
                file(&dir22.join("file1.png")),
                file(&dir22.join("file2.pdf")),
            ],
        );
        (state, root)
    }

    #[test]
    fn multi_level_tree_matches_the_requested_snapshot() {
        let (state, _) = example_tree();
        // Files follow the folders directly: no blank row inside the tree.
        assert_eq!(
            tree_text(&state),
            vec![
                "├ dir1",
                "├ dir2",
                "│ ├ dir21",
                "│ ├ dir22",
                "│ │   file1.png",
                "│ │   file2.pdf",
                "│ └ dir23",
                "├ dir3",
                "└ dir4",
            ]
        );
        assert!(state.visible_rows().iter().all(|row| !row.name.is_empty()));
    }

    #[test]
    fn only_directories_get_branches_and_the_last_directory_is_a_corner() {
        let root = PathBuf::from("/tmp/tree");
        // One directory followed by files: the directory is still `└`.
        let mut one = FilesPaneState::reset(Some(root.clone()));
        one.accept_listing(
            &root,
            vec![
                dir(&root.join("only")),
                file(&root.join("a.png")),
                file(&root.join("b.pdf")),
            ],
        );
        assert_eq!(
            prefixes(&one),
            vec![
                ("".into(), "tree".into()),
                ("└ ".into(), "only".into()),
                ("  ".into(), "a.png".into()),
                ("  ".into(), "b.pdf".into()),
            ]
        );
        // Three directories then files: ├ ├ └ for the directories only.
        let mut three = FilesPaneState::reset(Some(root.clone()));
        three.accept_listing(
            &root,
            vec![
                dir(&root.join("dir1")),
                dir(&root.join("dir2")),
                dir(&root.join("dir3")),
                file(&root.join("file1.png")),
                file(&root.join("file2.pdf")),
            ],
        );
        let marks = prefixes(&three)
            .into_iter()
            .skip(1)
            .map(|(prefix, _)| prefix)
            .collect::<Vec<_>>();
        assert_eq!(marks, vec!["├ ", "├ ", "└ ", "  ", "  "]);
        // A level with only files has no branch glyphs at all.
        let mut files_only = FilesPaneState::reset(Some(root.clone()));
        files_only.accept_listing(&root, vec![file(&root.join("x")), file(&root.join("y"))]);
        for (prefix, _) in prefixes(&files_only).into_iter().skip(1) {
            assert_eq!(prefix, "  ");
        }
    }

    #[test]
    fn file_rows_keep_only_ancestor_guides_and_align_with_same_depth_directories() {
        let (state, _) = example_tree();
        let rows = state.visible_rows();
        let find = |name: &str| rows.iter().find(|row| row.name == name).unwrap();
        for name in ["file1.png", "file2.pdf"] {
            let row = find(name);
            assert_eq!(row.kind, FileRowKind::File);
            let segments = tree_prefix_segments(row);
            // Own level: blank. Ancestor levels: the dir2 and dir22 guides.
            assert_eq!(segments, vec!["│ ", "│ ", "  "]);
            assert!(!segments.concat().contains(['├', '└']));
            assert!(!row.is_last_directory_sibling);
            assert_eq!(row.parent.as_deref(), Some(find("dir22").path.as_path()));
        }
        // The name column equals that of a directory at the same depth.
        let mut with_subdir = state.clone();
        let dir22 = find("dir22").path.clone();
        with_subdir
            .directories
            .get_mut(&dir22)
            .unwrap()
            .insert(0, dir(&dir22.join("sub")));
        let rows = with_subdir.visible_rows();
        let width =
            |name: &str| tree_prefix_width(rows.iter().find(|row| row.name == name).unwrap());
        assert_eq!(width("file1.png"), width("sub"));
        assert_eq!(width("dir21"), 4);
        assert_eq!(width("file1.png"), 6);
        // The first file directly follows the last subfolder.
        let sub = rows.iter().position(|row| row.name == "sub").unwrap();
        assert_eq!(rows[sub + 1].name, "file1.png");
        assert_eq!(rows[sub + 2].name, "file2.pdf");
    }

    #[test]
    fn guides_stop_where_no_later_directory_exists() {
        let root = PathBuf::from("/tmp/tree");
        let (a, z) = (root.join("a"), root.join("z"));
        let mut state = FilesPaneState::reset(Some(root.clone()));
        state.accept_listing(&root, vec![dir(&a), dir(&z), file(&root.join("top.txt"))]);
        expanded(&mut state, &z, vec![dir(&z.join("deep"))]);
        let deep = z.join("deep");
        expanded(&mut state, &deep, vec![file(&deep.join("memo.txt"))]);
        assert_eq!(
            tree_text(&state),
            vec!["├ a", "└ z", "  └ deep", "      memo.txt"]
        );
        for row in state.visible_rows() {
            if row.kind != FileRowKind::Directory {
                assert!(!tree_prefix(&row).contains(['├', '└', '│']), "{row:?}");
            }
        }
    }

    #[cfg(unix)]
    #[test]
    fn sorted_mixed_listing_marks_its_last_directory_not_its_last_file() {
        let base = std::env::temp_dir().join(format!("herdr-tree-sort-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        std::fs::create_dir_all(base.join("Beta")).unwrap();
        std::fs::create_dir_all(base.join("alpha")).unwrap();
        std::fs::write(base.join(".hidden"), "").unwrap();
        std::fs::write(base.join("zeta.txt"), "").unwrap();
        std::fs::write(base.join("Apple.md"), "").unwrap();
        let entries = list_directory(&base, &base).unwrap();
        let mut state = FilesPaneState::reset(Some(base.clone()));
        state.accept_listing(&base, entries);
        let rows = prefixes(&state);
        let _ = std::fs::remove_dir_all(&base);
        let names = rows
            .iter()
            .skip(1)
            .map(|(_, name)| name.as_str())
            .collect::<Vec<_>>();
        assert_eq!(
            names,
            vec!["alpha", "Beta", ".hidden", "Apple.md", "zeta.txt"]
        );
        let marks = rows
            .iter()
            .skip(1)
            .map(|(prefix, _)| prefix.as_str())
            .collect::<Vec<_>>();
        assert_eq!(marks, vec!["├ ", "└ ", "  ", "  ", "  "]);
    }

    #[test]
    fn only_the_focused_directory_shows_its_files() {
        let (state, root) = example_tree();
        let rows = state.visible_rows();
        let files = rows
            .iter()
            .filter(|row| row.kind == FileRowKind::File)
            .map(|row| row.name.as_str())
            .collect::<Vec<_>>();
        // Not the root's or dir2's (ancestors) files, only dir22's.
        assert_eq!(files, vec!["file1.png", "file2.pdf"]);
        assert!(
            state.directories.contains_key(&root),
            "ancestor listing is cached"
        );
        assert!(state
            .directories
            .get(&root.join("dir2"))
            .unwrap()
            .iter()
            .any(|entry| entry.name == "dir2-file.md"));
        // dir22 has no subfolders: its first file is the very next row.
        let dir22 = rows.iter().position(|row| row.name == "dir22").unwrap();
        assert_eq!(rows[dir22 + 1].name, "file1.png");
    }

    #[test]
    fn moving_focus_hides_old_files_selection_and_details_until_the_new_listing() {
        let (mut state, root) = example_tree();
        let dir22 = root.join("dir2/dir22");
        state.selected_file = Some(dir22.join("file1.png"));
        state.info = FileInfoState::Loading;
        let generation = state.metadata_generation;
        assert!(state.accepts_metadata(generation, &dir22.join("file1.png")));

        let dir23 = root.join("dir2/dir23");
        assert!(state.focus_dir_explicit(pane(), dir23.clone(), Instant::now()));
        assert!(state
            .visible_rows()
            .iter()
            .all(|row| row.kind == FileRowKind::Directory));
        assert_eq!(state.selected_file, None);
        assert!(matches!(state.info, FileInfoState::Hidden));
        assert!(!state.accepts_metadata(generation, &dir22.join("file1.png")));
        assert_eq!(state.expanded_dir.as_ref(), Some(&root.join("dir2")));
        let mut stale = state.pending_expand.clone().unwrap();
        stale.target = dir22.clone();
        stale.in_flight = true;
        assert!(state
            .apply_listing(&stale, Ok(vec![file(&dir22.join("x"))]))
            .is_err());
        complete(&mut state, vec![file(&dir23.join("new.txt"))]).unwrap();
        let files = state
            .visible_rows()
            .into_iter()
            .filter(|row| row.kind == FileRowKind::File)
            .map(|row| row.path)
            .collect::<Vec<_>>();
        assert_eq!(files, vec![dir23.join("new.txt")]);
    }

    #[test]
    fn refocusing_an_open_ancestor_waits_for_its_listing_before_showing_files() {
        let (mut state, root) = example_tree();
        let dir2 = root.join("dir2");
        state.focus_dir_explicit(pane(), dir2.clone(), Instant::now());
        assert_eq!(state.expanded_dir.as_ref(), Some(&dir2));
        assert!(!state
            .visible_rows()
            .iter()
            .any(|row| row.kind == FileRowKind::File));
        let cached = state.directories.get(&dir2).unwrap().clone();
        complete(&mut state, cached).unwrap();
        let files = state
            .visible_rows()
            .into_iter()
            .filter(|row| row.kind == FileRowKind::File)
            .map(|row| row.name)
            .collect::<Vec<_>>();
        assert_eq!(files, vec!["dir2-file.md"]);
    }

    #[test]
    fn gap_row_sits_between_tree_and_buttons_and_is_dropped_first_when_short() {
        // Normal height: header / tree / gap(1) / buttons / details(6).
        let area = Rect::new(2, 10, 25, 20);
        let layout = files_section_layout(area);
        assert_eq!(layout.header, Rect::new(2, 10, 25, 1));
        assert_eq!(layout.tree, Rect::new(2, 11, 25, 11));
        assert_eq!(layout.gap, Rect::new(2, 22, 25, 1));
        assert_eq!(layout.button_row, Rect::new(2, 23, 25, 1));
        assert_eq!(layout.details, Rect::new(2, 24, 25, 6));
        // Exactly the minimum with a gap: 1 tree row, the gap survives.
        let layout = files_section_layout(Rect::new(0, 0, 20, 10));
        assert_eq!((layout.tree.height, layout.gap.height), (1, 1));
        // One row shorter: the gap goes before the tree row or the details.
        let layout = files_section_layout(Rect::new(0, 0, 20, 9));
        assert_eq!(
            (layout.tree.height, layout.gap.height, layout.details.height),
            (1, 0, 6)
        );
        assert_eq!(layout.button_row.y, layout.tree.y + 1);
        // Any height, never panics, never overlaps, stays inside the area.
        for height in 0..40 {
            for width in [0, 1, 5, 25] {
                let area = Rect::new(0, 3, width, height);
                let layout = files_section_layout(area);
                if layout == FilesSectionLayout::default() {
                    continue;
                }
                let parts = [
                    layout.header,
                    layout.tree,
                    layout.gap,
                    layout.button_row,
                    layout.details,
                ];
                assert_eq!(parts.iter().map(|rect| rect.height).sum::<u16>(), height);
                for pair in parts.windows(2) {
                    assert_eq!(pair[0].y + pair[0].height, pair[1].y);
                }
                assert!(layout.tree.height >= 1 && layout.button_row.height == 1);
            }
        }
    }

    #[test]
    fn rendered_gap_is_blank_outside_the_tree_and_keyboard_moves_skip_it() {
        let (mut app, pane_id, _root, _dirs) = render_fixture(20);
        app.files_focus = Some(pane_id);
        let (buffer, geometry) = draw(&mut app);
        let gap = geometry.gap;
        assert_eq!(gap.height, 1);
        assert_eq!(gap.y, geometry.viewport.y + geometry.viewport.height);
        assert_eq!(geometry.button_row.y, gap.y + 1);
        for x in gap.x..gap.x + gap.width {
            assert_eq!(buffer[(x, gap.y)].symbol(), " ", "x={x}");
        }
        let bar = geometry.scrollbar.expect("20 dirs overflow");
        assert_eq!(bar.y + bar.height, gap.y);
        assert!(!geometry.rows.iter().any(|hit| hit.row.y == gap.y));
        // Scrolling never moves the gap or the buttons.
        app.files_by_pane.get_mut(&pane_id).unwrap().scroll = 5;
        let (_, scrolled) = draw(&mut app);
        assert_eq!(
            (scrolled.gap, scrolled.button_row),
            (gap, geometry.button_row)
        );
        // Keyboard targets are directories, all of them real rows.
        let state = &app.files_by_pane[&pane_id];
        assert_eq!(
            state.focusable_directories().len(),
            state.visible_rows().len()
        );
    }

    #[test]
    fn resized_layout_keeps_render_and_hitboxes_in_step() {
        let (mut app, pane_id, _root, _dirs) = render_fixture(20);
        app.files_focus = Some(pane_id);
        for (width, height) in [(106, 60), (106, 36), (80, 28), (106, 60)] {
            let view = Rect::new(0, 0, width, height);
            crate::ui::compute_view(&mut app, view);
            let geometry = app.view.files_geometry.clone().expect("files geometry");
            let layout = files_section_layout(geometry.area);
            assert_eq!(geometry.gap, layout.gap, "{view:?}");
            assert_eq!(geometry.button_row, layout.button_row);
            assert_eq!(geometry.viewport.height, layout.tree.height);
            let mut terminal =
                ratatui::Terminal::new(ratatui::backend::TestBackend::new(width, height)).unwrap();
            terminal
                .draw(|frame| render_files(&app, frame, geometry.area))
                .unwrap();
            let buffer = terminal.backend().buffer();
            // The drawn buttons start where their hitboxes are.
            let first = geometry.buttons[0];
            assert_eq!(buffer[(first.x, first.y)].symbol(), "[");
            for hit in &geometry.rows {
                assert!(hit.row.y < geometry.gap.y.max(geometry.button_row.y));
            }
            if geometry.gap.height == 1 {
                let gap = geometry.gap;
                assert!((gap.x..gap.x + gap.width).all(|x| buffer[(x, gap.y)].symbol() == " "));
            }
        }
    }

    #[test]
    fn glyphs_follow_structure_not_the_viewport_and_survive_elision() {
        let (mut app, pane_id, root, dirs) = render_fixture(20);
        let long = root.join("d03");
        {
            let state = app.files_by_pane.get_mut(&pane_id).unwrap();
            state.focus_dir_explicit(pane_id, long.clone(), Instant::now());
            complete(
                state,
                vec![
                    dir(&long.join("サブフォルダ")),
                    file(&long.join("とても長い日本語のファイル名を持つ資料のサンプル.pdf")),
                    file(&long.join("archive-of-the-whole-project.tar.gz")),
                ],
            )
            .unwrap();
        }
        let snapshot = |app: &mut AppState| {
            let (buffer, geometry) = draw(app);
            geometry
                .rows
                .iter()
                .map(|hit| {
                    let text = (hit.row.x..hit.name.x)
                        .map(|x| buffer[(x, hit.row.y)].symbol().to_string())
                        .collect::<String>();
                    (hit.path.clone(), text, hit.clone())
                })
                .collect::<Vec<_>>()
        };
        let before = snapshot(&mut app);
        app.files_by_pane.get_mut(&pane_id).unwrap().scroll = 2;
        let after = snapshot(&mut app);
        for (path, prefix, _) in &after {
            if let Some((_, earlier, _)) = before.iter().find(|(p, _, _)| p == path) {
                assert_eq!(prefix, earlier, "{path:?}");
            }
        }
        let (_, bottom_prefix, bottom) = after.last().unwrap();
        if let Some(index) = dirs.iter().position(|path| *path == bottom.path) {
            if index + 1 < dirs.len() {
                assert_eq!(bottom_prefix, "├ ");
            }
        }
        let find = |name: &str| {
            after
                .iter()
                .find(|(path, _, _)| path.to_string_lossy().contains(name))
                .unwrap_or_else(|| panic!("{name} rendered"))
                .clone()
        };
        let (_, sub_prefix, sub) = find("サブフォルダ");
        assert_eq!(sub_prefix, "│ └ ");
        let (_, _, pdf) = find("とても長い");
        // No blank row between the last folder and the first file.
        assert_eq!(pdf.row.y, sub.row.y + 1);
        for name in ["とても長い", "archive-of"] {
            let (_, prefix, hit) = find(name);
            assert_eq!(prefix, "│   ", "files keep the ancestor guide only");
            assert_eq!(
                hit.name.x, sub.name.x,
                "file names align with the directory"
            );
            assert!(hit.name.x + hit.name.width <= hit.row.x + hit.row.width);
        }
        let (buffer, _) = draw(&mut app);
        let pdf_name = (pdf.name.x..pdf.name.x + pdf.name.width)
            .map(|x| buffer[(x, pdf.row.y)].symbol().to_string())
            .collect::<String>();
        assert!(
            pdf_name.contains('…') && pdf_name.ends_with(".pdf"),
            "{pdf_name}"
        );
        let (_, _, tar) = find("archive-of");
        assert_eq!(tar.row.y, pdf.row.y + 1, "no blank row between files");
        let tar_name = (tar.name.x..tar.name.x + tar.name.width)
            .map(|x| buffer[(x, tar.row.y)].symbol().to_string())
            .collect::<String>();
        assert!(tar_name.ends_with(".tar.gz"), "{tar_name}");
    }

    fn tones_of(state: &FilesPaneState, name: &str) -> FilesRowTones {
        let rows = state.visible_rows();
        let row = rows
            .iter()
            .find(|row| row.name == name)
            .unwrap_or_else(|| panic!("{name}"));
        files_row_tones(
            row,
            state.expanded_dir.as_deref(),
            state.selected_file.as_deref(),
        )
    }

    #[test]
    fn tones_mark_the_expanded_directory_its_subtree_and_the_selected_file() {
        let (mut state, root) = example_tree();
        let dir22 = root.join("dir2/dir22");
        state.selected_file = Some(dir22.join("file2.pdf"));
        use FilesTone::{Descendant, Expanded, Normal, SelectedFile};
        let tones = tones_of(&state, "dir22");
        assert_eq!(tones.name, Expanded);
        assert_eq!(tones.segments, vec![Normal, Expanded]);
        // An unselected file inside the expanded folder: neutral name, only
        // its hierarchy cells inside dir22 are blue.
        let tones = tones_of(&state, "file1.png");
        assert_eq!(tones.name, Normal);
        assert_eq!(tones.segments, vec![Normal, Normal, Descendant]);
        // The selected file name is lime; its cells never are.
        let tones = tones_of(&state, "file2.pdf");
        assert_eq!(tones.name, SelectedFile);
        assert_eq!(tones.segments, vec![Normal, Normal, Descendant]);
        for name in ["tree", "dir2", "dir1", "dir21", "dir23", "dir3", "dir4"] {
            let tones = tones_of(&state, name);
            assert_eq!(tones.name, Normal, "{name}");
            assert!(tones.segments.iter().all(|tone| *tone == Normal), "{name}");
        }
    }

    #[test]
    fn deeper_descendants_color_every_level_inside_the_expanded_directory() {
        let root = PathBuf::from("/tmp/tree");
        let a = root.join("a");
        let row = |path: PathBuf, kind: FileRowKind, depth: u16| FileRow {
            parent: path.parent().map(Path::to_path_buf),
            is_dir: kind == FileRowKind::Directory,
            path,
            name: "x".into(),
            kind,
            depth,
            is_last_directory_sibling: false,
            ancestor_has_later_directory_sibling: vec![true; usize::from(depth) - 1],
        };
        use FilesTone::{Descendant, Normal};
        let deep = row(a.join("b/c"), FileRowKind::Directory, 3);
        let tones = files_row_tones(&deep, Some(&a), None);
        assert_eq!(tones.name, Descendant);
        assert_eq!(tones.segments, vec![Normal, Descendant, Descendant]);
        let deep_file = row(a.join("b/c.txt"), FileRowKind::File, 3);
        let tones = files_row_tones(&deep_file, Some(&a), None);
        assert_eq!(tones.name, Normal, "file names are never blue");
        assert_eq!(tones.segments, vec![Normal, Descendant, Descendant]);
        // Component-wise ancestry: `/tmp/tree/ab` is not inside `/tmp/tree/a`.
        let lookalike = row(root.join("ab"), FileRowKind::Directory, 1);
        let tones = files_row_tones(&lookalike, Some(&a), None);
        assert_eq!(tones.name, Normal);
        assert_eq!(tones.segments, vec![Normal]);
    }

    fn all_palettes() -> Vec<crate::app::state::Palette> {
        use crate::app::state::Palette;
        vec![
            Palette::catppuccin(),
            Palette::catppuccin_latte(),
            Palette::tokyo_night(),
            Palette::tokyo_night_day(),
            Palette::dracula(),
            Palette::nord(),
            Palette::gruvbox(),
            Palette::gruvbox_light(),
            Palette::one_dark(),
            Palette::one_light(),
            Palette::solarized(),
            Palette::solarized_light(),
            Palette::kanagawa(),
            Palette::rose_pine(),
            Palette::vesper(),
            Palette::terminal(),
        ]
    }

    fn contrast_of(fg: ratatui::style::Color, bg: ratatui::style::Color) -> Option<f32> {
        Some(rgb_contrast(
            crate::ui::panes::color_to_rgb(fg)?,
            crate::ui::panes::color_to_rgb(bg)?,
        ))
    }

    #[test]
    fn theme_colors_separate_blue_hierarchy_from_lime_selection_and_buttons() {
        for p in all_palettes() {
            let bg = p.panel_bg;
            let expanded = files_tone_color(&p, FilesTone::Expanded, bg);
            let descendant = files_tone_color(&p, FilesTone::Descendant, bg);
            let selected = files_selected_file_style(&p, true);
            let selected_idle = files_selected_file_style(&p, false);
            let button = files_button_style(&p, true, false);
            let pressed = files_button_style(&p, true, true);
            let disabled = files_button_style(&p, false, false);
            let border = files_details_border_color(&p);
            assert_ne!(expanded, descendant, "{p:?}");
            // Selection vs buttons: related hue, different role.
            assert_ne!(selected.fg, button.fg, "{p:?}");
            assert!(
                selected.bg.is_some(),
                "selected name has a token background"
            );
            assert_eq!(button.bg, None, "resting buttons have no background");
            assert!(pressed.bg.is_some(), "pressed buttons glow");
            assert_ne!(pressed.fg, button.fg, "{p:?}");
            assert_ne!(disabled.fg, button.fg, "{p:?}");
            assert_eq!(disabled.bg, None);
            assert_ne!(border, selected.fg.unwrap(), "{p:?}");
            assert_ne!(
                selected.bg, selected_idle.bg,
                "PTY focus dims the selection"
            );
            for (fg, on, what) in [
                (expanded, bg, "expanded"),
                (descendant, bg, "descendant"),
                (selected.fg.unwrap(), selected.bg.unwrap(), "selected"),
                (
                    selected_idle.fg.unwrap(),
                    selected_idle.bg.unwrap(),
                    "selected idle",
                ),
                (button.fg.unwrap(), bg, "button"),
                (pressed.fg.unwrap(), pressed.bg.unwrap(), "pressed"),
                (border, bg, "details border"),
            ] {
                if let Some(ratio) = contrast_of(fg, on) {
                    assert!(ratio >= FILES_MIN_CONTRAST, "{what} {ratio} {p:?}");
                }
            }
            if let (Some(e), Some(b)) = (
                crate::ui::panes::color_to_rgb(expanded),
                crate::ui::panes::color_to_rgb(descendant),
            ) {
                assert!(e.2 >= e.0 && b.2 >= b.0, "blue family {e:?} {b:?}");
            }
        }
    }

    #[test]
    fn lime_selection_leans_green_and_buttons_lean_yellow_in_rgb_themes() {
        let p = crate::app::state::Palette::catppuccin();
        let selected = rgb(files_selected_file_style(&p, true).fg.unwrap());
        let button = rgb(files_button_style(&p, true, false).fg.unwrap());
        let pressed = rgb(files_button_style(&p, true, true).fg.unwrap());
        let disabled = rgb(files_button_style(&p, false, false).fg.unwrap());
        // Green dominates both; the button carries more red (yellow-ward).
        assert!(selected.1 > selected.2 && button.1 > button.2);
        assert!(
            button.0 > selected.0,
            "button {button:?} selected {selected:?}"
        );
        let lum = |c| crate::ui::panes::relative_luminance(c);
        assert!(lum(pressed) > lum(button), "pressed is brighter");
        assert!(lum(disabled) < lum(button), "disabled is dimmer");
        // Disabled keeps the hue but loses saturation.
        let saturation = |(r, g, b): (u8, u8, u8)| {
            let (max, min) = (r.max(g).max(b), r.min(g).min(b));
            f32::from(max - min) / f32::from(max.max(1))
        };
        assert!(saturation(disabled) < saturation(button));
        // The details border is a darker lime than the selected name.
        let border = rgb(files_details_border_color(&p));
        assert!(lum(border) < lum(selected));
        assert!(border.1 > border.2);
    }

    #[test]
    fn rendered_tree_paints_blue_hierarchy_neutral_files_and_a_lime_token() {
        let (mut app, pane_id, root, _dirs) = render_fixture(3);
        let d01 = root.join("d01");
        let sub = d01.join("sub");
        {
            let state = app.files_by_pane.get_mut(&pane_id).unwrap();
            state.focus_dir_explicit(pane_id, d01.clone(), Instant::now());
            complete(
                state,
                vec![
                    dir(&sub),
                    file(&d01.join("a.txt")),
                    file(&d01.join("b.txt")),
                ],
            )
            .unwrap();
            state.selected_file = Some(d01.join("b.txt"));
        }
        app.files_focus = Some(pane_id);
        let (buffer, geometry) = draw(&mut app);
        let p = app.palette.clone();
        let selected = files_selected_file_style(&p, true);
        let descendant = files_tone_color(&p, FilesTone::Descendant, p.panel_bg);
        let hit = |path: &Path| {
            geometry
                .rows
                .iter()
                .find(|hit| hit.path == path)
                .unwrap()
                .clone()
        };
        // Selected file: lime fg + lime background on the name token only.
        let b = hit(&d01.join("b.txt"));
        for x in b.row.x..b.row.x + b.row.width {
            let in_name = x >= b.name.x && x < b.name.x + b.name.width;
            let cell = &buffer[(x, b.row.y)];
            assert_eq!(cell.bg == selected.bg.unwrap(), in_name, "x={x}");
            assert_eq!(cell.fg == selected.fg.unwrap(), in_name, "x={x}");
        }
        // Its own hierarchy guide (the `│` from d01's level is blank here;
        // d01's children level is inside the expanded folder) is never lime.
        let guide = &buffer[(b.row.x, b.row.y)];
        assert_ne!(guide.fg, selected.fg.unwrap());
        // Unselected file: neutral, not blue.
        let a = hit(&d01.join("a.txt"));
        let fg = buffer[(a.name.x, a.name.y)].fg;
        assert_eq!(fg, p.overlay1);
        assert_ne!(fg, descendant);
        // Subfolder inside the expanded folder and its `└`: paler blue.
        let s = hit(&sub);
        assert_eq!(buffer[(s.name.x, s.name.y)].fg, descendant);
        assert_eq!(buffer[(s.name.x - 2, s.name.y)].symbol(), "└");
        assert_eq!(buffer[(s.name.x - 2, s.name.y)].fg, descendant);
        // Sibling folders (other branches) and the root ancestor stay neutral.
        let d00 = hit(&root.join("d00"));
        assert_eq!(buffer[(d00.name.x, d00.name.y)].fg, p.text);
        assert_eq!(buffer[(d00.name.x - 2, d00.name.y)].fg, p.overlay0);
        // The focused (expanded) folder keeps the focus-row background with a
        // readable name.
        let row = row_of(&geometry, &d01);
        let focus_bg = files_focus_bg(&p, true);
        assert!(row_bgs(&buffer, row).iter().all(|bg| *bg == focus_bg));
        let name_fg = buffer[(row.x + 2, row.y)].fg;
        if let Some(ratio) = contrast_of(name_fg, focus_bg) {
            assert!(ratio >= FILES_MIN_CONTRAST);
        }
        // PTY focus: the selection stays visible, slightly dimmer.
        app.files_focus = None;
        let (buffer, _) = draw(&mut app);
        let idle = files_selected_file_style(&p, false);
        assert_eq!(buffer[(b.name.x, b.name.y)].bg, idle.bg.unwrap());
        assert_ne!(idle.bg, selected.bg);
    }

    #[test]
    fn details_frame_uses_a_darker_lime_border_and_a_neutral_body() {
        let (mut app, pane_id, root, _dirs) = render_fixture(3);
        let path = root.join("memo.txt");
        {
            let state = app.files_by_pane.get_mut(&pane_id).unwrap();
            let mut entries = state.directories[&root].clone();
            entries.push(file(&path));
            state.directories.insert(root.clone(), entries);
            state.selected_file = Some(path.clone());
            state.info = FileInfoState::Ready(FileInfo {
                key: MetadataCacheKey {
                    path: path.clone(),
                    size: 1,
                    modified: None,
                },
                title: "memo.txt".into(),
                lines: vec!["TXT".into(), "1 B".into(), "作成 -".into(), "更新 -".into()],
            });
        }
        let (buffer, geometry) = draw(&mut app);
        let frame = geometry.details_frame.expect("frame");
        let p = app.palette.clone();
        let border = files_details_border_color(&p);
        assert_eq!(buffer[(frame.x, frame.y)].symbol(), "┏");
        assert_eq!(buffer[(frame.x, frame.y)].fg, border);
        assert_eq!(buffer[(frame.x, frame.y + 1)].fg, border);
        assert_eq!(
            buffer[(frame.x + 1, frame.y + 1)].fg,
            p.overlay1,
            "neutral body"
        );
        assert_ne!(buffer[(frame.x + 1, frame.y + 1)].bg, selected_bg(&p));
    }

    fn selected_bg(p: &crate::app::state::Palette) -> ratatui::style::Color {
        files_selected_file_style(p, true).bg.unwrap()
    }

    #[test]
    fn button_enablement_follows_the_focused_directory() {
        let (mut state, root) = example_tree();
        assert_eq!(files_buttons_enabled(&state), [true, true, true, true]);
        state.focused_dir = Some(root.clone());
        assert_eq!(files_buttons_enabled(&state), [false, true, true, true]);
        state.focused_dir = Some(root.join("dir4"));
        assert_eq!(files_buttons_enabled(&state), [true, false, true, true]);
        let empty = FilesPaneState::default();
        assert_eq!(files_buttons_enabled(&empty), [false, false, false, false]);
    }

    #[test]
    fn rendered_buttons_show_the_pressed_button_only_and_never_a_disabled_one() {
        let (mut app, pane_id, _root, _dirs) = render_fixture(3);
        let now = Instant::now();
        let p = app.palette.clone();
        let pressed_bg = files_button_style(&p, true, true).bg.unwrap();
        let normal = files_button_style(&p, true, false).fg.unwrap();
        app.files_button_press = Some(FilesButtonPress::new(pane_id, 2, true, now));
        let (buffer, geometry) = draw(&mut app);
        for (index, rect) in geometry.buttons.iter().enumerate() {
            for x in rect.x..rect.x + rect.width {
                assert_eq!(
                    buffer[(x, rect.y)].bg == pressed_bg,
                    index == 2,
                    "button {index} x={x}"
                );
            }
        }
        // Brackets included: the whole `[down]` label shares one color.
        let down = geometry.buttons[1];
        for x in down.x..down.x + down.width {
            assert_eq!(buffer[(x, down.y)].fg, normal);
        }
        // Root is focused, so `[↑]` is disabled: even a press does not light it.
        app.files_button_press = Some(FilesButtonPress::new(pane_id, 0, true, now));
        let (buffer, geometry) = draw(&mut app);
        let up = geometry.buttons[0];
        assert_ne!(buffer[(up.x, up.y)].bg, pressed_bg);
        assert_eq!(
            buffer[(up.x, up.y)].fg,
            files_button_style(&p, false, false).fg.unwrap()
        );
        // Press bookkeeping: held never expires; released expires after the
        // minimum visible time.
        let mut press = FilesButtonPress::new(pane_id, 0, true, now);
        assert!(!press.expired(now + Duration::from_secs(5)));
        press.held = false;
        assert!(!press.expired(now + Duration::from_millis(100)));
        assert!(press.expired(now + FILES_BUTTON_PRESS_MIN));
        assert_eq!(FILES_BUTTON_PRESS_MIN, Duration::from_millis(140));
    }

    #[cfg(unix)]
    #[test]
    fn symlinks_leaving_the_root_are_never_followed_for_details() {
        let base = std::env::temp_dir().join(format!("herdr-files-link-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        let (root, outside) = (base.join("root"), base.join("outside"));
        std::fs::create_dir_all(&root).unwrap();
        std::fs::create_dir_all(&outside).unwrap();
        std::fs::write(outside.join("secret.txt"), "a\nb\nc\n").unwrap();
        std::fs::write(root.join("inside.txt"), "x\n").unwrap();
        std::os::unix::fs::symlink(outside.join("secret.txt"), root.join("escape.txt")).unwrap();
        std::os::unix::fs::symlink(root.join("inside.txt"), root.join("alias.txt")).unwrap();
        std::os::unix::fs::symlink(base.join("missing"), root.join("dangling.txt")).unwrap();
        let escape = collect_file_metadata_within(&root.join("escape.txt"), Some(&root)).unwrap();
        let alias = collect_file_metadata_within(&root.join("alias.txt"), Some(&root)).unwrap();
        let dangling =
            collect_file_metadata_within(&root.join("dangling.txt"), Some(&root)).unwrap();
        let no_root = collect_file_metadata_within(&root.join("escape.txt"), None).unwrap();
        let _ = std::fs::remove_dir_all(&base);
        for info in [&escape, &dangling, &no_root] {
            assert_eq!(info.lines, vec!["LINK", "-", "作成 -", "更新 -"]);
        }
        // A link that stays inside the tree gets the normal details.
        assert!(alias.lines[0].starts_with("TEXT"), "{:?}", alias.lines);
    }

    #[cfg(unix)]
    #[test]
    fn unusual_file_names_and_vanished_files_never_panic() {
        let base = std::env::temp_dir().join(format!("herdr-files-names-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        std::fs::create_dir_all(&base).unwrap();
        for name in [
            "空白 を 含む 名前.pdf",
            "改行\nを含む.txt",
            "絵文字😀.png",
            " leading space.mp4",
            "no-extension",
            ".hidden.wav",
        ] {
            let path = base.join(name);
            std::fs::write(&path, b"not really this format").unwrap();
            let info = collect_file_metadata_within(&path, Some(&base)).unwrap();
            assert_eq!(info.lines.len(), 4, "{name:?}");
            let _ = elide_filename(name, 7);
        }
        // Gone before the task ran: an error result, not a panic.
        assert!(collect_file_metadata_within(&base.join("gone.pdf"), Some(&base)).is_err());
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn compound_extension_and_unicode_elision_are_width_safe() {
        let value = elide_filename("very-long-name.tar.gz", 12);
        assert!(value.ends_with(".tar.gz"));
        assert!(unicode_width::UnicodeWidthStr::width(value.as_str()) <= 12);
        assert!(elide_filename("非常に長い名前", 5).contains('…'));
    }

    #[test]
    fn metadata_key_changes_when_file_size_changes() {
        let path =
            std::env::temp_dir().join(format!("herdr-files-metadata-{}", std::process::id()));
        std::fs::write(&path, "a").unwrap();
        let first = metadata_key(&path).unwrap();
        std::fs::write(&path, "larger").unwrap();
        let second = metadata_key(&path).unwrap();
        assert_ne!(first, second);
        let _ = std::fs::remove_file(path);
    }

    #[cfg(unix)]
    #[test]
    fn directory_listing_rejects_root_escape_and_keeps_hidden_files() {
        let base = std::env::temp_dir().join(format!("herdr-files-list-{}", std::process::id()));
        let root = base.join("root");
        let outside = base.join("outside");
        std::fs::create_dir_all(&root).unwrap();
        std::fs::create_dir_all(&outside).unwrap();
        std::fs::write(root.join(".hidden"), "x").unwrap();
        std::os::unix::fs::symlink(&outside, root.join("escape")).unwrap();
        let entries = list_directory(&root, &root).unwrap();
        assert!(entries.iter().any(|entry| entry.name == ".hidden"));
        assert!(list_directory(&root, &root.join("escape")).is_err());
        let _ = std::fs::remove_dir_all(base);
    }
}

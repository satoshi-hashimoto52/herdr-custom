use ratatui::{
    layout::{Constraint, Layout, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, Clear, Paragraph},
    Frame,
};

use super::text::display_width_u16;
use super::widgets::panel_contrast_fg;
use crate::{
    app::state::{CopyFeedback, Palette, ToastKind, ToastNotification},
    config::{StatusIndicatorStyle, ToastClipboardPosition, ToastHerdrPosition},
    detect::AgentState,
};

pub(crate) fn copy_feedback_rect(
    area: Rect,
    feedback: &CopyFeedback,
    offset_rows: u16,
    position: ToastClipboardPosition,
) -> Rect {
    if area.width == 0 || area.height == 0 {
        return Rect::default();
    }

    let content_width = feedback.message.len() as u16 + 4;
    let width = content_width.min(area.width);
    let height = 3u16.min(area.height);
    let x = match position {
        ToastClipboardPosition::TopLeft | ToastClipboardPosition::BottomLeft => area.x,
        ToastClipboardPosition::TopCenter | ToastClipboardPosition::BottomCenter => {
            area.x + area.width.saturating_sub(width) / 2
        }
        ToastClipboardPosition::TopRight | ToastClipboardPosition::BottomRight => {
            area.x + area.width.saturating_sub(width)
        }
    };
    let y = match position {
        ToastClipboardPosition::TopLeft
        | ToastClipboardPosition::TopCenter
        | ToastClipboardPosition::TopRight => area.y + offset_rows.min(area.height),
        ToastClipboardPosition::BottomLeft
        | ToastClipboardPosition::BottomCenter
        | ToastClipboardPosition::BottomRight => {
            area.y + area.height.saturating_sub(height + offset_rows)
        }
    };
    Rect::new(x, y, width, height)
}

pub(crate) fn toast_notification_rect(
    area: Rect,
    toast: &ToastNotification,
    offset_for_warning: bool,
    position: ToastHerdrPosition,
) -> Rect {
    let content_width = display_width_u16(&toast.title)
        .max(display_width_u16(&toast.context))
        .saturating_add(4);
    let width = content_width.saturating_add(2).min(area.width);
    let content_height = if toast.context.is_empty() { 1 } else { 2 };
    let height = (content_height + 2).min(area.height);
    let x = match position {
        ToastHerdrPosition::TopLeft | ToastHerdrPosition::BottomLeft => area.x,
        ToastHerdrPosition::TopRight | ToastHerdrPosition::BottomRight => {
            area.x + area.width.saturating_sub(width)
        }
    };
    let warning_offset = u16::from(offset_for_warning);
    let y = match position {
        ToastHerdrPosition::TopLeft | ToastHerdrPosition::TopRight => {
            area.y + warning_offset.min(area.height)
        }
        ToastHerdrPosition::BottomLeft | ToastHerdrPosition::BottomRight => {
            area.y + area.height.saturating_sub(height + warning_offset)
        }
    };
    Rect::new(x, y, width, height)
}

pub(super) fn render_toast_notification(
    frame: &mut Frame,
    area: Rect,
    toast: &ToastNotification,
    offset_for_warning: bool,
    position: ToastHerdrPosition,
    p: &Palette,
) {
    let dot_color = match toast.kind {
        ToastKind::NeedsAttention => p.red,
        ToastKind::Finished => p.blue,
        ToastKind::UpdateInstalled => p.accent,
    };
    let toast_area = toast_notification_rect(area, toast, offset_for_warning, position);

    frame.render_widget(Clear, toast_area);
    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(p.overlay0))
        .style(Style::default().bg(p.panel_bg));
    let inner = block.inner(toast_area);
    frame.render_widget(block, toast_area);

    if inner.height < 1 {
        return;
    }

    let [title_row, context_row] =
        Layout::vertical([Constraint::Length(1), Constraint::Length(1)]).areas(inner);

    let title = Line::from(vec![
        Span::styled("●", Style::default().fg(dot_color)),
        Span::raw(" "),
        Span::styled(
            &toast.title,
            Style::default().fg(p.text).add_modifier(Modifier::BOLD),
        ),
    ]);
    let context = Line::from(vec![
        Span::styled("  ", Style::default().fg(p.overlay0)),
        Span::styled(&toast.context, Style::default().fg(p.overlay0)),
    ]);

    frame.render_widget(Paragraph::new(title), title_row);
    if !toast.context.is_empty() && inner.height >= 2 {
        frame.render_widget(Paragraph::new(context), context_row);
    }
}

pub(super) fn render_copy_feedback(
    frame: &mut Frame,
    area: Rect,
    feedback: &CopyFeedback,
    offset_rows: u16,
    position: ToastClipboardPosition,
    p: &Palette,
) {
    let feedback_area = copy_feedback_rect(area, feedback, offset_rows, position);
    if feedback_area.is_empty() {
        return;
    }

    frame.render_widget(Clear, feedback_area);
    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(p.green))
        .style(Style::default().bg(p.panel_bg));
    let inner = block.inner(feedback_area);
    frame.render_widget(block, feedback_area);

    if inner.height == 0 {
        return;
    }

    let text = Line::from(vec![
        Span::styled("●", Style::default().fg(p.green).bg(p.panel_bg)),
        Span::raw(" "),
        Span::styled(
            &feedback.message,
            Style::default()
                .fg(p.text)
                .bg(p.panel_bg)
                .add_modifier(Modifier::BOLD),
        ),
    ]);
    frame.render_widget(Paragraph::new(text), inner);
}

pub(super) fn render_config_diagnostic(frame: &mut Frame, area: Rect, message: &str, p: &Palette) {
    let style = Style::default()
        .fg(panel_contrast_fg(p))
        .bg(p.yellow)
        .add_modifier(Modifier::BOLD);

    for (row, line) in message
        .lines()
        .filter(|line| !line.trim().is_empty())
        .take(area.height as usize)
        .enumerate()
    {
        let text = format!(" {line} ");
        let width = (text.len() as u16).min(area.width);
        let notif_area = Rect::new(
            area.x + area.width.saturating_sub(width),
            area.y + row as u16,
            width,
            1,
        );

        frame.render_widget(Clear, notif_area);
        frame.render_widget(Paragraph::new(Span::styled(text, style)), notif_area);
    }
}

pub(super) fn state_icon_symbol(
    state: AgentState,
    seen: bool,
    indicator_style: StatusIndicatorStyle,
) -> &'static str {
    match (indicator_style, state, seen) {
        (StatusIndicatorStyle::Dots, AgentState::Blocked, _) => "●",
        (StatusIndicatorStyle::Dots, AgentState::Working, _) => "●",
        (StatusIndicatorStyle::Dots, AgentState::Idle, false) => "●",
        (StatusIndicatorStyle::Dots, AgentState::Idle, true) => "○",
        (StatusIndicatorStyle::Dots, AgentState::Unknown, _) => "·",
        (StatusIndicatorStyle::Symbols, AgentState::Blocked, _) => "×",
        (StatusIndicatorStyle::Symbols, AgentState::Working, _) => "◐",
        (StatusIndicatorStyle::Symbols, AgentState::Idle, false) => "✓",
        (StatusIndicatorStyle::Symbols, AgentState::Idle, true) => "○",
        (StatusIndicatorStyle::Symbols, AgentState::Unknown, _) => "·",
    }
}

pub(super) fn state_icon(
    state: AgentState,
    seen: bool,
    indicator_style: StatusIndicatorStyle,
    p: &Palette,
) -> (&'static str, Style) {
    (
        state_icon_symbol(state, seen, indicator_style),
        Style::default().fg(state_label_color(state, seen, p)),
    )
}

/// Marker for an agent whose process died mid-turn.
///
/// Width one like every other marker, so an error never reflows a row, and
/// visually unlike the dots and check so it reads as wrong at a glance.
const ERROR_ICON: &str = "!";

/// Label for the five states a user reasons about.
///
/// Herdr detects `Working` and `Blocked`; the sidebar keeps `working` as the
/// word for the first and names the second for what the user is waiting on.
/// This is the displayed label only — [`pane_status_key`] still reports the
/// state names the socket API and per-pane overrides are keyed by.
pub(super) fn state_label(state: AgentState, seen: bool) -> &'static str {
    match (state, seen) {
        (AgentState::Blocked, _) => "waiting",
        (AgentState::Working, _) => "working",
        (AgentState::Idle, false) => "done",
        (AgentState::Idle, true) => "idle",
        (AgentState::Unknown, _) => "idle",
    }
}

/// Pane-level label, including the `error` case that has no `AgentState` of
/// its own because it comes from a recorded process exit rather than detection.
pub(super) fn pane_state_label(state: AgentState, seen: bool, errored: bool) -> &'static str {
    if errored {
        "error"
    } else {
        state_label(state, seen)
    }
}

/// Status key used to look up a per-pane label override reported over the API.
pub(super) fn pane_status_key(state: AgentState, seen: bool, errored: bool) -> &'static str {
    if errored {
        "error"
    } else {
        match (state, seen) {
            (AgentState::Idle, false) => "done",
            (AgentState::Idle, true) => "idle",
            (AgentState::Working, _) => "working",
            (AgentState::Blocked, _) => "blocked",
            (AgentState::Unknown, _) => "unknown",
        }
    }
}

/// Pane-level marker, styled so only `error` raises its voice.
pub(super) fn pane_state_icon(
    state: AgentState,
    seen: bool,
    errored: bool,
    indicator_style: StatusIndicatorStyle,
    p: &Palette,
) -> (&'static str, Style) {
    if errored {
        return (
            ERROR_ICON,
            Style::default()
                .fg(vivid(p.red, p, BLOCKED_LIFT))
                .add_modifier(Modifier::BOLD),
        );
    }
    state_icon(state, seen, indicator_style, p)
}

/// Pane-level label color, keeping `error` as the only emphasized state.
pub(super) fn pane_state_label_color(
    state: AgentState,
    seen: bool,
    errored: bool,
    p: &Palette,
) -> Color {
    if errored {
        vivid(p.red, p, BLOCKED_LIFT)
    } else {
        state_label_color(state, seen, p)
    }
}

pub(super) fn state_label_color(state: AgentState, seen: bool, p: &Palette) -> Color {
    match (state, seen) {
        (AgentState::Blocked, _) => vivid(p.red, p, BLOCKED_LIFT),
        (AgentState::Working, _) => vivid(p.yellow, p, WORKING_LIFT),
        (AgentState::Idle, false) => vivid(p.teal, p, DONE_LIFT),
        (AgentState::Idle, true) => vivid(p.green, p, IDLE_LIFT),
        (AgentState::Unknown, _) => vivid(p.overlay0, p, UNKNOWN_LIFT),
    }
}

/// Accent for the tab name — the word that says which project a row belongs to.
///
/// A fixed warm orange rather than a lift of the palette's own peach: peach as
/// the pastel themes ship it lands close enough to skin tone that it stops
/// reading as orange at label size, and this one accent is meant to be found
/// without hunting. Light palettes and palettes built from ANSI color names
/// keep `peach`, for the same reasons the state colors do.
pub(super) fn project_label_color(p: &Palette) -> Color {
    if matches!(p.peach, Color::Rgb(..)) && palette_is_dark(p) {
        Color::Rgb(PROJECT_ACCENT.0, PROJECT_ACCENT.1, PROJECT_ACCENT.2)
    } else {
        p.peach
    }
}

/// Target saturation and lightness for a lifted color, both 0..=1.
///
/// Saturation is a floor rather than a setting, so a theme that ships a more
/// vivid color than this keeps it; lightness is pinned, since it is what the
/// contrast against a dark sidebar rests on.
type Lift = (f32, f32);

/// Attention states run hot and bright; `idle` is lifted least, so a sidebar
/// full of finished agents stays quiet and the one that needs a person does
/// not.
const BLOCKED_LIFT: Lift = (1.0, 0.68);
const WORKING_LIFT: Lift = (1.0, 0.66);
const DONE_LIFT: Lift = (1.0, 0.62);
const IDLE_LIFT: Lift = (0.75, 0.70);
const UNKNOWN_LIFT: Lift = (0.18, 0.63);

/// The tab-name accent, held apart from the state hues so a project name and a
/// state label never read as the same color on neighbouring rows.
const PROJECT_ACCENT: super::panes::Rgb = (255, 179, 117);

/// Push a color toward the fluorescent end of its own hue.
///
/// The hue never moves, so a theme keeps its identity and the states keep the
/// meanings their colors already carry. Two cases are left alone entirely: a
/// light palette, where the same lift would put pale text on a pale
/// background, and a palette built from ANSI color names rather than RGB,
/// where the whole point is that the host terminal picks the color.
fn vivid(color: Color, p: &Palette, lift: Lift) -> Color {
    let Color::Rgb(r, g, b) = color else {
        return color;
    };
    if !palette_is_dark(p) {
        return color;
    }
    let (hue, saturation, _) = rgb_to_hsl((r, g, b));
    let (r, g, b) = hsl_to_rgb(hue, saturation.max(lift.0), lift.1);
    Color::Rgb(r, g, b)
}

fn palette_is_dark(p: &Palette) -> bool {
    super::panes::color_to_rgb(panel_contrast_fg(p))
        .map(|rgb| super::panes::relative_luminance(rgb) < 0.5)
        .unwrap_or(true)
}

/// Hue in degrees, saturation and lightness in 0..=1.
fn rgb_to_hsl(color: super::panes::Rgb) -> (f32, f32, f32) {
    let (r, g, b) = (
        f32::from(color.0) / 255.0,
        f32::from(color.1) / 255.0,
        f32::from(color.2) / 255.0,
    );
    let max = r.max(g).max(b);
    let min = r.min(g).min(b);
    let lightness = (max + min) / 2.0;
    let span = max - min;
    if span <= f32::EPSILON {
        return (0.0, 0.0, lightness);
    }
    let saturation = span / (1.0 - (2.0 * lightness - 1.0).abs());
    let hue = if max == r {
        60.0 * (((g - b) / span) % 6.0)
    } else if max == g {
        60.0 * ((b - r) / span + 2.0)
    } else {
        60.0 * ((r - g) / span + 4.0)
    };
    ((hue + 360.0) % 360.0, saturation.clamp(0.0, 1.0), lightness)
}

fn hsl_to_rgb(hue: f32, saturation: f32, lightness: f32) -> super::panes::Rgb {
    let chroma = (1.0 - (2.0 * lightness - 1.0).abs()) * saturation;
    let sector = hue / 60.0;
    let second = chroma * (1.0 - (sector % 2.0 - 1.0).abs());
    let (r, g, b) = match sector as u32 {
        0 => (chroma, second, 0.0),
        1 => (second, chroma, 0.0),
        2 => (0.0, chroma, second),
        3 => (0.0, second, chroma),
        4 => (second, 0.0, chroma),
        _ => (chroma, 0.0, second),
    };
    let base = lightness - chroma / 2.0;
    let channel = |value: f32| ((value + base) * 255.0).round().clamp(0.0, 255.0) as u8;
    (channel(r), channel(g), channel(b))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{ToastClipboardPosition, ToastHerdrPosition};

    fn toast() -> ToastNotification {
        ToastNotification {
            kind: ToastKind::Finished,
            title: "done".to_string(),
            context: "workspace".to_string(),
            position: None,
            target: None,
        }
    }

    fn feedback() -> CopyFeedback {
        CopyFeedback {
            message: "copied to clipboard".to_string(),
        }
    }

    #[test]
    fn state_icons_support_dot_and_distinct_symbol_styles() {
        let palette = Palette::catppuccin();
        for (indicator_style, expected_symbols) in [
            (StatusIndicatorStyle::Dots, ["●", "●", "●", "○", "·"]),
            (StatusIndicatorStyle::Symbols, ["×", "◐", "✓", "○", "·"]),
        ] {
            for ((state, seen), expected_symbol) in [
                (AgentState::Blocked, true),
                (AgentState::Working, true),
                (AgentState::Idle, false),
                (AgentState::Idle, true),
                (AgentState::Unknown, true),
            ]
            .into_iter()
            .zip(expected_symbols)
            {
                let (actual_symbol, style) = state_icon(state, seen, indicator_style, &palette);
                assert_eq!(actual_symbol, expected_symbol);
                assert_eq!(display_width_u16(actual_symbol), 1);
                assert_eq!(style.fg, Some(state_label_color(state, seen, &palette)));
            }
        }
    }

    #[test]
    fn toast_rect_uses_configured_corner() {
        let area = Rect::new(10, 20, 100, 40);
        let toast = toast();

        let top_left = toast_notification_rect(area, &toast, false, ToastHerdrPosition::TopLeft);
        assert_eq!(top_left.x, area.x);
        assert_eq!(top_left.y, area.y);

        let top_right = toast_notification_rect(area, &toast, false, ToastHerdrPosition::TopRight);
        assert_eq!(top_right.x + top_right.width, area.x + area.width);
        assert_eq!(top_right.y, area.y);

        let bottom_left =
            toast_notification_rect(area, &toast, false, ToastHerdrPosition::BottomLeft);
        assert_eq!(bottom_left.x, area.x);
        assert_eq!(bottom_left.y + bottom_left.height, area.y + area.height);

        let bottom_right =
            toast_notification_rect(area, &toast, false, ToastHerdrPosition::BottomRight);
        assert_eq!(bottom_right.x + bottom_right.width, area.x + area.width);
        assert_eq!(bottom_right.y + bottom_right.height, area.y + area.height);
    }

    #[test]
    fn toast_rect_uses_display_width_for_cjk_labels() {
        let area = Rect::new(0, 0, 100, 20);
        let toast = ToastNotification {
            kind: ToastKind::NeedsAttention,
            title: "重构用户认证模块".to_string(),
            context: "提交 herdr 的反馈".to_string(),
            position: None,
            target: None,
        };

        let rect = toast_notification_rect(area, &toast, false, ToastHerdrPosition::TopRight);

        let expected_content_width =
            display_width_u16(&toast.title).max(display_width_u16(&toast.context)) + 6;
        assert_eq!(rect.width, expected_content_width);
        assert_eq!(rect.x + rect.width, area.x + area.width);
    }

    #[test]
    fn copy_feedback_rect_uses_configured_position() {
        let area = Rect::new(10, 20, 100, 40);
        let feedback = feedback();

        let top_center = copy_feedback_rect(area, &feedback, 0, ToastClipboardPosition::TopCenter);
        assert_eq!(top_center.y, area.y);
        assert_eq!(
            top_center.x,
            area.x + area.width.saturating_sub(top_center.width) / 2
        );

        let bottom_center =
            copy_feedback_rect(area, &feedback, 0, ToastClipboardPosition::BottomCenter);
        assert_eq!(bottom_center.y + bottom_center.height, area.y + area.height);
        assert_eq!(
            bottom_center.x,
            area.x + area.width.saturating_sub(bottom_center.width) / 2
        );
    }

    #[test]
    fn state_labels_name_the_five_states_a_user_reasons_about() {
        let cases = [
            (AgentState::Working, true, false, "working"),
            (AgentState::Blocked, true, false, "waiting"),
            (AgentState::Idle, false, false, "done"),
            (AgentState::Idle, true, false, "idle"),
            (AgentState::Unknown, true, false, "idle"),
            // A recorded mid-turn exit outranks whatever detection last saw.
            (AgentState::Working, true, true, "error"),
            (AgentState::Idle, true, true, "error"),
        ];

        for (state, seen, errored, expected) in cases {
            assert_eq!(pane_state_label(state, seen, errored), expected);
        }
    }

    fn hue_of(color: Color) -> f32 {
        let Color::Rgb(r, g, b) = color else {
            panic!("expected an rgb color");
        };
        rgb_to_hsl((r, g, b)).0
    }

    fn saturation_of(color: Color) -> f32 {
        let Color::Rgb(r, g, b) = color else {
            panic!("expected an rgb color");
        };
        rgb_to_hsl((r, g, b)).1
    }

    #[test]
    fn state_colors_are_lifted_without_moving_off_their_hue() {
        let palette = Palette::catppuccin();
        let cases = [
            (AgentState::Blocked, true, palette.red),
            (AgentState::Working, true, palette.yellow),
            (AgentState::Idle, false, palette.teal),
            (AgentState::Idle, true, palette.green),
            (AgentState::Unknown, true, palette.overlay0),
        ];

        for (state, seen, source) in cases {
            let lifted = state_label_color(state, seen, &palette);
            assert!(
                (hue_of(lifted) - hue_of(source)).abs() < 1.0,
                "{state:?} changed hue: {source:?} -> {lifted:?}"
            );
            assert!(
                saturation_of(lifted) >= saturation_of(source),
                "{state:?} lost saturation: {source:?} -> {lifted:?}"
            );
        }

        // `idle` is the one state deliberately left quieter than the rest, so a
        // sidebar full of finished agents does not shout.
        let idle = saturation_of(state_label_color(AgentState::Idle, true, &palette));
        let blocked = saturation_of(state_label_color(AgentState::Blocked, true, &palette));
        assert!(idle < blocked);
    }

    #[test]
    fn lifted_state_colors_stay_readable_on_a_dark_sidebar() {
        let palette = Palette::catppuccin();
        // The sidebar inherits the terminal background; this is the darkest
        // ground the lifted colors are expected to sit on.
        let background = super::super::panes::relative_luminance((40, 44, 52));
        for (state, seen) in [
            (AgentState::Blocked, true),
            (AgentState::Working, true),
            (AgentState::Idle, false),
            (AgentState::Idle, true),
            (AgentState::Unknown, true),
        ] {
            let Color::Rgb(r, g, b) = state_label_color(state, seen, &palette) else {
                panic!("expected an rgb color");
            };
            let foreground = super::super::panes::relative_luminance((r, g, b));
            let contrast =
                (foreground.max(background) + 0.05) / (foreground.min(background) + 0.05);
            assert!(contrast >= 4.5, "{state:?} fell to {contrast:.2}:1");
        }
    }

    #[test]
    fn light_and_terminal_palettes_keep_their_own_state_colors() {
        let latte = Palette::catppuccin_latte();
        assert_eq!(
            state_label_color(AgentState::Working, true, &latte),
            latte.yellow
        );
        assert_eq!(project_label_color(&latte), latte.peach);

        // Palettes built from ANSI names leave the choice to the host terminal.
        let terminal = Palette::terminal();
        assert_eq!(
            state_label_color(AgentState::Working, true, &terminal),
            terminal.yellow
        );
    }

    #[test]
    fn the_project_accent_reads_as_orange() {
        let palette = Palette::catppuccin();
        let orange = project_label_color(&palette);
        assert_eq!(orange, Color::Rgb(255, 179, 117));
        let hue = hue_of(orange);
        assert!((15.0..45.0).contains(&hue), "hue {hue} is not orange");
        assert!(saturation_of(orange) > 0.9);
        // Far enough from the `working` label that the two never read as one
        // color on neighbouring rows.
        let working = hue_of(state_label_color(AgentState::Working, true, &palette));
        assert!((working - hue).abs() > 10.0);
    }

    #[test]
    fn error_status_key_is_distinct_so_overrides_can_target_it() {
        assert_eq!(pane_status_key(AgentState::Working, true, false), "working");
        assert_eq!(pane_status_key(AgentState::Blocked, true, false), "blocked");
        assert_eq!(pane_status_key(AgentState::Idle, false, false), "done");
        assert_eq!(pane_status_key(AgentState::Idle, true, false), "idle");
        assert_eq!(pane_status_key(AgentState::Unknown, true, false), "unknown");
        assert_eq!(pane_status_key(AgentState::Working, true, true), "error");
    }

    #[test]
    fn error_marker_stays_one_cell_wide_and_visually_emphasized() {
        let palette = Palette::catppuccin();

        let (symbol, style) = pane_state_icon(
            AgentState::Working,
            true,
            true,
            StatusIndicatorStyle::Dots,
            &palette,
        );

        let errored_red = pane_state_label_color(AgentState::Idle, true, true, &palette);
        assert_eq!(display_width_u16(symbol), 1);
        assert_eq!(style.fg, Some(errored_red));
        assert!(style.add_modifier.contains(Modifier::BOLD));
        assert_eq!(
            errored_red,
            state_label_color(AgentState::Blocked, true, &palette)
        );
    }
}

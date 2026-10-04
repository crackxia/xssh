//! Small presentation helpers shared by the pages.

use crate::i18n::{t, tf};
use gpui_kit::base::StyledExt as _;
use gpui_kit::component::notification::Notification;
use gpui_kit::component::scroll::Scrollbar;
use gpui_kit::component::{ActiveTheme as _, WindowExt as _, h_flex, v_flex};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::{
    AnyElement, App, Div, ElementId, InteractiveElement as _, IntoElement, ListHorizontalSizingBehavior, ParentElement, Pixels,
    SharedString, Styled, UniformList, UniformListScrollHandle, Window, div, px, uniform_list,
};
use std::rc::Rc;
use xssh_core::Error;

/// "3 秒" / "5 分" / "2 小时 10 分" / "3 天 4 小时" (English: "3 s" / "5 min" / "2 h 10 min" / "3 d 4 h").
pub fn human_secs(s: u64) -> String {
    match s {
        0..60 => tf!("{s} 秒", "{s} s"),
        60..3600 => tf!("{} 分", "{} min", s / 60),
        3600..86400 => tf!("{} 小时 {} 分", "{} h {} min", s / 3600, s % 3600 / 60),
        _ => tf!("{} 天 {} 小时", "{} d {} h", s / 86400, s % 86400 / 3600),
    }
}

/// `2026-09-28T18:33:05.123+08:00` -> `09-28 18:33:05`.
pub fn short_time(ts: &str) -> String {
    match (ts.get(5..10), ts.get(11..19)) {
        (Some(d), Some(t)) => format!("{d} {t}"),
        _ => ts.to_string(),
    }
}

pub fn error_text(e: &Error) -> String {
    match &e.hint {
        Some(h) => format!("{}\n{h}", e.message),
        None => e.message.clone(),
    }
}

pub fn notify_error(window: &mut Window, cx: &mut App, title: &str, e: &Error) {
    window.push_notification(Notification::error(error_text(e)).title(title.to_string()), cx);
}

pub fn notify_ok(window: &mut Window, cx: &mut App, msg: impl Into<SharedString>) {
    window.push_notification(Notification::success(msg), cx);
}

/// Page title row: title, a muted subtitle, and actions on the right. The subtitle never wraps:
/// keep it to one short sentence; a narrow window ends it with "…".
pub fn page_header(title: &str, subtitle: impl Into<SharedString>, actions: impl IntoElement, cx: &App) -> impl IntoElement {
    h_flex()
        .w_full()
        .justify_between()
        .items_center()
        .gap_4()
        .child(
            v_flex()
                .flex_1()
                .min_w_0()
                .gap_1()
                .child(div().text_xl().font_semibold().child(title.to_string()))
                .child(
                    div()
                        .text_sm()
                        .truncate()
                        .text_color(cx.theme().muted_foreground)
                        .child(subtitle.into()),
                ),
        )
        .child(h_flex().flex_none().gap_2().child(actions))
}

/// Centered muted text for empty tables and panels.
pub fn empty(text: impl Into<SharedString>, cx: &App) -> impl IntoElement {
    div()
        .w_full()
        .py_10()
        .flex()
        .justify_center()
        .text_sm()
        .text_color(cx.theme().muted_foreground)
        .child(text.into())
}

/// An empty state that says what is missing and how it gets filled: a title and a muted hint
/// (skipped when empty).
pub fn empty_state(title: impl Into<SharedString>, hint: impl Into<SharedString>, cx: &App) -> Div {
    let hint: SharedString = hint.into();
    v_flex()
        .w_full()
        .items_center()
        .gap_1p5()
        .pt_10()
        .px_6()
        .child(div().text_sm().font_medium().child(title.into()))
        .when(!hint.is_empty(), |this| {
            this.child(
                div()
                    .max_w(px(520.))
                    .text_xs()
                    .text_center()
                    .text_color(cx.theme().muted_foreground)
                    .child(hint),
            )
        })
}

/// Multi-line text (scripts, heredocs) on one line: lines joined by " ↵ ", blank lines dropped.
pub fn one_line(s: &str) -> String {
    s.lines().map(str::trim).filter(|l| !l.is_empty()).collect::<Vec<_>>().join(" ↵ ")
}

/// Height of one monospace line in `mono_view`.
const MONO_ROW: Pixels = px(18.);

/// Virtualized monospace text (terminal screens, logs): only the visible lines are laid out, so
/// transcripts with many thousands of lines scroll smoothly. Lines keep their spacing, do not
/// wrap, and scroll horizontally when wider than the view.
pub fn mono_view(id: impl Into<ElementId>, lines: Rc<Vec<String>>, handle: &UniformListScrollHandle, cx: &App) -> impl IntoElement {
    let widest = lines.iter().enumerate().max_by_key(|(_, l)| l.chars().count()).map(|(i, _)| i);
    let font = cx.theme().mono_font_family.clone();
    let count = lines.len();
    virtual_list(
        uniform_list(id, count, move |range, _, _| {
            range
                .map(|i| {
                    let l = &lines[i];
                    div()
                        .h(MONO_ROW)
                        .whitespace_nowrap()
                        .child(if l.is_empty() { " ".to_string() } else { l.clone() })
                })
                .collect()
        })
        .font_family(font)
        .text_xs()
        .with_horizontal_sizing_behavior(ListHorizontalSizingBehavior::Unconstrained)
        .with_width_from_item(widest),
        handle,
        true,
    )
}

/// A `uniform_list` filling its container, with overlay scrollbars on the same handle.
pub fn virtual_list(list: UniformList, handle: &UniformListScrollHandle, horizontal: bool) -> impl IntoElement {
    div()
        .relative()
        .size_full()
        .child(list.track_scroll(handle).size_full())
        .child(
            div()
                .absolute()
                .top_0()
                .right_0()
                .bottom_0()
                .w(Scrollbar::width())
                .child(Scrollbar::vertical(handle).viewport_from_layout()),
        )
        .when(horizontal, |this| {
            this.child(
                div()
                    .absolute()
                    .left_0()
                    .right_0()
                    .bottom_0()
                    .h(Scrollbar::width())
                    .child(Scrollbar::horizontal(handle).viewport_from_layout()),
            )
        })
}

/// Scrolled to (near) the end, or nothing to scroll yet: new output should keep it there.
pub fn at_end(handle: &UniformListScrollHandle) -> bool {
    handle.is_scrolled_to_end().unwrap_or(true)
}

/// A column of a virtualized table: fixed width, or the remaining space when `width` is None.
/// The title is a (Chinese, English) pair.
#[derive(Clone, Copy)]
pub struct Col {
    pub title: (&'static str, &'static str),
    pub width: Option<f32>,
    pub right: bool,
}

pub const fn col(zh: &'static str, en: &'static str, width: f32) -> Col {
    Col {
        title: (zh, en),
        width: Some(width),
        right: false,
    }
}

pub const fn col_flex(zh: &'static str, en: &'static str) -> Col {
    Col {
        title: (zh, en),
        width: None,
        right: false,
    }
}

pub const fn col_right(zh: &'static str, en: &'static str, width: f32) -> Col {
    Col {
        title: (zh, en),
        width: Some(width),
        right: true,
    }
}

fn cell(c: Col, child: AnyElement) -> Div {
    let d = h_flex().h_full().px_2().overflow_hidden().when(c.right, |this| this.justify_end());
    match c.width {
        Some(w) => d.w(px(w)).flex_none(),
        None => d.flex_1().min_w_0(),
    }
    .child(child)
}

/// A table: a rounded, bordered frame with a fixed header and a virtualized body. Rows scroll
/// under the header and are clipped by the frame, so a long table ends at a visible edge instead
/// of rows being cut off in the middle of the page.
pub fn table(cols: &[Col], list: UniformList, handle: &UniformListScrollHandle, cx: &App) -> impl IntoElement {
    frame(cols, virtual_list(list, handle, false).into_any_element(), cx)
}

/// The same frame and header with an `empty_state` instead of rows (no data, no match, errors).
pub fn table_empty(cols: &[Col], title: impl Into<SharedString>, hint: impl Into<SharedString>, cx: &App) -> impl IntoElement {
    frame(cols, empty_state(title, hint, cx).into_any_element(), cx)
}

fn frame(cols: &[Col], body: AnyElement, cx: &App) -> impl IntoElement {
    let radius = cx.theme().radius_lg;
    let mut header = h_flex()
        .w_full()
        .h(px(38.))
        .flex_none()
        .px_2()
        .text_xs()
        .font_medium()
        .bg(cx.theme().table_head)
        .text_color(cx.theme().muted_foreground)
        .border_b_1()
        .border_color(cx.theme().border)
        .children(cols.iter().map(|c| cell(*c, t(c.title.0, c.title.1).into_any_element())));
    // Only the top corners follow the frame (1px inside its border).
    let inner = gpui_kit::AbsoluteLength::Pixels((radius - px(1.)).max(px(0.)));
    header.style().corner_radii.top_left = Some(inner);
    header.style().corner_radii.top_right = Some(inner);
    v_flex()
        .size_full()
        .overflow_hidden()
        .rounded(radius)
        .border_1()
        .border_color(cx.theme().border)
        .bg(cx.theme().background)
        .child(header)
        .child(div().flex_1().min_h_0().child(body))
}

/// One fixed-height row of a `table` (`cells` in column order).
pub fn table_row(cols: &[Col], cells: Vec<AnyElement>, height: Pixels, cx: &App) -> Div {
    h_flex()
        .w_full()
        .h(height)
        .px_2()
        .text_sm()
        .border_b_1()
        .border_color(cx.theme().table_row_border)
        .hover(|this| this.bg(cx.theme().table_hover))
        .children(cols.iter().zip(cells).map(|(c, e)| cell(*c, e)))
}

/// Single-line text that ends with "…" when it does not fit.
pub fn clip(text: impl Into<SharedString>) -> AnyElement {
    div().min_w_0().truncate().child(text.into()).into_any_element()
}

//! Audit log: what agents ran, where, and how it ended. Virtualized, so thousands of records
//! scroll smoothly.

use crate::backend::Backend;
use crate::ui::{self, Col, col, col_flex, col_right};
use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::input::{Input, InputEvent, InputState};
use gpui_kit::component::tag::Tag;
use gpui_kit::component::{ActiveTheme as _, IconName, Sizable as _, WindowExt as _, h_flex, v_flex};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::{
    AnyElement, App, AppContext as _, Context, Entity, InteractiveElement as _, IntoElement, ParentElement as _, Render,
    StatefulInteractiveElement as _, Styled as _, Subscription, UniformListScrollHandle, Window, div, px, uniform_list,
};
use std::rc::Rc;
use std::sync::Arc;
use xssh_store::audit::AuditRecord;

const LIMIT: usize = 5000;
const COLS: [Col; 6] = [
    col("时间", 130.),
    col("动作", 120.),
    col("主机", 110.),
    col_flex("内容"),
    col("结果", 90.),
    col_right("耗时", 80.),
];

pub struct AuditPage {
    backend: Arc<Backend>,
    records: Rc<Vec<AuditRecord>>,
    /// Indices into `records` matching the filter.
    shown: Rc<Vec<usize>>,
    search: Entity<InputState>,
    scroll: UniformListScrollHandle,
    _subs: Vec<Subscription>,
}

impl AuditPage {
    pub fn new(backend: Arc<Backend>, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let search = cx.new(|cx| InputState::new(window, cx).placeholder("筛选主机、动作或命令"));
        let sub = cx.subscribe(&search, |this, _, e: &InputEvent, cx| {
            if matches!(e, InputEvent::Change) {
                this.filter(cx);
            }
        });
        AuditPage {
            backend,
            records: Rc::default(),
            shown: Rc::default(),
            search,
            scroll: UniformListScrollHandle::new(),
            _subs: vec![sub],
        }
    }

    /// The log can be ~20 MB of JSON lines: parse it off the UI thread.
    pub fn reload(&mut self, cx: &mut Context<Self>) {
        let backend = self.backend.clone();
        let task = cx.background_spawn(async move { backend.audit(LIMIT) });
        cx.spawn(async move |this, cx| {
            let records = task.await;
            let _ = this.update(cx, |this, cx| {
                this.records = Rc::new(records);
                this.filter(cx);
            });
        })
        .detach();
    }

    fn filter(&mut self, cx: &mut Context<Self>) {
        let q = self.search.read(cx).value().to_lowercase();
        let hit = |s: Option<&str>| s.is_some_and(|s| s.to_lowercase().contains(&q));
        self.shown = Rc::new(
            (0..self.records.len())
                .filter(|&i| {
                    let r = &self.records[i];
                    q.is_empty() || hit(Some(&r.action)) || hit(r.host.as_deref()) || hit(r.command.as_deref()) || hit(r.target.as_deref())
                })
                .collect(),
        );
        cx.notify();
    }
}

fn took(r: &AuditRecord) -> String {
    r.duration_ms
        .map(|d| {
            if d >= 1000 {
                format!("{:.1} s", d as f64 / 1000.0)
            } else {
                format!("{d} ms")
            }
        })
        .unwrap_or_default()
}

/// Success is the normal case: plain muted text, so failures and non-zero exits stand out.
fn result(r: &AuditRecord, cx: &App) -> AnyElement {
    match (r.ok, r.exit_code) {
        (false, _) => Tag::danger().outline().xsmall().child("失败").into_any_element(),
        (true, Some(c)) if c != 0 => Tag::warning().outline().xsmall().child(format!("退出 {c}")).into_any_element(),
        _ => div().text_xs().text_color(cx.theme().muted_foreground).child("成功").into_any_element(),
    }
}

/// Everything recorded for one operation, with the full (multi-line) command.
fn show_detail(r: &AuditRecord, window: &mut Window, cx: &mut App) {
    let mut fields: Vec<(&str, String)> = vec![("时间", r.ts.clone()), ("动作", r.action.clone())];
    fields.extend(r.host.clone().map(|h| ("主机", h)));
    fields.extend(r.target.clone().map(|t| ("目标", t)));
    fields.extend(r.exit_code.map(|c| ("退出码", c.to_string())));
    fields.extend(Some(took(r)).filter(|t| !t.is_empty()).map(|t| ("耗时", t)));
    fields.extend(r.error.clone().map(|e| ("错误", e)));
    fields.extend(r.client_pid.map(|p| ("调用进程", p.to_string())));
    fields.extend(r.client_cwd.clone().map(|c| ("调用目录", c)));
    let lines: Rc<Vec<String>> = Rc::new(r.command.as_deref().unwrap_or("").lines().map(String::from).collect());
    let ok = r.ok && r.exit_code.unwrap_or(0) == 0;
    let title = format!("{} {}", r.action, if ok { "成功" } else { "未成功" });
    let scroll = UniformListScrollHandle::new();
    window.open_dialog(cx, move |dialog, _, cx| {
        let muted = cx.theme().muted_foreground;
        let list = v_flex().gap_1().children(fields.iter().map(|(k, v)| {
            h_flex()
                .gap_4()
                .items_start()
                .text_sm()
                .child(div().w(px(72.)).flex_none().text_color(muted).child(k.to_string()))
                .child(div().min_w_0().flex_1().child(v.clone()))
        }));
        dialog.title(title.clone()).w(px(760.)).child(
            v_flex().gap_3().child(list).when(!lines.is_empty(), |this| {
                this.child(div().text_sm().text_color(muted).child("命令")).child(
                    div()
                        .h(px((lines.len().min(16) as f32) * 18. + 24.))
                        .p_3()
                        .rounded(cx.theme().radius)
                        .bg(cx.theme().muted)
                        .child(ui::mono_view("audit-cmd", lines.clone(), &scroll, cx)),
                )
            }),
        )
    });
}

fn render_row(ix: usize, r: &AuditRecord, cx: &App) -> AnyElement {
    let what = r.command.clone().or(r.target.clone()).or(r.error.clone()).unwrap_or_default();
    let rec = r.clone();
    ui::table_row(
        &COLS,
        vec![
            ui::clip(ui::short_time(&r.ts)),
            ui::clip(r.action.clone()),
            ui::clip(r.host.clone().unwrap_or_default()),
            div()
                .min_w_0()
                .truncate()
                .font_family(cx.theme().mono_font_family.clone())
                .text_xs()
                .child(ui::one_line(&what))
                .into_any_element(),
            result(r, cx),
            ui::clip(took(r)),
        ],
        px(36.),
        cx,
    )
    .id(("audit-row", ix))
    .cursor_pointer()
    .on_click(move |_, window, cx| show_detail(&rec, window, cx))
    .into_any_element()
}

impl Render for AuditPage {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let actions = h_flex()
            .gap_2()
            .child(div().w(px(260.)).child(Input::new(&self.search).cleanable(true)))
            .child(
                Button::new("audit-refresh")
                    .ghost()
                    .icon(IconName::RefreshCw)
                    .on_click(cx.listener(|this, _, _, cx| this.reload(cx))),
            );
        let body = if self.shown.is_empty() {
            let (title, hint) = if self.records.is_empty() {
                ("还没有记录", "agent 通过 xssh 在远端执行的命令、传输和文件修改都会记录在这里。")
            } else {
                ("没有匹配的记录", "筛选范围包括主机、动作、命令和目标路径。")
            };
            ui::table_empty(&COLS, title, hint, cx)
            .into_any_element()
        } else {
            let (records, shown) = (self.records.clone(), self.shown.clone());
            ui::table(
                &COLS,
                uniform_list("audit-rows", shown.len(), move |range, _, cx| {
                    range.map(|i| render_row(i, &records[shown[i]], cx)).collect()
                }),
                &self.scroll,
                cx,
            )
            .into_any_element()
        };
        v_flex()
            .size_full()
            .p_6()
            .gap_4()
            .child(ui::page_header(
                "审计日志",
                format!("最近 {} 条远程操作（最新在前），来自 audit.jsonl。点击一行查看完整命令。", self.records.len()),
                actions,
                cx,
            ))
            .child(div().flex_1().min_h_0().child(body))
    }
}

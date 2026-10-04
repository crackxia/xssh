//! Audit log: what agents ran, where, and how it ended. Virtualized, so thousands of records
//! scroll smoothly.

use crate::backend::Backend;
use crate::ui::{self, Col, col, col_flex, col_right};
use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::input::{Input, InputEvent, InputState};
use gpui_kit::component::tag::Tag;
use gpui_kit::component::{ActiveTheme as _, IconName, Sizable as _, h_flex, v_flex};
use gpui_kit::{
    AnyElement, App, AppContext as _, Context, Entity, IntoElement, ParentElement as _, Render, Styled as _, Subscription,
    UniformListScrollHandle, Window, div, px, uniform_list,
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

fn render_row(r: &AuditRecord, cx: &App) -> AnyElement {
    let what = r.command.clone().or(r.target.clone()).or(r.error.clone()).unwrap_or_default();
    let result = match (r.ok, r.exit_code) {
        (false, _) => Tag::danger().child("失败"),
        (true, Some(0)) | (true, None) => Tag::success().child("成功"),
        (true, Some(c)) => Tag::warning().child(format!("退出 {c}")),
    };
    let took = r
        .duration_ms
        .map(|d| {
            if d >= 1000 {
                format!("{:.1} s", d as f64 / 1000.0)
            } else {
                format!("{d} ms")
            }
        })
        .unwrap_or_default();
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
                .child(what)
                .into_any_element(),
            result.outline().xsmall().into_any_element(),
            ui::clip(took),
        ],
        px(36.),
        cx,
    )
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
            ui::table_empty(
                &COLS,
                if self.records.is_empty() {
                    "还没有记录"
                } else {
                    "没有匹配的记录"
                },
                cx,
            )
            .into_any_element()
        } else {
            let (records, shown) = (self.records.clone(), self.shown.clone());
            ui::table(
                &COLS,
                uniform_list("audit-rows", shown.len(), move |range, _, cx| {
                    range.map(|i| render_row(&records[shown[i]], cx)).collect()
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
                format!("最近 {} 条远程操作（最新在前），来自 audit.jsonl。", self.records.len()),
                actions,
                cx,
            ))
            .child(div().flex_1().min_h_0().child(body))
    }
}

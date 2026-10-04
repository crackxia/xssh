//! Remote background jobs (`xssh job start`): records are local, states come from the hosts.

use crate::app::row_button;
use crate::backend::Backend;
use crate::ui::{self, Col, col, col_flex, col_right};
use gpui_kit::base::{Disableable as _, StyledExt as _};
use gpui_kit::component::button::{Button, ButtonVariant};
use gpui_kit::component::tag::Tag;
use gpui_kit::component::{ActiveTheme as _, IconName, Sizable as _, WindowExt as _, h_flex, v_flex};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::{
    AnyElement, Context, IntoElement, ParentElement as _, Render, SharedString, Styled as _, UniformListScrollHandle, Window, div, px,
    uniform_list,
};
use serde_json::Value;
use std::collections::HashMap;
use std::rc::Rc;
use std::sync::Arc;
use xssh_core::api::Request;
use xssh_store::jobs::JobRecord;

const COLS: [Col; 6] = [
    col("任务", 170.),
    col("主机", 110.),
    col_flex("命令"),
    col("启动时间", 130.),
    col("状态", 100.),
    col_right("操作", 110.),
];

pub struct JobsPage {
    backend: Arc<Backend>,
    records: Rc<Vec<JobRecord>>,
    /// id -> (state, exit code), from the last status query.
    states: HashMap<String, (String, Option<i64>)>,
    checking: bool,
    scroll: UniformListScrollHandle,
}

impl JobsPage {
    pub fn new(backend: Arc<Backend>, _: &mut Window, cx: &mut Context<Self>) -> Self {
        let mut p = JobsPage {
            backend,
            records: Rc::default(),
            states: HashMap::new(),
            checking: false,
            scroll: UniformListScrollHandle::new(),
        };
        p.reload(cx);
        p
    }

    pub fn reload(&mut self, cx: &mut Context<Self>) {
        self.records = Rc::new(self.backend.job_records());
        cx.notify();
    }

    /// Ask the hosts for each job's state (connects over SSH).
    fn check(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.checking = true;
        cx.notify();
        let fut = self.backend.act(Request::JobList { host: None, all: true });
        cx.spawn_in(window, async move |this, cx| {
            let r = fut.await;
            let _ = this.update_in(cx, |this, window, cx| {
                this.checking = false;
                match r {
                    Ok(Value::Array(rows)) => {
                        this.states = rows
                            .iter()
                            .filter_map(|r| {
                                Some((
                                    r["id"].as_str()?.to_string(),
                                    (r["state"].as_str().unwrap_or("unknown").to_string(), r["exit_code"].as_i64()),
                                ))
                            })
                            .collect();
                    }
                    Ok(_) => {}
                    Err(e) => ui::notify_error(window, cx, "查询任务状态失败", &e),
                }
                this.reload(cx);
            });
        })
        .detach();
    }

    fn show_logs(&mut self, id: String, window: &mut Window, cx: &mut Context<Self>) {
        let fut = self.backend.act(Request::JobLogs {
            id: id.clone(),
            offset: None,
            tail: Some(5000),
            max_bytes: 4 * 1024 * 1024,
        });
        cx.spawn_in(window, async move |_, cx| {
            let r = fut.await;
            let _ = cx.update(|window, cx| match r {
                Ok(v) => {
                    let lines: Rc<Vec<String>> = Rc::new(v["output"].as_str().unwrap_or("").lines().map(String::from).collect());
                    let state = v["state"].as_str().unwrap_or("").to_string();
                    let scroll = UniformListScrollHandle::new();
                    scroll.scroll_to_bottom();
                    window.open_dialog(cx, move |dialog, _, cx| {
                        dialog
                            .title(format!("任务 {id} 日志（{}）", state_label(&state)))
                            .w(px(900.))
                            .child(
                                div()
                                    .h(px(480.))
                                    .p_3()
                                    .rounded(cx.theme().radius)
                                    .bg(cx.theme().muted)
                                    .child(if lines.is_empty() {
                                        ui::empty("日志为空", cx).into_any_element()
                                    } else {
                                        ui::mono_view("job-log", lines.clone(), &scroll, cx).into_any_element()
                                    }),
                            )
                    });
                }
                Err(e) => ui::notify_error(window, cx, "读取日志失败", &e),
            });
        })
        .detach();
    }

    fn confirm_kill(&mut self, rec: JobRecord, window: &mut Window, cx: &mut Context<Self>) {
        let page = cx.entity().downgrade();
        window.open_alert_dialog(cx, move |alert, _, _| {
            let (page, rec) = (page.clone(), rec.clone());
            alert
                .title(format!("终止任务 {}？", rec.name.clone().unwrap_or(rec.id.clone())))
                .description(format!("向 {} 上的进程组发送 TERM：{}", rec.host, rec.command))
                .confirm()
                .ok_text("终止")
                .ok_variant(ButtonVariant::Danger)
                .cancel_text("取消")
                .on_ok(move |_, window, cx| {
                    let id = rec.id.clone();
                    let _ = page.update(cx, |p, cx| {
                        let fut = p.backend.act(Request::JobKill { id, signal: "TERM".into() });
                        cx.spawn_in(window, async move |this, cx| {
                            let r = fut.await;
                            let _ = this.update_in(cx, |this, window, cx| {
                                match r {
                                    Ok(_) => ui::notify_ok(window, cx, "已发送终止信号"),
                                    Err(e) => ui::notify_error(window, cx, "终止失败", &e),
                                }
                                this.check(window, cx);
                            });
                        })
                        .detach();
                    });
                    true
                })
        });
    }

    fn render_row(&self, r: &JobRecord, cx: &mut Context<Self>) -> AnyElement {
        let state = self.states.get(&r.id);
        let muted = cx.theme().muted_foreground;
        // Unknown until queried: plain muted text, so the tags that do show carry news.
        let status = match state.map(|s| s.0.as_str()) {
            None => div().text_xs().text_color(muted).child("未查询").into_any_element(),
            Some(st) => match st {
                "running" => Tag::info().child("运行中"),
                "exited" => match state.and_then(|s| s.1) {
                    Some(0) => Tag::success().child("完成 0"),
                    Some(c) => Tag::danger().child(format!("退出 {c}")),
                    None => Tag::success().child("已结束"),
                },
                s => Tag::warning().child(state_label(s)),
            }
            .outline()
            .xsmall()
            .into_any_element(),
        };
        let (id, rec) = (r.id.clone(), r.clone());
        let actions = h_flex()
            .gap_1()
            .child({
                let id = id.clone();
                row_button(SharedString::from(format!("log-{id}")), "日志")
                    .on_click(cx.listener(move |this, _, window, cx| this.show_logs(id.clone(), window, cx)))
            })
            .child(
                row_button(SharedString::from(format!("kill-{id}")), "终止")
                    .on_click(cx.listener(move |this, _, window, cx| this.confirm_kill(rec.clone(), window, cx))),
            );
        ui::table_row(
            &COLS,
            vec![
                h_flex()
                    .min_w_0()
                    .gap_1p5()
                    .child(div().min_w_0().truncate().font_medium().child(r.name.clone().unwrap_or(r.id.clone())))
                    .when(r.name.is_some(), |this| {
                        this.child(div().flex_none().text_xs().text_color(muted).child(r.id.clone()))
                    })
                    .into_any_element(),
                ui::clip(r.host.clone()),
                div()
                    .min_w_0()
                    .truncate()
                    .font_family(cx.theme().mono_font_family.clone())
                    .text_xs()
                    .child(ui::one_line(&r.command))
                    .into_any_element(),
                ui::clip(ui::short_time(&r.started_at)),
                status,
                actions.into_any_element(),
            ],
            px(40.),
            cx,
        )
        .into_any_element()
    }
}

fn state_label(s: &str) -> &'static str {
    match s {
        "running" => "运行中",
        "exited" => "已结束",
        "killed" => "已终止",
        "lost" => "丢失",
        _ => "未知",
    }
}

impl Render for JobsPage {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let actions = Button::new("jobs-check")
            .outline()
            .icon(IconName::RefreshCw)
            .label("查询状态")
            .loading(self.checking)
            .disabled(self.checking)
            .on_click(cx.listener(|this, _, window, cx| this.check(window, cx)));
        let body = if self.records.is_empty() {
            ui::table_empty(
                &COLS,
                "没有后台任务",
                "agent 用 `xssh job start <主机> -- <命令>` 启动的长任务会出现在这里；任务在远端运行，点“查询状态”连接主机获取最新状态。",
                cx,
            )
            .into_any_element()
        } else {
            let records = self.records.clone();
            ui::table(
                &COLS,
                uniform_list(
                    "job-rows",
                    records.len(),
                    cx.processor(move |this, range: std::ops::Range<usize>, _, cx| {
                        range.map(|i| this.render_row(&records[i], cx)).collect()
                    }),
                ),
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
                "后台任务",
                format!("{} 个任务。任务在远端独立运行，不受守护进程或本程序影响。", self.records.len()),
                actions,
                cx,
            ))
            .child(div().flex_1().min_h_0().child(body))
    }
}

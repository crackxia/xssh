//! Interactive sessions held by the daemon: who is doing what, the live screen, transcripts.
//! Watching uses `SessionPeek`, which does not count as activity: agents' idle timers and
//! "screen unchanged" tracking are unaffected. Every list here is virtualized.

use crate::backend::Backend;
use crate::model::DaemonModel;
use crate::ui;
use gpui_kit::base::StyledExt as _;
use gpui_kit::component::button::{Button, ButtonVariant, ButtonVariants as _};
use gpui_kit::component::tag::Tag;
use gpui_kit::component::{ActiveTheme as _, IconName, Sizable as _, WindowExt as _, h_flex, v_flex};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::{
    App, AppContext as _, Context, Entity, InteractiveElement as _, IntoElement, ParentElement as _, Render, SharedString,
    StatefulInteractiveElement as _, Styled as _, Task, UniformListScrollHandle, Window, div, px, uniform_list,
};
use std::path::PathBuf;
use std::rc::Rc;
use std::sync::Arc;
use std::time::Duration;
use xssh_core::api::{PromptKind, Request, ScreenSnapshot, SessionInfo, SessionState};

const POLL: Duration = Duration::from_millis(1000);
const CARD_H: f32 = 84.;
const HISTORY_H: f32 = 30.;

#[derive(Clone, PartialEq)]
enum Selected {
    Live(String),
    Closed(PathBuf),
}

#[derive(Clone, Copy, PartialEq)]
enum View {
    Screen,
    Log,
}

/// One refresh step of the detail view.
enum Poll {
    Screen(std::pin::Pin<Box<dyn std::future::Future<Output = xssh_core::Result<serde_json::Value>>>>),
    Log(Task<(u64, Vec<String>)>),
}

pub struct SessionsPage {
    backend: Arc<Backend>,
    daemon: Entity<DaemonModel>,
    selected: Option<Selected>,
    view: View,
    screen: Option<ScreenSnapshot>,
    /// Screen or transcript lines of the selection.
    lines: Rc<Vec<String>>,
    /// Transcript size when `lines` was read (live log view).
    log_len: u64,
    history: Rc<Vec<(String, PathBuf)>>,
    visible: bool,
    detail_scroll: UniformListScrollHandle,
    live_scroll: UniformListScrollHandle,
    history_scroll: UniformListScrollHandle,
    _poll: Option<Task<()>>,
}

impl SessionsPage {
    pub fn new(backend: Arc<Backend>, daemon: Entity<DaemonModel>, _: &mut Window, cx: &mut Context<Self>) -> Self {
        cx.observe(&daemon, |this, _, cx| {
            // Keep the selection valid when sessions come and go.
            if let Some(Selected::Live(id)) = &this.selected
                && !this.daemon.read(cx).sessions.iter().any(|s| &s.id == id)
            {
                this.selected = None;
                this.screen = None;
            }
            cx.notify();
        })
        .detach();
        SessionsPage {
            backend,
            daemon,
            selected: None,
            view: View::Screen,
            screen: None,
            lines: Rc::default(),
            log_len: 0,
            history: Rc::default(),
            visible: false,
            detail_scroll: UniformListScrollHandle::new(),
            live_scroll: UniformListScrollHandle::new(),
            history_scroll: UniformListScrollHandle::new(),
            _poll: None,
        }
    }

    pub fn set_visible(&mut self, visible: bool, cx: &mut Context<Self>) {
        if visible == self.visible {
            return;
        }
        self.visible = visible;
        if !visible {
            self._poll = None;
            return;
        }
        self.reload_history(cx);
        self._poll = Some(cx.spawn(async move |this, cx| {
            loop {
                let Ok(step) = this.update(cx, |this, cx| this.poll(cx)) else {
                    break;
                };
                let applied = match step {
                    Some(Poll::Screen(fut)) => {
                        let r = fut.await;
                        this.update(cx, |this, cx| {
                            if let Ok(v) = r
                                && let Ok(s) = serde_json::from_value::<ScreenSnapshot>(v)
                                && this.screen.as_ref().map(|x| &x.screen) != Some(&s.screen)
                            {
                                // `session run` wrapper and end-marker lines are plumbing, not content.
                                let lines = s.screen.lines().filter(|l| !l.contains("__XSSH_")).map(String::from).collect();
                                this.set_lines(lines, cx);
                                this.screen = Some(s);
                            }
                        })
                    }
                    Some(Poll::Log(task)) => {
                        let (len, lines) = task.await;
                        this.update(cx, |this, cx| {
                            this.log_len = len;
                            this.set_lines(lines, cx);
                        })
                    }
                    None => Ok(()),
                };
                if applied.is_err() {
                    break;
                }
                cx.background_executor().timer(POLL).await;
            }
        }));
    }

    fn reload_history(&mut self, cx: &mut Context<Self>) {
        let backend = self.backend.clone();
        let task = cx.background_spawn(async move { backend.closed_transcripts(usize::MAX) });
        cx.spawn(async move |this, cx| {
            let h = task.await;
            let _ = this.update(cx, |this, cx| {
                this.history = Rc::new(h);
                cx.notify();
            });
        })
        .detach();
    }

    /// What to fetch for the detail view now: a screen peek, or a transcript that grew.
    fn poll(&mut self, cx: &mut Context<Self>) -> Option<Poll> {
        let Some(Selected::Live(id)) = &self.selected else { return None };
        match self.view {
            View::Screen => Some(Poll::Screen(Box::pin(
                self.backend.observe(Request::SessionPeek { id: id.clone() }),
            ))),
            View::Log => {
                let path = self.daemon.read(cx).sessions.iter().find(|s| &s.id == id)?.transcript.clone();
                let len = std::fs::metadata(&path).map(|m| m.len()).unwrap_or(0);
                if len == self.log_len && !self.lines.is_empty() {
                    return None;
                }
                Some(Poll::Log(self.read_log(PathBuf::from(path), cx)))
            }
        }
    }

    /// Read and render a transcript off the UI thread (it can be megabytes).
    fn read_log(&self, path: PathBuf, cx: &mut Context<Self>) -> Task<(u64, Vec<String>)> {
        let backend = self.backend.clone();
        cx.background_spawn(async move {
            let len = std::fs::metadata(&path).map(|m| m.len()).unwrap_or(0);
            (len, backend.transcript(&path.to_string_lossy(), usize::MAX))
        })
    }

    /// Replace the detail lines, following the end when the view was already there.
    fn set_lines(&mut self, lines: Vec<String>, cx: &mut Context<Self>) {
        let follow = ui::at_end(&self.detail_scroll);
        self.lines = Rc::new(lines);
        if follow {
            self.detail_scroll.scroll_to_bottom();
        }
        cx.notify();
    }

    fn select(&mut self, sel: Selected, cx: &mut Context<Self>) {
        self.screen = None;
        self.lines = Rc::default();
        self.log_len = 0;
        self.detail_scroll.scroll_to_bottom();
        if let Selected::Closed(p) = &sel {
            self.view = View::Log;
            let task = self.read_log(p.clone(), cx);
            cx.spawn(async move |this, cx| {
                let (len, lines) = task.await;
                let _ = this.update(cx, |this, cx| {
                    this.log_len = len;
                    this.set_lines(lines, cx);
                });
            })
            .detach();
        }
        self.selected = Some(sel);
        cx.notify();
    }

    fn set_view(&mut self, view: View, cx: &mut Context<Self>) {
        if self.view != view {
            self.view = view;
            self.screen = None;
            self.lines = Rc::default();
            self.log_len = 0;
            self.detail_scroll.scroll_to_bottom();
            cx.notify();
        }
    }

    fn confirm_close(&mut self, s: SessionInfo, window: &mut Window, cx: &mut Context<Self>) {
        let page = cx.entity().downgrade();
        window.open_alert_dialog(cx, move |alert, _, _| {
            let (page, s) = (page.clone(), s.clone());
            alert
                .title(format!("关闭会话 {}？", s.key()))
                .description(format!(
                    "{} 上的 shell 及其中运行的程序会被结束。如果有 agent 正在使用这个会话，它的后续操作会失败。会话日志会保留。",
                    s.host
                ))
                .confirm()
                .ok_text("关闭会话")
                .ok_variant(ButtonVariant::Danger)
                .cancel_text("取消")
                .on_ok(move |_, window, cx| {
                    let id = s.id.clone();
                    let _ = page.update(cx, |p, cx| p.close(id, window, cx));
                    true
                })
        });
    }

    fn close(&mut self, id: String, window: &mut Window, cx: &mut Context<Self>) {
        let fut = self.backend.observe(Request::SessionClose { id });
        cx.spawn_in(window, async move |this, cx| {
            let r = fut.await;
            let _ = this.update_in(cx, |this, window, cx| {
                match r {
                    Ok(_) => ui::notify_ok(window, cx, "会话已关闭"),
                    Err(e) => ui::notify_error(window, cx, "关闭失败", &e),
                }
                this.selected = None;
                this.reload_history(cx);
                cx.notify();
            });
        })
        .detach();
    }

    fn render_card(&self, s: &SessionInfo, cx: &mut Context<Self>) -> impl IntoElement {
        let selected = self.selected == Some(Selected::Live(s.id.clone()));
        let id = s.id.clone();
        let muted = cx.theme().muted_foreground;
        div().h(px(CARD_H)).py_0p5().child(
            div()
                .id(SharedString::from(format!("s-{}", s.id)))
                .size_full()
                .p_2()
                .rounded(cx.theme().radius)
                .cursor_pointer()
                .when(selected, |this| this.bg(cx.theme().accent))
                .hover(|this| this.bg(cx.theme().secondary_hover))
                .on_click(cx.listener(move |this, _, _, cx| this.select(Selected::Live(id.clone()), cx)))
                .child(
                    h_flex()
                        .justify_between()
                        .child(div().font_semibold().truncate().child(s.key().to_string()))
                        .child(state_tag(s)),
                )
                .child(
                    div()
                        .text_xs()
                        .text_color(muted)
                        .child(format!("{} · 空闲 {}", s.host, ui::human_secs(s.idle_secs))),
                )
                .child(div().text_xs().truncate().child(if s.cmd.is_empty() {
                    " ".to_string()
                } else {
                    format!("运行：{}", s.cmd)
                }))
                .child(div().text_xs().truncate().text_color(muted).child(if s.last.is_empty() {
                    " ".to_string()
                } else {
                    format!("最近输入：{}", s.last)
                })),
        )
    }

    fn render_lists(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let d = self.daemon.read(cx);
        let n_live = d.sessions.len();
        let running = d.running();
        let live_paths: Vec<PathBuf> = d.sessions.iter().map(|s| PathBuf::from(&s.transcript)).collect();
        let history: Rc<Vec<(String, PathBuf)>> = Rc::new(self.history.iter().filter(|(_, p)| !live_paths.contains(p)).cloned().collect());
        let muted = cx.theme().muted_foreground;
        let live = if n_live == 0 {
            ui::empty(
                if running {
                    "没有打开的会话"
                } else {
                    "守护进程未运行，没有会话"
                },
                cx,
            )
            .into_any_element()
        } else {
            ui::virtual_list(
                uniform_list(
                    "live-sessions",
                    n_live,
                    cx.processor(|this, range: std::ops::Range<usize>, _, cx| {
                        let sessions = this.daemon.read(cx).sessions.clone();
                        range
                            .filter_map(|i| sessions.get(i).map(|s| this.render_card(s, cx).into_any_element()))
                            .collect()
                    }),
                ),
                &self.live_scroll,
                false,
            )
            .into_any_element()
        };
        let n_hist = history.len();
        v_flex()
            .w(px(300.))
            .flex_none()
            .h_full()
            .border_r_1()
            .border_color(cx.theme().border)
            .child(div().px_2().pt_2().flex_1().min_h(px(CARD_H)).child(live))
            .when(n_hist > 0, |this| {
                this.child(
                    div()
                        .px_4()
                        .py_2()
                        .border_t_1()
                        .border_color(cx.theme().border)
                        .text_xs()
                        .text_color(muted)
                        .child(format!("已关闭的会话日志（{n_hist}）")),
                )
                .child(div().px_2().flex_1().min_h_0().child(ui::virtual_list(
                    uniform_list(
                        "history",
                        n_hist,
                        cx.processor(move |this, range: std::ops::Range<usize>, _, cx| {
                            range
                                .map(|i| {
                                    let (name, path) = history[i].clone();
                                    let selected = this.selected == Some(Selected::Closed(path.clone()));
                                    div()
                                        .id(SharedString::from(format!("h-{name}")))
                                        .h(px(HISTORY_H))
                                        .px_2()
                                        .flex()
                                        .items_center()
                                        .rounded(cx.theme().radius)
                                        .text_sm()
                                        .cursor_pointer()
                                        .when(selected, |this| this.bg(cx.theme().accent))
                                        .hover(|this| this.bg(cx.theme().secondary_hover))
                                        .on_click(cx.listener(move |this, _, _, cx| this.select(Selected::Closed(path.clone()), cx)))
                                        .child(div().truncate().child(name))
                                        .into_any_element()
                                })
                                .collect()
                        }),
                    ),
                    &self.history_scroll,
                    false,
                )))
            })
    }

    fn render_lines(&self, cx: &App) -> impl IntoElement {
        div()
            .flex_1()
            .min_h_0()
            .p_3()
            .rounded(cx.theme().radius)
            .bg(cx.theme().muted)
            .child(if self.lines.is_empty() {
                ui::empty(
                    if self.view == View::Screen {
                        "正在读取屏幕…"
                    } else {
                        "日志为空"
                    },
                    cx,
                )
                .into_any_element()
            } else {
                ui::mono_view("session-lines", self.lines.clone(), &self.detail_scroll, cx).into_any_element()
            })
    }

    fn render_detail(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let live = match &self.selected {
            None => return ui::empty("选择左侧的会话查看它的屏幕和日志", cx).into_any_element(),
            Some(Selected::Closed(p)) => {
                return v_flex()
                    .size_full()
                    .gap_2()
                    .p_4()
                    .child(
                        div()
                            .font_semibold()
                            .child(p.file_name().map(|n| n.to_string_lossy().to_string()).unwrap_or_default()),
                    )
                    .child(self.render_lines(cx))
                    .into_any_element();
            }
            Some(Selected::Live(id)) => self.daemon.read(cx).sessions.iter().find(|s| &s.id == id).cloned(),
        };
        let Some(s) = live else {
            return ui::empty("会话已结束", cx).into_any_element();
        };
        let tab = |id: &'static str, label: &'static str, view: View, cx: &mut Context<Self>| {
            let b = Button::new(id).small().label(label);
            if self.view == view { b.primary() } else { b.outline() }.on_click(cx.listener(move |this, _, _, cx| this.set_view(view, cx)))
        };
        let header = h_flex()
            .justify_between()
            .gap_2()
            .child(
                v_flex()
                    .min_w_0()
                    .child(
                        h_flex()
                            .gap_2()
                            .child(div().text_lg().font_semibold().child(s.key().to_string()))
                            .child(state_tag(&s)),
                    )
                    .child(div().text_xs().text_color(cx.theme().muted_foreground).child(format!(
                        "{} · {}×{} · 创建于 {}",
                        s.host,
                        s.cols,
                        s.rows,
                        ui::short_time(&s.created_at)
                    ))),
            )
            .child(
                h_flex()
                    .flex_none()
                    .gap_2()
                    .child(tab("view-screen", "屏幕", View::Screen, cx))
                    .child(tab("view-log", "日志", View::Log, cx))
                    .child({
                        let s = s.clone();
                        Button::new("session-close")
                            .small()
                            .danger()
                            .icon(IconName::Close)
                            .label("关闭会话")
                            .on_click(cx.listener(move |this, _, window, cx| this.confirm_close(s.clone(), window, cx)))
                    }),
            );
        v_flex()
            .size_full()
            .gap_3()
            .p_4()
            .child(header)
            .child(self.render_lines(cx))
            .child(
                div()
                    .text_xs()
                    .text_color(cx.theme().muted_foreground)
                    .child("只读查看：不会向会话发送任何输入，也不影响 agent 的读取进度。"),
            )
            .into_any_element()
    }
}

fn state_tag(s: &SessionInfo) -> Tag {
    let tag = match (s.state, s.prompt) {
        (SessionState::Running, _) => Tag::info().child("运行中"),
        (SessionState::WaitingInput, Some(PromptKind::Shell) | None) => Tag::success().child("空闲"),
        (SessionState::WaitingInput, Some(PromptKind::Password)) => Tag::warning().child("等待密码"),
        (SessionState::WaitingInput, Some(PromptKind::Confirm)) => Tag::warning().child("等待确认"),
        (SessionState::WaitingInput, Some(PromptKind::Repl)) => Tag::info().child("REPL"),
        (SessionState::WaitingInput, Some(PromptKind::Pager)) => Tag::warning().child("分页器"),
        (SessionState::WaitingInput, Some(PromptKind::Input)) => Tag::warning().child("等待输入"),
        (SessionState::Quiet, _) => Tag::secondary().child("无输出"),
        (SessionState::Exited, _) => Tag::danger().child("已退出"),
        (SessionState::Disconnected, _) if s.persistent => Tag::warning().child("已分离"),
        (SessionState::Disconnected, _) => Tag::danger().child("连接断开"),
    };
    tag.outline().xsmall()
}

impl Render for SessionsPage {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let n = self.daemon.read(cx).sessions.len();
        v_flex()
            .size_full()
            .child(div().px_6().pt_6().pb_4().child(ui::page_header(
                "会话",
                format!("{n} 个交互式会话，由 agent 通过 `xssh session` 打开。这里只读查看。"),
                div(),
                cx,
            )))
            .child(
                h_flex()
                    .flex_1()
                    .min_h_0()
                    .items_start()
                    .border_t_1()
                    .border_color(cx.theme().border)
                    .child(self.render_lists(cx))
                    .child(div().flex_1().h_full().min_w_0().child(self.render_detail(cx))),
            )
    }
}

//! Daemon status, start/stop/restart, and its log.

use crate::backend::Backend;
use crate::model::{DaemonModel, DaemonStatus};
use crate::ui;
use gpui_kit::base::{Disableable as _, StyledExt as _};
use gpui_kit::component::button::{Button, ButtonVariant, ButtonVariants as _};
use gpui_kit::component::{ActiveTheme as _, IconName, WindowExt as _, h_flex, v_flex};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::{
    App, AppContext as _, Context, Entity, IntoElement, ParentElement as _, Render, Styled as _, UniformListScrollHandle,
    Window, div, px,
};
use std::future::Future;
use std::rc::Rc;
use std::sync::Arc;
use xssh_core::Result;

pub struct DaemonPage {
    backend: Arc<Backend>,
    daemon: Entity<DaemonModel>,
    log: Rc<Vec<String>>,
    log_scroll: UniformListScrollHandle,
}

#[derive(Clone, Copy)]
enum Op {
    Start,
    Stop,
    Restart,
}

impl DaemonPage {
    pub fn new(backend: Arc<Backend>, daemon: Entity<DaemonModel>, cx: &mut Context<Self>) -> Self {
        cx.observe(&daemon, |_, _, cx| cx.notify()).detach();
        DaemonPage {
            backend,
            daemon,
            log: Rc::default(),
            log_scroll: UniformListScrollHandle::new(),
        }
    }

    /// The log rotates at 5 MB: read it off the UI thread, show the newest lines at the bottom.
    pub fn reload(&mut self, cx: &mut Context<Self>) {
        let backend = self.backend.clone();
        let task = cx.background_spawn(async move { backend.daemon_log(usize::MAX).lines().map(String::from).collect::<Vec<_>>() });
        cx.spawn(async move |this, cx| {
            let lines = task.await;
            let _ = this.update(cx, |this, cx| {
                this.log = Rc::new(lines);
                this.log_scroll.scroll_to_bottom();
                cx.notify();
            });
        })
        .detach();
    }

    fn confirm(&mut self, op: Op, window: &mut Window, cx: &mut Context<Self>) {
        if matches!(op, Op::Start) {
            self.run(op, window, cx);
            return;
        }
        let d = self.daemon.read(cx);
        let held = if d.running() {
            format!("它持有的 {} 个会话和 {} 个端口转发", d.sessions.len(), d.forwards.len())
        } else {
            "它持有的所有会话和端口转发".to_string()
        };
        let page = cx.entity().downgrade();
        let (title, ok) = match op {
            Op::Stop => ("停止守护进程？", "停止"),
            _ => ("重启守护进程？", "重启"),
        };
        window.open_alert_dialog(cx, move |alert, _, _| {
            let page = page.clone();
            alert
                .title(title)
                .description(format!(
                    "将关闭{held}，正在使用它们的 agent 会失去这些会话。已保存的主机、密码和后台任务不受影响。"
                ))
                .confirm()
                .ok_text(ok)
                .ok_variant(ButtonVariant::Danger)
                .cancel_text("取消")
                .on_ok(move |_, window, cx| {
                    let _ = page.update(cx, |p, cx| p.run(op, window, cx));
                    true
                })
        });
    }

    fn run(&mut self, op: Op, window: &mut Window, cx: &mut Context<Self>) {
        let fut: std::pin::Pin<Box<dyn Future<Output = Result<()>>>> = match op {
            Op::Start => Box::pin(self.backend.start_daemon()),
            Op::Stop => Box::pin(self.backend.stop_daemon()),
            Op::Restart => Box::pin(self.backend.restart_daemon()),
        };
        self.daemon.update(cx, |m, cx| {
            m.busy = true;
            cx.notify();
        });
        cx.spawn_in(window, async move |this, cx| {
            let r = fut.await;
            let _ = this.update_in(cx, |this, window, cx| {
                this.daemon.update(cx, |m, cx| {
                    m.busy = false;
                    cx.notify();
                });
                match r {
                    Ok(()) => ui::notify_ok(
                        window,
                        cx,
                        match op {
                            Op::Start => "守护进程已启动",
                            Op::Stop => "守护进程已停止",
                            Op::Restart => "守护进程已重启",
                        },
                    ),
                    Err(e) => ui::notify_error(window, cx, "操作失败", &e),
                }
                this.reload(cx);
            });
        })
        .detach();
    }
}

fn row(label: &str, value: impl IntoElement, cx: &App) -> impl IntoElement {
    h_flex()
        .gap_4()
        .py_1()
        .child(
            div()
                .w(px(110.))
                .flex_none()
                .text_sm()
                .text_color(cx.theme().muted_foreground)
                .child(label.to_string()),
        )
        .child(div().min_w_0().text_sm().child(value))
}

impl Render for DaemonPage {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let d = self.daemon.read(cx);
        let running = d.running();
        // A daemon that answers but is incompatible can still be stopped or restarted.
        let present = running || matches!(d.status, DaemonStatus::Error(_));
        let busy = d.busy;
        let (dot, status) = match &d.status {
            DaemonStatus::Unknown => (cx.theme().muted_foreground, "检测中…".to_string()),
            DaemonStatus::Running => (cx.theme().success, "运行中".to_string()),
            DaemonStatus::Stopped => (cx.theme().muted_foreground, "未运行（agent 调用 xssh 时自动启动）".to_string()),
            DaemonStatus::Error(e) => (cx.theme().danger, e.clone()),
        };
        let conns = if d.connections.is_empty() {
            "无".to_string()
        } else {
            d.connections
                .iter()
                .map(|(a, n)| if *n > 1 { format!("{a} ×{n}") } else { a.clone() })
                .collect::<Vec<_>>()
                .join("，")
        };
        let info = v_flex()
            .p_4()
            .rounded(cx.theme().radius)
            .border_1()
            .border_color(cx.theme().border)
            .child(row(
                "状态",
                h_flex()
                    .gap_2()
                    .child(div().flex_none().size(px(8.)).rounded_full().bg(dot))
                    .child(div().font_medium().child(status)),
                cx,
            ))
            .when_some(d.pid, |this, pid| this.child(row("PID", pid.to_string(), cx)))
            .when(running, |this| {
                this.child(row("已运行", ui::human_secs(d.uptime_secs), cx))
                    .child(row("版本", d.version.clone(), cx))
                    .child(row(
                        "密钥存储",
                        if d.secret_backend == "keyring" {
                            "系统凭据管理器"
                        } else {
                            "加密文件"
                        },
                        cx,
                    ))
                    .child(row("SSH 连接", conns, cx))
                    .child(row("会话 / 转发", format!("{} / {}", d.sessions.len(), d.forwards.len()), cx))
            })
            .child(row("数据目录", self.backend.paths.home.display().to_string(), cx));
        let actions = h_flex()
            .gap_2()
            .child(
                Button::new("daemon-start")
                    .primary()
                    .icon(IconName::Play)
                    .label("启动")
                    .disabled(present || busy)
                    .on_click(cx.listener(|this, _, window, cx| this.confirm(Op::Start, window, cx))),
            )
            .child(
                Button::new("daemon-restart")
                    .outline()
                    .icon(IconName::RotateCw)
                    .label("重启")
                    .disabled(!present || busy)
                    .on_click(cx.listener(|this, _, window, cx| this.confirm(Op::Restart, window, cx))),
            )
            .child(
                Button::new("daemon-stop")
                    .danger()
                    .icon(IconName::Pause)
                    .label("停止")
                    .disabled(!present || busy)
                    .on_click(cx.listener(|this, _, window, cx| this.confirm(Op::Stop, window, cx))),
            );
        v_flex()
            .size_full()
            .p_6()
            .gap_4()
            .child(ui::page_header(
                "守护进程",
                "持有 SSH 连接、会话和端口转发。关闭本窗口不会影响它；空闲一段时间后它会自行退出。",
                actions,
                cx,
            ))
            .child(info)
            .child(
                h_flex()
                    .justify_between()
                    .child(div().text_sm().font_weight(gpui_kit::FontWeight::MEDIUM).child("守护进程日志"))
                    .child(
                        Button::new("daemon-log-refresh")
                            .ghost()
                            .icon(IconName::RefreshCw)
                            .label("刷新")
                            .on_click(cx.listener(|this, _, _, cx| this.reload(cx))),
                    ),
            )
            .child(
                div()
                    .flex_1()
                    .min_h_0()
                    .p_3()
                    .rounded(cx.theme().radius)
                    .bg(cx.theme().muted)
                    .child(if self.log.is_empty() {
                        ui::empty("暂无日志", cx).into_any_element()
                    } else {
                        ui::mono_view("daemon-log", self.log.clone(), &self.log_scroll, cx).into_any_element()
                    }),
            )
    }
}

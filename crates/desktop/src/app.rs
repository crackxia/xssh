//! Main window: navigation, daemon status polling, and the active page.

use crate::backend::Backend;
use crate::model::{DaemonModel, DaemonStatus};
use crate::pages::{AuditPage, DaemonPage, ForwardsPage, HostsPage, IntegrationsPage, JobsPage, SessionsPage};
use gpui_kit::base::StyledExt as _;
use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::{ActiveTheme as _, Icon, IconName, Sizable as _, TitleBar, h_flex, v_flex};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::{
    AppContext as _, Context, Entity, Image, ImageFormat, InteractiveElement as _, IntoElement, ParentElement as _, Render,
    StatefulInteractiveElement as _, Styled as _, Task, Window, div, img, px,
};
use std::sync::{Arc, LazyLock};
use std::time::Duration;
use xssh_core::api::Request;

const POLL: Duration = Duration::from_secs(2);

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Page {
    Hosts,
    Sessions,
    Forwards,
    Jobs,
    Audit,
    Daemon,
    Integrations,
}

impl Page {
    const ALL: [Page; 7] = [
        Page::Hosts,
        Page::Sessions,
        Page::Forwards,
        Page::Jobs,
        Page::Audit,
        Page::Daemon,
        Page::Integrations,
    ];

    fn label(self) -> &'static str {
        match self {
            Page::Hosts => "主机",
            Page::Sessions => "会话",
            Page::Forwards => "端口转发",
            Page::Jobs => "后台任务",
            Page::Audit => "审计日志",
            Page::Daemon => "守护进程",
            Page::Integrations => "集成",
        }
    }

    fn icon(self) -> IconName {
        match self {
            Page::Hosts => IconName::Globe,
            Page::Sessions => IconName::SquareTerminal,
            Page::Forwards => IconName::Network,
            Page::Jobs => IconName::Cpu,
            Page::Audit => IconName::FileText,
            Page::Daemon => IconName::Settings,
            Page::Integrations => IconName::Bot,
        }
    }

    fn id(self) -> &'static str {
        match self {
            Page::Hosts => "nav-hosts",
            Page::Sessions => "nav-sessions",
            Page::Forwards => "nav-forwards",
            Page::Jobs => "nav-jobs",
            Page::Audit => "nav-audit",
            Page::Daemon => "nav-daemon",
            Page::Integrations => "nav-integrations",
        }
    }
}

pub struct MainView {
    page: Page,
    daemon: Entity<DaemonModel>,
    hosts: Entity<HostsPage>,
    sessions: Entity<SessionsPage>,
    forwards: Entity<ForwardsPage>,
    jobs: Entity<JobsPage>,
    audit: Entity<AuditPage>,
    daemon_page: Entity<DaemonPage>,
    integrations: Entity<IntegrationsPage>,
    _poll: Task<()>,
}

impl MainView {
    pub fn new(backend: Arc<Backend>, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let daemon = cx.new(|_| DaemonModel::new());
        cx.observe(&daemon, |_, _, cx| cx.notify()).detach();
        let poll = {
            let backend = backend.clone();
            let daemon = daemon.downgrade();
            cx.spawn(async move |_, cx| {
                loop {
                    let r = backend.observe(Request::DaemonStatus).await;
                    if daemon
                        .update(cx, |m, cx| {
                            m.apply(r);
                            cx.notify();
                        })
                        .is_err()
                    {
                        break;
                    }
                    cx.background_executor().timer(POLL).await;
                }
            })
        };
        MainView {
            page: Page::Hosts,
            hosts: cx.new(|cx| HostsPage::new(backend.clone(), window, cx)),
            sessions: cx.new(|cx| SessionsPage::new(backend.clone(), daemon.clone(), window, cx)),
            forwards: cx.new(|cx| ForwardsPage::new(backend.clone(), daemon.clone(), window, cx)),
            jobs: cx.new(|cx| JobsPage::new(backend.clone(), window, cx)),
            audit: cx.new(|cx| AuditPage::new(backend.clone(), window, cx)),
            daemon_page: cx.new(|cx| DaemonPage::new(backend.clone(), daemon.clone(), cx)),
            integrations: cx.new(IntegrationsPage::new),
            daemon,
            _poll: poll,
        }
    }

    fn show(&mut self, page: Page, cx: &mut Context<Self>) {
        self.page = page;
        match page {
            Page::Hosts => self.hosts.update(cx, |p, cx| p.reload(cx)),
            Page::Jobs => self.jobs.update(cx, |p, cx| p.reload(cx)),
            Page::Audit => self.audit.update(cx, |p, cx| p.reload(cx)),
            Page::Daemon => self.daemon_page.update(cx, |p, cx| p.reload(cx)),
            Page::Integrations => self.integrations.update(cx, |p, cx| p.reload(cx)),
            Page::Sessions | Page::Forwards => {}
        }
        self.sessions.update(cx, |p, cx| p.set_visible(page == Page::Sessions, cx));
        cx.notify();
    }

    fn render_nav(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let d = self.daemon.read(cx);
        let (dot, text) = match &d.status {
            DaemonStatus::Unknown => (cx.theme().muted_foreground, "检测中…".to_string()),
            DaemonStatus::Running => (
                cx.theme().success,
                format!("运行中 · {} 会话 · {} 转发", d.sessions.len(), d.forwards.len()),
            ),
            DaemonStatus::Stopped => (cx.theme().muted_foreground, "未运行（按需自动启动）".to_string()),
            DaemonStatus::Error(_) => (cx.theme().danger, "版本不兼容".to_string()),
        };
        let counts = |p: Page| match p {
            Page::Sessions if !d.sessions.is_empty() => Some(d.sessions.len()),
            Page::Forwards if !d.forwards.is_empty() => Some(d.forwards.len()),
            _ => None,
        };
        v_flex()
            .w(px(200.))
            .h_full()
            .flex_none()
            .border_r_1()
            .border_color(cx.theme().border)
            .bg(cx.theme().sidebar)
            .child(v_flex().px_2().pt_3().gap_1().flex_1().children(Page::ALL.into_iter().map(|p| {
                let label = match counts(p) {
                    Some(n) => format!("{}  {n}", p.label()),
                    None => p.label().to_string(),
                };
                let selected = self.page == p;
                h_flex()
                    .id(p.id())
                    .w_full()
                    .gap_2()
                    .px_3()
                    .py_2()
                    .rounded(cx.theme().radius)
                    .text_sm()
                    .cursor_pointer()
                    .when(selected, |this| {
                        this.bg(cx.theme().sidebar_accent)
                            .text_color(cx.theme().sidebar_accent_foreground)
                            .font_medium()
                    })
                    .when(!selected, |this| this.hover(|this| this.bg(cx.theme().sidebar_accent.opacity(0.5))))
                    .on_click(cx.listener(move |this, _, _, cx| this.show(p, cx)))
                    .child(Icon::new(p.icon()).small())
                    .child(label)
            })))
            .child(
                div()
                    .id("nav-status")
                    .px_4()
                    .py_3()
                    .border_t_1()
                    .border_color(cx.theme().border)
                    .cursor_pointer()
                    .on_click(cx.listener(|this, _, _, cx| this.show(Page::Daemon, cx)))
                    .child(
                        h_flex()
                            .gap_2()
                            .child(div().size(px(8.)).rounded_full().bg(dot))
                            .child(div().text_xs().text_color(cx.theme().muted_foreground).child("守护进程")),
                    )
                    .child(div().pt_1().text_xs().child(text)),
            )
    }
}

/// The app icon (`assets/icon`), shown in the title bar.
static LOGO: LazyLock<Arc<Image>> = LazyLock::new(|| {
    Arc::new(Image::from_bytes(
        ImageFormat::Png,
        include_bytes!("../../../assets/icon/xssh-64.png").to_vec(),
    ))
});

impl Render for MainView {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let content = match self.page {
            Page::Hosts => self.hosts.clone().into_any_element(),
            Page::Sessions => self.sessions.clone().into_any_element(),
            Page::Forwards => self.forwards.clone().into_any_element(),
            Page::Jobs => self.jobs.clone().into_any_element(),
            Page::Audit => self.audit.clone().into_any_element(),
            Page::Daemon => self.daemon_page.clone().into_any_element(),
            Page::Integrations => self.integrations.clone().into_any_element(),
        };
        // Frameless window: the title bar is ours (drag, double-click, and on Windows the system
        // snap layouts on the maximize button all still work through its control areas).
        let title_bar = TitleBar::new().bg(cx.theme().sidebar).border_color(cx.theme().border).child(
            h_flex()
                .gap_2()
                .child(img(LOGO.clone()).size(px(18.)))
                .child(div().text_sm().font_semibold().child("xssh")),
        );
        v_flex()
            .size_full()
            .bg(cx.theme().background)
            .text_color(cx.theme().foreground)
            .child(title_bar)
            .child(
                h_flex()
                    .w_full()
                    .flex_1()
                    .min_h_0()
                    .items_start()
                    .child(self.render_nav(cx))
                    .child(div().flex_1().h_full().min_w_0().child(content)),
            )
    }
}

/// Keeps sizes consistent when a button is used in a table row.
pub fn row_button(id: impl Into<gpui_kit::ElementId>, label: &str) -> Button {
    Button::new(id).ghost().xsmall().label(label.to_string())
}

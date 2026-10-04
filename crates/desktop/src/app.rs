//! Main window: navigation, daemon status polling, and the active page.

use crate::backend::Backend;
use crate::i18n::{self, Lang, t, tf};
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
            Page::Hosts => t("主机", "Hosts"),
            Page::Sessions => t("会话", "Sessions"),
            Page::Forwards => t("端口转发", "Port forwards"),
            Page::Jobs => t("后台任务", "Jobs"),
            Page::Audit => t("审计日志", "Audit log"),
            Page::Daemon => t("守护进程", "Daemon"),
            Page::Integrations => t("集成", "Integrations"),
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
    backend: Arc<Backend>,
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
            backend,
            daemon,
            _poll: poll,
        }
    }

    /// Switch the interface language. Pages are rebuilt so text set when they were built
    /// (placeholders, choices) follows too; their data is reloaded from the store and daemon.
    fn set_lang(&mut self, lang: Lang, window: &mut Window, cx: &mut Context<Self>) {
        if lang == i18n::lang() {
            return;
        }
        i18n::set(lang, &self.backend.paths.home);
        let (backend, daemon) = (self.backend.clone(), self.daemon.clone());
        self.sessions.update(cx, |p, cx| p.set_visible(false, cx));
        self.hosts = cx.new(|cx| HostsPage::new(backend.clone(), window, cx));
        self.sessions = cx.new(|cx| SessionsPage::new(backend.clone(), daemon.clone(), window, cx));
        self.forwards = cx.new(|cx| ForwardsPage::new(backend.clone(), daemon.clone(), window, cx));
        self.jobs = cx.new(|cx| JobsPage::new(backend.clone(), window, cx));
        self.audit = cx.new(|cx| AuditPage::new(backend.clone(), window, cx));
        self.daemon_page = cx.new(|cx| DaemonPage::new(backend.clone(), daemon.clone(), cx));
        self.integrations = cx.new(IntegrationsPage::new);
        self.show(self.page, window, cx);
    }

    fn show(&mut self, page: Page, window: &mut Window, cx: &mut Context<Self>) {
        self.page = page;
        match page {
            Page::Hosts => self.hosts.update(cx, |p, cx| p.reload(cx)),
            Page::Jobs => self.jobs.update(cx, |p, cx| p.reload(cx)),
            Page::Audit => self.audit.update(cx, |p, cx| p.reload(cx)),
            Page::Daemon => self.daemon_page.update(cx, |p, cx| p.reload(cx)),
            Page::Integrations => self.integrations.update(cx, |p, cx| p.reload(cx)),
            Page::Forwards => self.forwards.update(cx, |p, cx| p.reload(window, cx)),
            Page::Sessions => {}
        }
        self.sessions.update(cx, |p, cx| p.set_visible(page == Page::Sessions, cx));
        cx.notify();
    }

    fn render_nav(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let d = self.daemon.read(cx);
        let (dot, text) = match &d.status {
            DaemonStatus::Unknown => (cx.theme().muted_foreground, t("检测中…", "Checking…").to_string()),
            DaemonStatus::Running => (
                cx.theme().success,
                tf!(
                    "运行中 · {} 会话 · {} 转发",
                    "Running · {} sess · {} fwd",
                    d.sessions.len(),
                    d.forwards.len()
                ),
            ),
            DaemonStatus::Stopped => (
                cx.theme().muted_foreground,
                t("未运行（按需自动启动）", "Stopped (starts on demand)").to_string(),
            ),
            DaemonStatus::Error(_) => (cx.theme().danger, t("版本不兼容", "Version mismatch").to_string()),
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
                    .on_click(cx.listener(move |this, _, window, cx| this.show(p, window, cx)))
                    .child(Icon::new(p.icon()).small())
                    .child(div().flex_1().child(p.label()))
                    .when_some(counts(p), |this, n| {
                        this.child(div().text_xs().text_color(cx.theme().muted_foreground).child(n.to_string()))
                    })
            })))
            .child(
                div()
                    .id("nav-status")
                    .px_4()
                    .py_3()
                    .border_t_1()
                    .border_color(cx.theme().border)
                    .cursor_pointer()
                    .on_click(cx.listener(|this, _, window, cx| this.show(Page::Daemon, window, cx)))
                    .child(
                        h_flex().gap_2().child(div().size(px(8.)).rounded_full().bg(dot)).child(
                            div()
                                .text_xs()
                                .text_color(cx.theme().muted_foreground)
                                .child(t("守护进程", "Daemon")),
                        ),
                    )
                    .child(div().pt_1().text_xs().truncate().child(text)),
            )
            .child(self.render_lang(cx))
    }

    /// "中文 · English": the current language in the foreground, the other one clickable.
    fn render_lang(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let current = i18n::lang();
        h_flex()
            .px_4()
            .py_2()
            .gap_1()
            .border_t_1()
            .border_color(cx.theme().border)
            .text_xs()
            .text_color(cx.theme().muted_foreground)
            .children([Lang::Zh, Lang::En].into_iter().enumerate().map(|(i, lang)| {
                let item = div()
                    .id(("lang", i))
                    .px_1p5()
                    .py_0p5()
                    .rounded(cx.theme().radius)
                    .child(lang.name());
                if lang == current {
                    item.text_color(cx.theme().foreground).font_medium().into_any_element()
                } else {
                    item.cursor_pointer()
                        .hover(|this| this.bg(cx.theme().sidebar_accent.opacity(0.5)).text_color(cx.theme().foreground))
                        .on_click(cx.listener(move |this, _, window, cx| this.set_lang(lang, window, cx)))
                        .into_any_element()
                }
            }))
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

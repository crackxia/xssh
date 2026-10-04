//! Port forwards held by the daemon: list, add, stop.

use crate::app::row_button;
use crate::backend::Backend;
use crate::model::DaemonModel;
use crate::ui::{self, Col, col, col_flex, col_right};
use gpui_kit::base::Disableable as _;
use gpui_kit::component::button::{Button, ButtonVariant, ButtonVariants as _};
use gpui_kit::component::input::{Input, InputState};
use gpui_kit::component::radio::RadioGroup;
use gpui_kit::component::tag::Tag;
use gpui_kit::component::{ActiveTheme as _, IconName, Sizable as _, WindowExt as _, h_flex, v_flex};
use gpui_kit::{
    AnyElement, AppContext as _, Context, Entity, IntoElement, ParentElement as _, Render, SharedString, Styled as _,
    UniformListScrollHandle, Window, div, px, uniform_list,
};
use std::sync::Arc;
use xssh_core::api::{ForwardInfo, Kind, Request};

pub struct ForwardsPage {
    backend: Arc<Backend>,
    daemon: Entity<DaemonModel>,
    host: Entity<InputState>,
    spec: Entity<InputState>,
    remote: bool,
    adding: bool,
    scroll: UniformListScrollHandle,
}

impl ForwardsPage {
    pub fn new(backend: Arc<Backend>, daemon: Entity<DaemonModel>, window: &mut Window, cx: &mut Context<Self>) -> Self {
        cx.observe(&daemon, |_, _, cx| cx.notify()).detach();
        ForwardsPage {
            backend,
            daemon,
            host: cx.new(|cx| InputState::new(window, cx).placeholder("主机别名")),
            spec: cx.new(|cx| InputState::new(window, cx).placeholder("5433:localhost:5432")),
            remote: false,
            adding: false,
            scroll: UniformListScrollHandle::new(),
        }
    }

    fn add(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let host = self.host.read(cx).value().trim().to_string();
        let spec = self.spec.read(cx).value().trim().to_string();
        if host.is_empty() || spec.is_empty() {
            ui::notify_error(window, cx, "无法添加", &xssh_core::Error::usage("请填写主机别名和转发规则"));
            return;
        }
        let (local, remote) = if self.remote { (None, Some(spec)) } else { (Some(spec), None) };
        self.adding = true;
        cx.notify();
        let fut = self.backend.act(Request::ForwardAdd {
            host,
            local,
            remote,
            dynamic: None,
        });
        cx.spawn_in(window, async move |this, cx| {
            let r = fut.await;
            let _ = this.update_in(cx, |this, window, cx| {
                this.adding = false;
                match r {
                    Ok(v) => {
                        ui::notify_ok(window, cx, format!("已添加转发 {}", v["description"].as_str().unwrap_or("")));
                        this.spec.update(cx, |s, cx| s.set_value("", window, cx));
                    }
                    Err(e) => ui::notify_error(window, cx, "添加失败", &e),
                }
                cx.notify();
            });
        })
        .detach();
    }

    fn confirm_stop(&mut self, f: ForwardInfo, window: &mut Window, cx: &mut Context<Self>) {
        let page = cx.entity().downgrade();
        window.open_alert_dialog(cx, move |alert, _, _| {
            let (page, f) = (page.clone(), f.clone());
            alert
                .title(format!("停止转发 {}？", f.id))
                .description(format!("{}\n正在使用它的连接会断开。", f.description))
                .confirm()
                .ok_text("停止")
                .ok_variant(ButtonVariant::Danger)
                .cancel_text("取消")
                .on_ok(move |_, window, cx| {
                    let id = f.id.clone();
                    let _ = page.update(cx, |p, cx| {
                        let fut = p.backend.observe(Request::ForwardStop { id });
                        cx.spawn_in(window, async move |_, cx| {
                            let r = fut.await;
                            let _ = cx.update(|window, cx| match r {
                                Ok(_) => ui::notify_ok(window, cx, "转发已停止"),
                                Err(e) => ui::notify_error(window, cx, "停止失败", &e),
                            });
                        })
                        .detach();
                    });
                    true
                })
        });
    }
}

impl ForwardsPage {
    fn render_row(&self, f: ForwardInfo, cx: &mut Context<Self>) -> AnyElement {
        let (dir, listen, target) = match f.spec.kind {
            Kind::Local => (
                "-L",
                format!("本机 {}:{}", f.spec.bind_addr, f.spec.bind_port),
                format!("{} 可达的 {}:{}", f.host, f.spec.target_host, f.spec.target_port),
            ),
            Kind::Remote => (
                "-R",
                format!("{} 的 {}:{}", f.host, f.spec.bind_addr, f.spec.bind_port),
                format!("本机 {}:{}", f.spec.target_host, f.spec.target_port),
            ),
            Kind::Dynamic => (
                "-D",
                format!("本机 SOCKS5 {}:{}", f.spec.bind_addr, f.spec.bind_port),
                format!("经 {} 访问任意地址", f.host),
            ),
        };
        let status = if f.alive {
            Tag::success().outline().xsmall().child("正常")
        } else {
            Tag::danger().outline().xsmall().child("已断开")
        };
        let id = f.id.clone();
        ui::table_row(
            &COLS,
            vec![
                ui::clip(f.id.clone()),
                ui::clip(f.host.clone()),
                ui::clip(dir),
                ui::clip(listen),
                ui::clip(target),
                ui::clip(f.connections.to_string()),
                status.into_any_element(),
                row_button(SharedString::from(format!("stop-{id}")), "停止")
                    .on_click(cx.listener(move |this, _, window, cx| this.confirm_stop(f.clone(), window, cx)))
                    .into_any_element(),
            ],
            px(40.),
            cx,
        )
        .into_any_element()
    }
}

const COLS: [Col; 8] = [
    col("ID", 90.),
    col("主机", 120.),
    col("方向", 60.),
    col_flex("监听"),
    col_flex("目标"),
    col_right("连接数", 70.),
    col("状态", 80.),
    col_right("操作", 70.),
];

impl Render for ForwardsPage {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let forwards = self.daemon.read(cx).forwards.clone();
        let add = h_flex()
            .gap_2()
            .child(div().w(px(140.)).child(Input::new(&self.host)))
            .child(
                RadioGroup::horizontal("fwd-kind")
                    .children(["本地 -L", "远端 -R"])
                    .selected_index(Some(self.remote as usize))
                    .on_change(cx.listener(|this, ix: &usize, _, cx| {
                        this.remote = *ix == 1;
                        cx.notify();
                    })),
            )
            .child(div().w(px(220.)).child(Input::new(&self.spec)))
            .child(
                Button::new("fwd-add")
                    .primary()
                    .icon(IconName::Plus)
                    .label("添加")
                    .loading(self.adding)
                    .disabled(self.adding)
                    .on_click(cx.listener(|this, _, window, cx| this.add(window, cx))),
            );
        let hint = if self.remote {
            "远端 -R：[绑定地址:]远端端口:本机目标地址:端口。服务器上的端口转到本机。"
        } else {
            "本地 -L：[绑定地址:]本机端口:目标地址:端口。本机 127.0.0.1 的端口经服务器转到目标。"
        };
        let body = if forwards.is_empty() {
            ui::table_empty(&COLS, "没有端口转发", cx).into_any_element()
        } else {
            let n = forwards.len();
            ui::table(
                &COLS,
                uniform_list(
                    "fwd-rows",
                    n,
                    cx.processor(|this, range: std::ops::Range<usize>, _, cx| {
                        let forwards = this.daemon.read(cx).forwards.clone();
                        range
                            .filter_map(|i| forwards.get(i).map(|f| this.render_row(f.clone(), cx)))
                            .collect()
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
                "端口转发",
                "由守护进程维持；停止守护进程会关闭所有转发。",
                div(),
                cx,
            ))
            .child(
                v_flex()
                    .gap_2()
                    .p_4()
                    .rounded(cx.theme().radius)
                    .border_1()
                    .border_color(cx.theme().border)
                    .child(add)
                    .child(div().text_xs().text_color(cx.theme().muted_foreground).child(hint)),
            )
            .child(div().flex_1().min_h_0().child(body))
    }
}

//! Saved hosts: list, add/edit/remove, stored secrets, connection test, host key trust.

use crate::app::row_button;
use crate::backend::{Backend, SecretKind};
use crate::ui::{self, Col, col, col_flex, col_right};
use gpui_kit::base::StyledExt as _;
use gpui_kit::component::button::{Button, ButtonVariant, ButtonVariants as _};
use gpui_kit::component::form::{Field, Form};
use gpui_kit::component::input::{Input, InputEvent, InputState};
use gpui_kit::component::radio::RadioGroup;
use gpui_kit::component::tag::Tag;
use gpui_kit::component::{ActiveTheme as _, IconName, Sizable as _, WindowExt as _, h_flex, v_flex};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::{
    AnyElement, App, AppContext as _, Context, Entity, IntoElement, ParentElement as _, Render, SharedString, Styled as _, Subscription,
    UniformListScrollHandle, Window, div, px, uniform_list,
};
use std::collections::{HashMap, HashSet};
use std::rc::Rc;
use std::sync::Arc;
use xssh_core::api::Request;
use xssh_core::{Error, ErrorCode, Result};
use xssh_store::hosts::Host;

pub struct HostsPage {
    backend: Arc<Backend>,
    hosts: Vec<Host>,
    secrets: HashMap<String, Vec<SecretKind>>,
    load_error: Option<String>,
    search: Entity<InputState>,
    testing: HashSet<String>,
    scroll: UniformListScrollHandle,
    _subs: Vec<Subscription>,
}

impl HostsPage {
    pub fn new(backend: Arc<Backend>, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let search = cx.new(|cx| InputState::new(window, cx).placeholder("搜索别名、地址、标签或备注"));
        let sub = cx.subscribe(&search, |_, _, e: &InputEvent, cx| {
            if matches!(e, InputEvent::Change) {
                cx.notify();
            }
        });
        let mut page = HostsPage {
            backend,
            hosts: vec![],
            secrets: HashMap::new(),
            load_error: None,
            search,
            testing: HashSet::new(),
            scroll: UniformListScrollHandle::new(),
            _subs: vec![sub],
        };
        page.reload(cx);
        page
    }

    pub fn reload(&mut self, cx: &mut Context<Self>) {
        match self.backend.hosts() {
            Ok(h) => {
                self.secrets = self.backend.stored_secrets(&h.iter().map(|h| h.alias.clone()).collect::<Vec<_>>());
                self.hosts = h;
                self.load_error = None;
            }
            Err(e) => self.load_error = Some(ui::error_text(&e)),
        }
        cx.notify();
    }

    fn visible(&self, cx: &App) -> Vec<Host> {
        let q = self.search.read(cx).value().to_lowercase();
        self.hosts
            .iter()
            .filter(|h| {
                q.is_empty()
                    || h.alias.to_lowercase().contains(&q)
                    || h.host.to_lowercase().contains(&q)
                    || h.user.to_lowercase().contains(&q)
                    || h.tags.iter().any(|t| t.to_lowercase().contains(&q))
                    || h.note.as_deref().is_some_and(|n| n.to_lowercase().contains(&q))
            })
            .cloned()
            .collect()
    }

    fn open_form(&mut self, original: Option<Host>, window: &mut Window, cx: &mut Context<Self>) {
        let form = cx.new(|cx| HostForm::new(original.as_ref(), window, cx));
        let page = cx.entity().downgrade();
        let orig = original.map(|h| h.alias);
        let title = if orig.is_some() { "编辑主机" } else { "添加主机" };
        window.open_dialog(cx, move |dialog, _, _| {
            let (page, form, orig) = (page.clone(), form.clone(), orig.clone());
            dialog.title(title).w(px(640.)).child(form.clone()).footer(
                h_flex()
                    .gap_2()
                    .justify_end()
                    .child(
                        Button::new("host-cancel")
                            .outline()
                            .label("取消")
                            .on_click(|_, window, cx| window.close_dialog(cx)),
                    )
                    .child(Button::new("host-save").primary().label("保存").on_click(move |_, window, cx| {
                        let host = form.read(cx).host(cx);
                        let _ = page.update(cx, |p, cx| p.save(orig.as_deref(), host, window, cx));
                    })),
            )
        });
    }

    fn save(&mut self, original: Option<&str>, host: Result<Host>, window: &mut Window, cx: &mut Context<Self>) {
        let r = host.and_then(|h| {
            let alias = h.alias.clone();
            self.backend.save_host(original, h).map(|_| alias)
        });
        match r {
            Ok(alias) => {
                window.close_dialog(cx);
                ui::notify_ok(window, cx, format!("已保存主机 {alias}"));
                self.reload(cx);
            }
            Err(e) => ui::notify_error(window, cx, "保存失败", &e),
        }
    }

    fn open_secrets(&mut self, alias: String, window: &mut Window, cx: &mut Context<Self>) {
        let stored = self.secrets.get(&alias).cloned().unwrap_or_default();
        let form = cx.new(|cx| SecretForm::new(alias.clone(), stored, window, cx));
        let page = cx.entity().downgrade();
        window.open_dialog(cx, move |dialog, _, _| {
            let (page, form) = (page.clone(), form.clone());
            let (page2, form2) = (page.clone(), form.clone());
            dialog.title("凭据").w(px(520.)).child(form.clone()).footer(
                h_flex()
                    .w_full()
                    .gap_2()
                    .justify_between()
                    .child(
                        Button::new("secret-clear")
                            .ghost()
                            .label("删除已保存的")
                            .on_click(move |_, window, cx| {
                                let (alias, kind) = form2.read(cx).target();
                                let _ = page2.update(cx, |p, cx| p.set_secret(alias, kind, None, window, cx));
                            }),
                    )
                    .child(
                        h_flex()
                            .gap_2()
                            .child(
                                Button::new("secret-cancel")
                                    .outline()
                                    .label("取消")
                                    .on_click(|_, window, cx| window.close_dialog(cx)),
                            )
                            .child(Button::new("secret-save").primary().label("保存").on_click(move |_, window, cx| {
                                let (alias, kind) = form.read(cx).target();
                                let value = form.read(cx).value(cx);
                                let _ = page.update(cx, |p, cx| p.set_secret(alias, kind, Some(value), window, cx));
                            })),
                    ),
            )
        });
    }

    fn set_secret(&mut self, alias: String, kind: SecretKind, value: Option<String>, window: &mut Window, cx: &mut Context<Self>) {
        if value.as_deref().is_some_and(|v| v.is_empty() || v.contains(['\n', '\r'])) {
            ui::notify_error(window, cx, "保存失败", &Error::usage("请输入单行、非空的值"));
            return;
        }
        match self.backend.set_secret(&alias, kind, value.as_deref()) {
            Ok(()) => {
                window.close_dialog(cx);
                let what = if value.is_some() { "已保存" } else { "已删除" };
                ui::notify_ok(window, cx, format!("{alias} 的{}{what}", kind.label()));
                self.reload(cx);
            }
            Err(e) => ui::notify_error(window, cx, "操作失败", &e),
        }
    }

    fn confirm_remove(&mut self, alias: String, window: &mut Window, cx: &mut Context<Self>) {
        let page = cx.entity().downgrade();
        window.open_alert_dialog(cx, move |alert, _, _| {
            let (page, alias) = (page.clone(), alias.clone());
            alert
                .title(format!("删除主机 {alias}？"))
                .description("同时删除为它保存的密码和口令。远端服务器不受影响。")
                .confirm()
                .ok_text("删除")
                .ok_variant(ButtonVariant::Danger)
                .cancel_text("取消")
                .on_ok(move |_, window, cx| {
                    let alias = alias.clone();
                    let _ = page.update(cx, |p, cx| match p.backend.remove_host(&alias) {
                        Ok(()) => {
                            ui::notify_ok(window, cx, format!("已删除主机 {alias}"));
                            p.reload(cx);
                        }
                        Err(e) => ui::notify_error(window, cx, "删除失败", &e),
                    });
                    true
                })
        });
    }

    fn test(&mut self, alias: String, window: &mut Window, cx: &mut Context<Self>) {
        self.testing.insert(alias.clone());
        cx.notify();
        let fut = self.backend.act(Request::HostTest { host: alias.clone() });
        cx.spawn_in(window, async move |this, cx| {
            let r = fut.await;
            let _ = this.update_in(cx, |this, window, cx| {
                this.testing.remove(&alias);
                match r {
                    Ok(v) => {
                        let os = v["facts"]["os_pretty"].as_str().or(v["facts"]["os"].as_str()).unwrap_or("");
                        ui::notify_ok(
                            window,
                            cx,
                            format!(
                                "{alias} 连接正常：{} 认证，{} ms {os}",
                                v["auth"].as_str().unwrap_or("?"),
                                v["connect_ms"]
                            ),
                        );
                    }
                    Err(e) if e.code == ErrorCode::HostKeyMismatch => this.offer_trust(alias.clone(), e, window, cx),
                    Err(e) => ui::notify_error(window, cx, &format!("{alias} 连接失败"), &e),
                }
                this.reload(cx);
            });
        })
        .detach();
    }

    /// The server's host key changed: only a person who verified the new key may accept it.
    fn offer_trust(&mut self, alias: String, e: Error, window: &mut Window, cx: &mut Context<Self>) {
        let page = cx.entity().downgrade();
        let detail = ui::error_text(&e);
        window.open_alert_dialog(cx, move |alert, _, _| {
            let (page, alias) = (page.clone(), alias.clone());
            alert
                .title(format!("{alias} 的主机密钥已变化"))
                .description(format!(
                    "{detail}\n\n可能是服务器重装或更换了密钥，也可能是中间人攻击。请先通过其他渠道确认新指纹，再选择信任。"
                ))
                .confirm()
                .ok_text("我已核实，信任新密钥")
                .ok_variant(ButtonVariant::Danger)
                .cancel_text("取消")
                .on_ok(move |_, window, cx| {
                    let alias = alias.clone();
                    let _ = page.update(cx, |p, cx| p.trust(alias, window, cx));
                    true
                })
        });
    }

    fn trust(&mut self, alias: String, window: &mut Window, cx: &mut Context<Self>) {
        // Two steps: read the key the server presents now, then record exactly that key.
        let probe = self.backend.act(Request::HostTrust {
            host: alias.clone(),
            fingerprint: None,
        });
        let backend = self.backend.clone();
        cx.spawn_in(window, async move |this, cx| {
            let r = match probe.await {
                Ok(v) => {
                    let fp = v["presented_fingerprint"].as_str().map(String::from);
                    backend
                        .act(Request::HostTrust {
                            host: alias.clone(),
                            fingerprint: fp,
                        })
                        .await
                }
                Err(e) => Err(e),
            };
            let _ = this.update_in(cx, |_, window, cx| match r {
                Ok(v) => ui::notify_ok(
                    window,
                    cx,
                    format!("已信任 {alias} 的新主机密钥 {}", v["trusted_fingerprint"].as_str().unwrap_or("")),
                ),
                Err(e) => ui::notify_error(window, cx, "信任失败", &e),
            });
        })
        .detach();
    }

    fn render_row(&self, h: &Host, cx: &mut Context<Self>) -> AnyElement {
        let alias = h.alias.clone();
        let secrets = self.secrets.get(&h.alias).cloned().unwrap_or_default();
        let auth = {
            let mut parts = vec![];
            if let Some(k) = h.key_name.as_deref().or(h.key.as_deref()) {
                parts.push(format!("密钥 {}", short_path(k)));
            }
            if secrets.contains(&SecretKind::Password) {
                parts.push("密码".into());
            }
            if parts.is_empty() {
                parts.push("agent / 默认密钥".into());
            }
            if secrets.contains(&SecretKind::Sudo) {
                parts.push("sudo".into());
            }
            parts.join(" · ")
        };
        let facts = h.facts.clone().unwrap_or_default();
        let os = facts.os_pretty.or(facts.os).unwrap_or_default();
        let muted = cx.theme().muted_foreground;
        let actions = h_flex()
            .gap_1()
            .child({
                let a = alias.clone();
                row_button(SharedString::from(format!("test-{alias}")), "测试")
                    .loading(self.testing.contains(&alias))
                    .on_click(cx.listener(move |this, _, window, cx| this.test(a.clone(), window, cx)))
            })
            .child({
                let host = h.clone();
                row_button(SharedString::from(format!("edit-{alias}")), "编辑")
                    .on_click(cx.listener(move |this, _, window, cx| this.open_form(Some(host.clone()), window, cx)))
            })
            .child({
                let a = alias.clone();
                row_button(SharedString::from(format!("secret-{alias}")), "凭据")
                    .on_click(cx.listener(move |this, _, window, cx| this.open_secrets(a.clone(), window, cx)))
            })
            .child({
                let a = alias.clone();
                row_button(SharedString::from(format!("rm-{alias}")), "删除")
                    .on_click(cx.listener(move |this, _, window, cx| this.confirm_remove(a.clone(), window, cx)))
            });
        ui::table_row(
            &COLS,
            vec![
                v_flex()
                    .min_w_0()
                    .child(div().font_semibold().truncate().child(h.alias.clone()))
                    .when_some(h.note.clone(), |this, n| {
                        this.child(div().text_xs().text_color(muted).truncate().child(n))
                    })
                    .into_any_element(),
                ui::clip(format!(
                    "{}@{}{}",
                    h.user,
                    h.host,
                    if h.port == 22 { String::new() } else { format!(":{}", h.port) }
                )),
                ui::clip(auth),
                h_flex()
                    .gap_1()
                    .overflow_hidden()
                    .when_some(h.jump.clone(), |this, j| {
                        this.child(Tag::info().outline().child(format!("经 {j}")).xsmall())
                    })
                    .children(h.tags.iter().map(|t| Tag::secondary().child(t.clone()).xsmall()))
                    .into_any_element(),
                v_flex()
                    .min_w_0()
                    .child(div().truncate().child(os))
                    .when_some(facts.last_ok, |this, t| {
                        this.child(
                            div()
                                .text_xs()
                                .text_color(muted)
                                .truncate()
                                .child(format!("最近连通 {}", ui::short_time(&t))),
                        )
                    })
                    .into_any_element(),
                actions.into_any_element(),
            ],
            px(52.),
            cx,
        )
        .into_any_element()
    }
}

const COLS: [Col; 6] = [
    col("别名 / 备注", 180.),
    col("地址", 170.),
    col("认证", 140.),
    col("跳板 / 标签", 110.),
    col_flex("系统"),
    col_right("操作", 190.),
];

fn short_path(p: &str) -> String {
    p.rsplit(['/', '\\']).next().unwrap_or(p).to_string()
}

impl Render for HostsPage {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let visible = self.visible(cx);
        let actions = h_flex()
            .gap_2()
            .child(div().w(px(260.)).child(Input::new(&self.search).cleanable(true)))
            .child(
                Button::new("hosts-refresh")
                    .ghost()
                    .icon(IconName::RefreshCw)
                    .on_click(cx.listener(|this, _, _, cx| this.reload(cx))),
            )
            .child(
                Button::new("hosts-add")
                    .primary()
                    .icon(IconName::Plus)
                    .label("添加主机")
                    .on_click(cx.listener(|this, _, window, cx| this.open_form(None, window, cx))),
            );
        let body: gpui_kit::AnyElement = if let Some(e) = &self.load_error {
            ui::table_empty(&COLS, "读取 hosts.toml 失败", e.clone(), cx).into_any_element()
        } else if self.hosts.is_empty() {
            ui::table_empty(
                &COLS,
                "还没有主机",
                "点击“添加主机”，或在终端运行 `xssh host import` 导入 ~/.ssh/config。",
                cx,
            )
            .into_any_element()
        } else if visible.is_empty() {
            ui::table_empty(&COLS, "没有匹配的主机", "搜索范围包括别名、地址、用户名、标签和备注。", cx).into_any_element()
        } else {
            let visible = Rc::new(visible);
            ui::table(
                &COLS,
                uniform_list(
                    "host-rows",
                    visible.len(),
                    cx.processor(move |this, range: std::ops::Range<usize>, _, cx| {
                        range.map(|i| this.render_row(&visible[i], cx)).collect()
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
                "主机",
                format!(
                    "{} 台服务器。agent 按别名使用，密码存在系统凭据管理器里，agent 看不到。",
                    self.hosts.len()
                ),
                actions,
                cx,
            ))
            .child(div().flex_1().min_h_0().child(body))
    }
}

fn input(window: &mut Window, cx: &mut App, placeholder: &str, value: Option<String>) -> Entity<InputState> {
    cx.new(|cx| {
        let s = InputState::new(window, cx).placeholder(placeholder.to_string());
        match value {
            Some(v) => s.default_value(v),
            None => s,
        }
    })
}

/// The add/edit host form, shown in a dialog.
struct HostForm {
    alias: Entity<InputState>,
    host: Entity<InputState>,
    port: Entity<InputState>,
    user: Entity<InputState>,
    key: Entity<InputState>,
    jump: Entity<InputState>,
    tags: Entity<InputState>,
    note: Entity<InputState>,
    encoding: Entity<InputState>,
    /// Kept from the original host.
    key_name: Option<String>,
}

impl HostForm {
    fn new(h: Option<&Host>, window: &mut Window, cx: &mut Context<Self>) -> Self {
        HostForm {
            alias: input(window, cx, "例如 web1", h.map(|h| h.alias.clone())),
            host: input(window, cx, "IP 或域名", h.map(|h| h.host.clone())),
            port: input(window, cx, "22", h.map(|h| h.port.to_string())),
            user: input(window, cx, "例如 root", h.map(|h| h.user.clone())),
            key: input(window, cx, "私钥文件路径（可选）", h.and_then(|h| h.key.clone())),
            jump: input(window, cx, "跳板机别名（可选）", h.and_then(|h| h.jump.clone())),
            tags: input(window, cx, "用逗号分隔，例如 prod, web", h.map(|h| h.tags.join(", "))),
            note: input(window, cx, "用途说明，agent 会看到", h.and_then(|h| h.note.clone())),
            encoding: input(window, cx, "utf-8（默认）或 gbk", h.and_then(|h| h.encoding.clone())),
            key_name: h.and_then(|h| h.key_name.clone()),
        }
    }

    fn host(&self, cx: &App) -> Result<Host> {
        let v = |s: &Entity<InputState>| s.read(cx).value().trim().to_string();
        let opt = |s: &Entity<InputState>| Some(v(s)).filter(|x| !x.is_empty());
        let alias = v(&self.alias);
        let host = v(&self.host);
        let user = v(&self.user);
        if alias.is_empty() || host.is_empty() || user.is_empty() {
            return Err(Error::usage("别名、地址和用户名必填"));
        }
        let port = match opt(&self.port) {
            None => 22,
            Some(p) => p.parse().map_err(|_| Error::usage(format!("端口无效：{p}")))?,
        };
        Ok(Host {
            alias,
            host,
            port,
            user,
            key: opt(&self.key),
            key_name: self.key_name.clone(),
            jump: opt(&self.jump),
            tags: v(&self.tags)
                .split([',', '，', ' '])
                .map(str::trim)
                .filter(|t| !t.is_empty())
                .map(String::from)
                .collect(),
            note: opt(&self.note),
            encoding: opt(&self.encoding),
            facts: None,
            ..Default::default()
        })
    }
}

impl Render for HostForm {
    fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
        Form::new()
            .columns(2)
            .child(Field::new().label("别名").required(true).child(Input::new(&self.alias)))
            .child(Field::new().label("用户名").required(true).child(Input::new(&self.user)))
            .child(Field::new().label("地址").required(true).child(Input::new(&self.host)))
            .child(Field::new().label("端口").child(Input::new(&self.port)))
            .child(Field::new().label("私钥").col_span(2).child(Input::new(&self.key)))
            .child(Field::new().label("跳板机").child(Input::new(&self.jump)))
            .child(Field::new().label("远端编码").child(Input::new(&self.encoding)))
            .child(Field::new().label("标签").col_span(2).child(Input::new(&self.tags)))
            .child(Field::new().label("备注").col_span(2).child(Input::new(&self.note)))
    }
}

const KINDS: [SecretKind; 3] = [SecretKind::Password, SecretKind::Sudo, SecretKind::Passphrase];

/// Set or delete one stored secret of a host.
struct SecretForm {
    alias: String,
    stored: Vec<SecretKind>,
    kind: usize,
    value: Entity<InputState>,
}

impl SecretForm {
    fn new(alias: String, stored: Vec<SecretKind>, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let value = cx.new(|cx| InputState::new(window, cx).placeholder("输入后保存，不会显示给 agent").masked(true));
        SecretForm {
            alias,
            stored,
            kind: 0,
            value,
        }
    }

    fn target(&self) -> (String, SecretKind) {
        (self.alias.clone(), KINDS[self.kind])
    }

    fn value(&self, cx: &App) -> String {
        self.value.read(cx).value().to_string()
    }
}

impl Render for SecretForm {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let labels: Vec<String> = KINDS
            .iter()
            .map(|k| format!("{}{}", k.label(), if self.stored.contains(k) { "（已保存）" } else { "" }))
            .collect();
        v_flex()
            .gap_4()
            .child(div().text_sm().text_color(cx.theme().muted_foreground).child(format!(
                "为 {} 保存到系统凭据管理器。xssh 在需要时自动填写，agent 只能看到“已保存”。",
                self.alias
            )))
            .child(
                RadioGroup::horizontal("secret-kind")
                    .children(labels)
                    .selected_index(Some(self.kind))
                    .on_change(cx.listener(|this, ix: &usize, _, cx| {
                        this.kind = *ix;
                        cx.notify();
                    })),
            )
            .child(Input::new(&self.value).mask_toggle())
    }
}

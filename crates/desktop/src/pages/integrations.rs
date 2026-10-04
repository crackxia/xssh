//! Integrations: put the xssh folder on PATH, and install the xssh skill for agent CLIs
//! (Claude Code, Codex, Gemini CLI, ...).

use crate::app::row_button;
use crate::backend::cli_sibling;
use crate::ui::{self, Col, col, col_flex, col_right};
use gpui_kit::base::{Disableable as _, StyledExt as _};
use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::tag::Tag;
use gpui_kit::component::{ActiveTheme as _, IconName, Sizable as _, h_flex, v_flex};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::{
    AnyElement, App, AppContext as _, Context, IntoElement, ParentElement as _, Render, SharedString, Styled as _, UniformListScrollHandle,
    Window, div, px, uniform_list,
};
use std::path::PathBuf;
use std::rc::Rc;
use xssh_core::{Error, Result};
use xssh_store::skills::{self, AGENTS, Agent, SkillState};
use xssh_store::user_path::{self, PathStatus};

const COLS: [Col; 4] = [col("工具", 170.), col_flex("Skill 文件"), col("状态", 90.), col_right("操作", 130.)];

struct Row {
    agent: &'static Agent,
    detected: bool,
    state: SkillState,
    /// The skill file, shown relative to the home folder (`~`) so the tool-specific part is visible.
    file: String,
}

pub struct IntegrationsPage {
    /// The xssh CLI next to this app, or None when it is not there.
    cli: Option<PathBuf>,
    /// Its folder.
    cli_dir: Option<PathBuf>,
    path: Result<PathStatus>,
    path_busy: bool,
    home: Result<PathBuf>,
    /// What an install writes now (with the CLI's full path only while it is not on PATH); rows
    /// compare the installed file against it.
    skill: String,
    rows: Rc<Vec<Row>>,
    scroll: UniformListScrollHandle,
}

impl IntegrationsPage {
    pub fn new(cx: &mut Context<Self>) -> Self {
        let cli = cli_sibling();
        let mut p = IntegrationsPage {
            cli_dir: cli.as_ref().and_then(|c| c.parent().map(PathBuf::from)),
            cli,
            path: Ok(PathStatus::default()),
            path_busy: false,
            home: skills::home_dir(),
            skill: String::new(),
            rows: Rc::default(),
            scroll: UniformListScrollHandle::new(),
        };
        p.reload(cx);
        p
    }

    /// Re-read the PATH setting and every skill file (a few small reads).
    pub fn reload(&mut self, cx: &mut Context<Self>) {
        if let Some(dir) = &self.cli_dir {
            self.path = user_path::status(dir);
        }
        self.skill = skills::content(self.cli.as_deref());
        if let Ok(home) = &self.home {
            self.rows = Rc::new(
                AGENTS
                    .iter()
                    .map(|a| Row {
                        agent: a,
                        detected: a.detected(home),
                        state: a.state(home, &self.skill),
                        file: match a.skill_file(home).strip_prefix(home) {
                            Ok(rel) => format!("~{}{}", std::path::MAIN_SEPARATOR, rel.display()),
                            Err(_) => a.skill_file(home).display().to_string(),
                        },
                    })
                    .collect(),
            );
        }
        cx.notify();
    }

    fn set_path(&mut self, on: bool, window: &mut Window, cx: &mut Context<Self>) {
        let Some(dir) = self.cli_dir.clone() else { return };
        self.path_busy = true;
        cx.notify();
        // The settings broadcast can wait on slow windows: keep it off the UI thread.
        let task = cx.background_spawn(async move {
            if on {
                user_path::register(&dir)
            } else {
                user_path::unregister(&dir)
            }
        });
        cx.spawn_in(window, async move |this, cx| {
            let r = task.await;
            let _ = this.update_in(cx, |this, window, cx| {
                this.path_busy = false;
                match r {
                    Ok(()) if on => ui::notify_ok(window, cx, if cfg!(windows) { "已加入 PATH。从开始菜单或 Win+R 新启动的终端可直接运行 xssh；已打开的资源管理器窗口、终端窗口里启动的程序仍是旧 PATH" } else { "已加入 PATH。重新登录或新开登录 shell 后可直接运行 xssh" }),
                    Ok(()) => ui::notify_ok(window, cx, "已从 PATH 移除"),
                    Err(e) => ui::notify_error(window, cx, "修改 PATH 失败", &e),
                }
                this.reload(cx);
            });
        })
        .detach();
    }

    /// Write the current skill for `agents`; `verb` ("安装" / "更新") only words the notices.
    fn install(&mut self, agents: Vec<&'static Agent>, verb: &'static str, window: &mut Window, cx: &mut Context<Self>) {
        let Ok(home) = &self.home else { return };
        let mut done = Vec::new();
        for a in agents {
            match a.install(home, &self.skill) {
                Ok(_) => done.push(a.name),
                Err(e) => ui::notify_error(window, cx, &format!("为 {} {verb} skill 失败", a.name), &e),
            }
        }
        if !done.is_empty() {
            ui::notify_ok(window, cx, format!("已{verb} xssh skill：{}", done.join("、")));
        }
        self.reload(cx);
    }

    fn uninstall(&mut self, agent: &'static Agent, window: &mut Window, cx: &mut Context<Self>) {
        let Ok(home) = &self.home else { return };
        match agent.uninstall(home) {
            Ok(_) => ui::notify_ok(window, cx, format!("已从 {} 移除 xssh skill", agent.name)),
            Err(e) => ui::notify_error(window, cx, "移除失败", &e),
        }
        self.reload(cx);
    }

    fn render_row(&self, r: &Row, cx: &mut Context<Self>) -> AnyElement {
        let muted = cx.theme().muted_foreground;
        let tag = match r.state {
            SkillState::Current => Tag::success().child("已安装"),
            SkillState::Outdated => Tag::warning().child("需更新"),
            SkillState::Missing => Tag::secondary().child("未安装"),
        };
        let a = r.agent;
        let mut actions = h_flex().gap_1();
        if r.state != SkillState::Current {
            let label = if r.state == SkillState::Missing { "安装" } else { "更新" };
            actions = actions.child(
                row_button(SharedString::from(format!("skill-install-{}", a.id)), label)
                    .on_click(cx.listener(move |this, _, window, cx| this.install(vec![a], label, window, cx))),
            );
        }
        if r.state != SkillState::Missing {
            actions = actions.child(
                row_button(SharedString::from(format!("skill-rm-{}", a.id)), "移除")
                    .on_click(cx.listener(move |this, _, window, cx| this.uninstall(a, window, cx))),
            );
        }
        ui::table_row(
            &COLS,
            vec![
                v_flex()
                    .min_w_0()
                    .child(div().font_medium().truncate().child(a.name))
                    .when(!r.detected, |this| {
                        this.child(div().text_xs().text_color(muted).child("未检测到该工具"))
                    })
                    .into_any_element(),
                div()
                    .min_w_0()
                    .truncate()
                    .text_xs()
                    .font_family(cx.theme().mono_font_family.clone())
                    .text_color(if r.detected { cx.theme().foreground } else { muted })
                    .child(r.file.clone())
                    .into_any_element(),
                tag.outline().xsmall().into_any_element(),
                actions.into_any_element(),
            ],
            px(48.),
            cx,
        )
        .into_any_element()
    }

    fn render_path_card(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let hint = if cfg!(windows) {
            "把本文件夹放到当前用户环境变量 PATH 的最前面（HKCU\\Environment，无需管理员权限）。终端和 agent 可直接运行 `xssh`，不必写完整路径。只对之后启动的程序生效：从开始菜单或 Win+R 打开即可；从已打开的资源管理器文件夹窗口、Windows Terminal 窗口、VS Code 等里启动的仍继承旧 PATH，需先关闭这些窗口（或注销后重新登录）。"
        } else {
            "在 ~/.profile（以及 ~/.bash_profile / ~/.zprofile）中加入一段 PATH 设置，把本文件夹放在最前面。重新登录或新开终端后，终端和 agent 可直接运行 `xssh`。"
        };
        let status = self.path.as_ref().ok();
        let effective = matches!((&self.cli_dir, status), (Some(d), Some(s)) if s.effective(d));
        let (tag, registered) = match (&self.cli_dir, &self.path) {
            (None, _) => (Tag::danger().child("找不到 xssh"), false),
            (_, Err(_)) => (Tag::danger().child("无法读取"), false),
            (_, Ok(s)) if s.user && effective => (Tag::success().child("已注册"), true),
            (_, Ok(s)) if s.system && effective => (Tag::info().child("已在系统 PATH"), false),
            (_, Ok(s)) if s.unclosed_quote.is_some() => (Tag::danger().child("PATH 格式有误"), false),
            (_, Ok(s)) if s.resolved.is_some() => (Tag::warning().child("被其他 xssh 覆盖"), false),
            _ => (Tag::secondary().child("未注册"), false),
        };
        // Why a new terminal would not run this xssh: a broken PATH value, or another copy first.
        let shadow = status.filter(|_| !effective && self.cli_dir.is_some()).and_then(|s| {
            if let Some(entry) = &s.unclosed_quote {
                return Some(format!(
                    "PATH 条目 `{entry}` 的引号没有闭合：cmd 会把它之后的所有条目（包括本文件夹）当成一个不存在的文件夹，因此找不到 xssh。删掉这个多余的引号即可（在系统 PATH 中时需管理员权限：系统属性 → 环境变量）。"
                ));
            }
            s.resolved.as_ref().map(|p| {
                format!(
                    "新终端运行 `xssh` 会先找到 {}，而不是这里的 xssh。点击“加入 PATH”把本文件夹移到最前；若仍被覆盖（它在系统 PATH 中），请删除该文件或调整系统 PATH。",
                    p.display()
                )
            })
        });
        let button = if registered {
            Button::new("path-toggle").outline().label("从 PATH 移除")
        } else {
            Button::new("path-toggle").primary().icon(IconName::Plus).label("加入 PATH")
        }
        .small()
        .loading(self.path_busy)
        .disabled(self.path_busy || self.cli_dir.is_none())
        .on_click(cx.listener(move |this, _, window, cx| this.set_path(!registered, window, cx)));
        let folder = match &self.cli_dir {
            Some(d) => d.display().to_string(),
            None => "本程序所在文件夹里没有 xssh 可执行文件：请把 xssh 与 xssh-desktop 放在同一文件夹。".into(),
        };
        v_flex()
            .p_4()
            .gap_2()
            .rounded(cx.theme().radius)
            .border_1()
            .border_color(cx.theme().border)
            .child(
                h_flex()
                    .gap_4()
                    .justify_between()
                    .child(
                        h_flex()
                            .gap_2()
                            .child(div().font_medium().child("命令行 PATH"))
                            .child(tag.outline().xsmall()),
                    )
                    .child(button),
            )
            .child(div().text_xs().text_color(cx.theme().muted_foreground).child(hint))
            .child(
                h_flex()
                    .gap_2()
                    .text_xs()
                    .child(div().text_color(cx.theme().muted_foreground).child("文件夹"))
                    .child(div().font_family(cx.theme().mono_font_family.clone()).child(folder)),
            )
            .when_some(shadow, |this, text| {
                this.child(div().text_xs().text_color(cx.theme().warning).child(text))
            })
            .when_some(self.path.as_ref().err(), |this, e: &Error| {
                this.child(div().text_xs().text_color(cx.theme().danger).child(ui::error_text(e)))
            })
    }
}

fn section_title(title: &str, desc: &str, cx: &App) -> impl IntoElement {
    v_flex()
        .min_w_0()
        .flex_1()
        .gap_1()
        .child(div().font_medium().child(title.to_string()))
        .child(div().text_xs().text_color(cx.theme().muted_foreground).child(desc.to_string()))
}

impl Render for IntegrationsPage {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let pick = |f: &dyn Fn(&Row) -> bool| -> Vec<&'static Agent> { self.rows.iter().filter(|r| f(r)).map(|r| r.agent).collect() };
        // Every installed skill that differs from what this build writes (another build, or xssh
        // moved), and the detected tools that have none yet.
        let outdated = pick(&|r| r.state == SkillState::Outdated);
        let missing = pick(&|r| r.detected && r.state == SkillState::Missing);
        let update_all = (!outdated.is_empty()).then(|| {
            Button::new("skill-update-all")
                .primary()
                .small()
                .icon(IconName::RefreshCw)
                .label(format!("全部更新（{}）", outdated.len()))
                .on_click(cx.listener(move |this, _, window, cx| this.install(outdated.clone(), "更新", window, cx)))
        });
        let install_all = Button::new("skill-install-all")
            .small()
            .icon(IconName::Plus)
            .label(format!("安装到已检测到的工具（{}）", missing.len()))
            .disabled(missing.is_empty())
            .when(update_all.is_none(), |b| b.primary())
            .when(update_all.is_some(), |b| b.outline())
            .on_click(cx.listener(move |this, _, window, cx| this.install(missing.clone(), "安装", window, cx)));
        let table = match &self.home {
            Err(e) => ui::table_empty(&COLS, ui::error_text(e), cx).into_any_element(),
            Ok(_) => {
                let rows = self.rows.clone();
                ui::table(
                    &COLS,
                    uniform_list(
                        "skill-rows",
                        rows.len(),
                        cx.processor(move |this, range: std::ops::Range<usize>, _, cx| {
                            range.map(|i| this.render_row(&rows[i], cx)).collect()
                        }),
                    ),
                    &self.scroll,
                    cx,
                )
                .into_any_element()
            }
        };
        v_flex()
            .size_full()
            .p_6()
            .gap_4()
            .child(ui::page_header(
                "集成",
                "让终端和 AI 编程工具直接使用 xssh。",
                Button::new("integrations-refresh")
                    .ghost()
                    .icon(IconName::RefreshCw)
                    .on_click(cx.listener(|this, _, _, cx| this.reload(cx))),
                cx,
            ))
            .child(self.render_path_card(cx))
            .child(
                h_flex()
                    .gap_4()
                    .items_end()
                    .child(section_title(
                        "AI 编程工具 Skill",
                        "安装后，agent 遇到与远程服务器相关的任务会自动使用 xssh。Skill 内容即 `xssh guide`；xssh 未加入 PATH 时还会写明本机 xssh 的完整路径，已加入则省略以节省 agent 上下文。升级、移动 xssh 或加入 / 移出 PATH 后，状态会显示“需更新”，点“全部更新”即可。",
                        cx,
                    ))
                    .child(h_flex().gap_2().children(update_all).child(install_all)),
            )
            .child(div().flex_1().min_h_0().child(table))
    }
}

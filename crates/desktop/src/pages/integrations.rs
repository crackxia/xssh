//! Integrations: put the xssh folder on PATH, and install the xssh skill for agent CLIs
//! (Claude Code, Codex, Gemini CLI, ...).

use crate::app::row_button;
use crate::backend::cli_sibling;
use crate::i18n::{t, tf};
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

const COLS: [Col; 4] = [
    col("工具", "Tool", 170.),
    col_flex("Skill 文件", "Skill file"),
    col("状态", "State", 90.),
    col_right("操作", "Actions", 130.),
];

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
                    Ok(()) if on => ui::notify_ok(
                        window,
                        cx,
                        if cfg!(windows) {
                            t(
                                "已加入 PATH。从开始菜单或 Win+R 新启动的终端可直接运行 xssh；已打开的资源管理器窗口、终端窗口里启动的程序仍是旧 PATH",
                                "Added to PATH. Terminals started fresh from the Start menu or Win+R can run xssh; programs started from already open Explorer or terminal windows keep the old PATH",
                            )
                        } else {
                            t(
                                "已加入 PATH。重新登录或新开登录 shell 后可直接运行 xssh",
                                "Added to PATH. Log in again or open a new login shell to run xssh",
                            )
                        },
                    ),
                    Ok(()) => ui::notify_ok(window, cx, t("已从 PATH 移除", "Removed from PATH")),
                    Err(e) => ui::notify_error(window, cx, t("修改 PATH 失败", "Cannot change PATH"), &e),
                }
                this.reload(cx);
            });
        })
        .detach();
    }

    /// Write the current skill for `agents`; `update` only words the notices.
    fn install(&mut self, agents: Vec<&'static Agent>, update: bool, window: &mut Window, cx: &mut Context<Self>) {
        let Ok(home) = &self.home else { return };
        let mut done = Vec::new();
        for a in agents {
            match a.install(home, &self.skill) {
                Ok(_) => done.push(a.name),
                Err(e) => {
                    let title = match update {
                        true => tf!("为 {} 更新 skill 失败", "Cannot update the skill for {}", a.name),
                        false => tf!("为 {} 安装 skill 失败", "Cannot install the skill for {}", a.name),
                    };
                    ui::notify_error(window, cx, &title, &e)
                }
            }
        }
        if !done.is_empty() {
            let names = done.join(t("、", ", "));
            let msg = match update {
                true => tf!("已更新 xssh skill：{names}", "Updated the xssh skill: {names}"),
                false => tf!("已安装 xssh skill：{names}", "Installed the xssh skill: {names}"),
            };
            ui::notify_ok(window, cx, msg);
        }
        self.reload(cx);
    }

    fn uninstall(&mut self, agent: &'static Agent, window: &mut Window, cx: &mut Context<Self>) {
        let Ok(home) = &self.home else { return };
        match agent.uninstall(home) {
            Ok(_) => ui::notify_ok(
                window,
                cx,
                tf!("已从 {} 移除 xssh skill", "Removed the xssh skill from {}", agent.name),
            ),
            Err(e) => ui::notify_error(window, cx, t("移除失败", "Remove failed"), &e),
        }
        self.reload(cx);
    }

    fn render_row(&self, r: &Row, cx: &mut Context<Self>) -> AnyElement {
        let muted = cx.theme().muted_foreground;
        let tag = match r.state {
            SkillState::Current => Tag::success().child(t("已安装", "Installed")),
            SkillState::Outdated => Tag::warning().child(t("需更新", "Outdated")),
            SkillState::Missing => Tag::secondary().child(t("未安装", "Missing")),
        };
        let a = r.agent;
        let mut actions = h_flex().gap_1();
        if r.state != SkillState::Current {
            let update = r.state == SkillState::Outdated;
            let label = if update { t("更新", "Update") } else { t("安装", "Install") };
            actions = actions.child(
                row_button(SharedString::from(format!("skill-install-{}", a.id)), label)
                    .on_click(cx.listener(move |this, _, window, cx| this.install(vec![a], update, window, cx))),
            );
        }
        if r.state != SkillState::Missing {
            actions = actions.child(
                row_button(SharedString::from(format!("skill-rm-{}", a.id)), t("移除", "Remove"))
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
                        this.child(div().text_xs().text_color(muted).child(t("未检测到该工具", "Not detected")))
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
        // What it does, then when it takes effect: two short paragraphs instead of one long one.
        let (hint, effect) = if cfg!(windows) {
            (
                t(
                    "把本文件夹放到当前用户环境变量 PATH 的最前面（HKCU\\Environment，无需管理员权限）。终端和 agent 可直接运行 `xssh`，不必写完整路径。",
                    "Puts this folder first on your user PATH (HKCU\\Environment, no admin rights needed). Terminals and agents can then run `xssh` without its full path.",
                ),
                t(
                    "只对之后启动的程序生效：从开始菜单或 Win+R 打开即可；从已打开的资源管理器文件夹窗口、Windows Terminal 窗口、VS Code 等里启动的仍继承旧 PATH，需先关闭这些窗口（或注销后重新登录）。",
                    "Only programs started afterwards see it: open them from the Start menu or Win+R. Programs started from already open Explorer, Windows Terminal or VS Code windows inherit the old PATH until those windows are closed (or you sign out and back in).",
                ),
            )
        } else {
            (
                t(
                    "在 ~/.profile（以及 ~/.bash_profile / ~/.zprofile）中加入一段 PATH 设置，把本文件夹放在最前面。",
                    "Adds a PATH line to ~/.profile (and ~/.bash_profile / ~/.zprofile) that puts this folder first.",
                ),
                t(
                    "重新登录或新开终端后，终端和 agent 可直接运行 `xssh`。",
                    "After logging in again or opening a new terminal, terminals and agents can run `xssh`.",
                ),
            )
        };
        let status = self.path.as_ref().ok();
        let effective = matches!((&self.cli_dir, status), (Some(d), Some(s)) if s.effective(d));
        let (tag, registered) = match (&self.cli_dir, &self.path) {
            (None, _) => (Tag::danger().child(t("找不到 xssh", "xssh not found")), false),
            (_, Err(_)) => (Tag::danger().child(t("无法读取", "Unreadable")), false),
            (_, Ok(s)) if s.user && effective => (Tag::success().child(t("已注册", "On PATH")), true),
            (_, Ok(s)) if s.system && effective => (Tag::info().child(t("已在系统 PATH", "On system PATH")), false),
            (_, Ok(s)) if s.unclosed_quote.is_some() => (Tag::danger().child(t("PATH 格式有误", "Malformed PATH")), false),
            (_, Ok(s)) if s.resolved.is_some() => (Tag::warning().child(t("被其他 xssh 覆盖", "Shadowed")), false),
            _ => (Tag::secondary().child(t("未注册", "Not on PATH")), false),
        };
        // Why a new terminal would not run this xssh: a broken PATH value, or another copy first.
        let shadow = status.filter(|_| !effective && self.cli_dir.is_some()).and_then(|s| {
            if let Some(entry) = &s.unclosed_quote {
                return Some(tf!(
                    "PATH 条目 `{entry}` 的引号没有闭合：cmd 会把它之后的所有条目（包括本文件夹）当成一个不存在的文件夹，因此找不到 xssh。删掉这个多余的引号即可（在系统 PATH 中时需管理员权限：系统属性 → 环境变量）。",
                    "The PATH entry `{entry}` has an unclosed quote: cmd reads every entry after it (this folder included) as one folder that does not exist, so it cannot find xssh. Delete the stray quote (on the system PATH this needs admin rights: System Properties → Environment Variables)."
                ));
            }
            s.resolved.as_ref().map(|p| {
                tf!(
                    "新终端运行 `xssh` 会先找到 {}，而不是这里的 xssh。点击“加入 PATH”把本文件夹移到最前；若仍被覆盖（它在系统 PATH 中），请删除该文件或调整系统 PATH。",
                    "A new terminal running `xssh` finds {} before this one. Add to PATH moves this folder first; if it is still shadowed (from the system PATH), delete that file or adjust the system PATH.",
                    p.display()
                )
            })
        });
        let button = if registered {
            Button::new("path-toggle").outline().label(t("从 PATH 移除", "Remove from PATH"))
        } else {
            Button::new("path-toggle")
                .primary()
                .icon(IconName::Plus)
                .label(t("加入 PATH", "Add to PATH"))
        }
        .small()
        .loading(self.path_busy)
        .disabled(self.path_busy || self.cli_dir.is_none())
        .on_click(cx.listener(move |this, _, window, cx| this.set_path(!registered, window, cx)));
        let folder = match &self.cli_dir {
            Some(d) => d.display().to_string(),
            None => t(
                "本程序所在文件夹里没有 xssh 可执行文件：请把 xssh 与 xssh-desktop 放在同一文件夹。",
                "There is no xssh executable in this app's folder: keep xssh and xssh-desktop in the same folder.",
            )
            .into(),
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
                            .child(div().font_medium().child(t("命令行 PATH", "Command-line PATH")))
                            .child(tag.outline().xsmall()),
                    )
                    .child(button),
            )
            .child(
                v_flex()
                    .gap_1()
                    .text_xs()
                    .text_color(cx.theme().muted_foreground)
                    .child(hint)
                    .child(effect),
            )
            .child(
                h_flex()
                    .gap_2()
                    .text_xs()
                    .child(div().text_color(cx.theme().muted_foreground).child(t("文件夹", "Folder")))
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
        .child(
            div()
                .text_xs()
                .truncate()
                .text_color(cx.theme().muted_foreground)
                .child(desc.to_string()),
        )
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
                .label(tf!("全部更新（{}）", "Update all ({})", outdated.len()))
                .on_click(cx.listener(move |this, _, window, cx| this.install(outdated.clone(), true, window, cx)))
        });
        let install_all = Button::new("skill-install-all")
            .small()
            .icon(IconName::Plus)
            .label(tf!("安装到已检测到的工具（{}）", "Install for detected tools ({})", missing.len()))
            .disabled(missing.is_empty())
            .when(update_all.is_none(), |b| b.primary())
            .when(update_all.is_some(), |b| b.outline())
            .on_click(cx.listener(move |this, _, window, cx| this.install(missing.clone(), false, window, cx)));
        let table = match &self.home {
            Err(e) => {
                ui::table_empty(&COLS, t("无法确定用户主目录", "Cannot find the home folder"), ui::error_text(e), cx).into_any_element()
            }
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
                t("集成", "Integrations"),
                t(
                    "让终端和 AI 编程工具直接使用 xssh。",
                    "Let terminals and AI coding tools use xssh directly.",
                ),
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
                        t("AI 编程工具 Skill", "Skill for AI coding tools"),
                        t(
                            "agent 处理远程服务器任务时自动使用 xssh；升级、移动 xssh 或改动 PATH 后需更新。",
                            "Agents use xssh for server work. Update after moving or upgrading xssh.",
                        ),
                        cx,
                    ))
                    .child(h_flex().gap_2().children(update_all).child(install_all)),
            )
            .child(div().flex_1().min_h_0().child(table))
    }
}

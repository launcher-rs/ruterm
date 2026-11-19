//! //! 本地终端：shell 探测刷新、条目重命名 / 起始目录 / 参数 / 环境变量，标签创建。

use std::sync::Arc;

use rgpui::input_ui::{Input, InputEvent, InputState};
use rgpui::prelude::*;
use rgpui::{
    AnyElement, Button, ButtonVariants, Context, ContextMenuExt, Entity, Icon, IconName, Image,
    ImageFormat, InteractiveElement, IntoElement, MouseButton, ParentElement, PopupMenuItem,
    SharedString, StatefulInteractiveElement, Styled, Window, div, h_flex, img, px, v_flex,
};
use rgpui_term::{Event as TerminalEvent, TerminalBuilder, TerminalView};

use crate::NewLocalTerminal;
use crate::local_shell::{NetTool, ShellProfile};
use crate::platform::Platform;
use crate::tr;

use super::*;

/// 本地 shell 弹窗（重命名 / 设置起始目录）状态。
pub(super) struct ShellPrompt {
    /// profile id（路径 + 参数）；扫描重建列表后仍能定位。
    pub(super) id: String,
    pub(super) kind: ShellPromptKind,
    /// 系统默认名或默认目录文本，用于「留空恢复默认」提示。
    pub(super) default_text: String,
    pub(super) input: Entity<InputState>,
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub(super) enum ShellPromptKind {
    Rename,
    WorkingDirectory,
    Arguments,
    Environment,
}

impl ShellPromptKind {
    /// 环境变量是多行文本（每行一条），其余为单行。
    pub(super) fn multi_line(self) -> bool {
        matches!(self, Self::Environment)
    }
}

impl RuTermApp {
    /// 本地 shell 的展示名：用户改过用别名，否则用扫描出的系统名。
    pub(super) fn shell_name(&self, profile: &ShellProfile) -> SharedString {
        match self.local_shell_prefs.name_of(&profile.id()) {
            Some(alias) => SharedString::from(alias.to_string()),
            None => SharedString::from(profile.name.clone()),
        }
    }

    /// 搜索是否命中该 shell（自定义名、系统名或可执行文件名）。
    pub(super) fn shell_matches(&self, profile: &ShellProfile, query: &str) -> bool {
        if query.is_empty() {
            return true;
        }
        let query = query.to_lowercase();
        let alias_hit = self
            .local_shell_prefs
            .name_of(&profile.id())
            .is_some_and(|alias| alias.to_lowercase().contains(&query));
        alias_hit || profile.matches(&query)
    }

    /// 打开本地 shell 重命名弹窗（预填当前展示名）。
    pub(super) fn open_shell_prompt(
        &mut self,
        profile: &ShellProfile,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let id = profile.id();
        let default_text = profile.name.clone();
        let current = self.shell_name(profile).to_string();
        self.open_shell_input(
            id,
            ShellPromptKind::Rename,
            default_text.clone(),
            current,
            default_text,
            window,
            cx,
        );
    }

    /// 打开「起始目录」弹窗（预填已设置的目录，留空＝进程当前目录）。
    pub(super) fn open_shell_cwd_prompt(
        &mut self,
        profile: &ShellProfile,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let id = profile.id();
        let default_text = std::env::current_dir()
            .map(|dir| dir.display().to_string())
            .unwrap_or_default();
        let current = self.local_shell_prefs.cwd_of(&id).unwrap_or("").to_string();
        self.open_shell_input(
            id,
            ShellPromptKind::WorkingDirectory,
            default_text.clone(),
            current,
            default_text,
            window,
            cx,
        );
    }

    /// 打开「启动参数」弹窗（预填已保存的参数原文，留空＝不加参数）。
    pub(super) fn open_shell_args_prompt(
        &mut self,
        profile: &ShellProfile,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let id = profile.id();
        let current = self
            .local_shell_prefs
            .args
            .get(&id)
            .cloned()
            .unwrap_or_default();
        self.open_shell_input(
            id,
            ShellPromptKind::Arguments,
            String::new(),
            current,
            "-NoLogo -NoProfile".into(),
            window,
            cx,
        );
    }

    /// 打开「环境变量」弹窗（预填已保存的 KEY=VALUE 行，留空＝不注入）。
    pub(super) fn open_shell_env_prompt(
        &mut self,
        profile: &ShellProfile,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let id = profile.id();
        let current = crate::local_shell::env_text(&self.local_shell_prefs.env_map_of(&id));
        self.open_shell_input(
            id,
            ShellPromptKind::Environment,
            String::new(),
            current,
            "KEY=VALUE".into(),
            window,
            cx,
        );
    }

    #[allow(clippy::too_many_arguments)]
    pub(super) fn open_shell_input(
        &mut self,
        id: String,
        kind: ShellPromptKind,
        default_text: String,
        current: String,
        placeholder: String,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let multi_line = kind.multi_line();
        let input = cx.new(|cx| {
            let state = InputState::new(window, cx)
                .placeholder(placeholder)
                .default_value(current);
            if multi_line {
                // 多行输入在无固定高度的容器里会塌成一行，用 auto_grow 撑到 6 行起。
                state.auto_grow(6, 12)
            } else {
                state
            }
        });
        // 单行弹窗回车即确认，与分组重命名一致；多行里回车是换行，点「保存」提交。
        if !multi_line {
            cx.subscribe(&input, |this, _, event: &InputEvent, cx| {
                if matches!(event, InputEvent::PressEnter { shift: false, .. }) {
                    this.commit_shell_prompt(cx);
                }
            })
            .detach();
        }
        input.update(cx, |state, cx| state.focus(window, cx));
        self.shell_prompt = Some(ShellPrompt {
            id,
            kind,
            default_text,
            input,
        });
        cx.notify();
    }

    /// 确认弹窗：重命名留空/同名即恢复默认；起始目录留空即用进程当前目录。
    pub(super) fn commit_shell_prompt(&mut self, cx: &mut Context<Self>) {
        let Some(prompt) = self.shell_prompt.take() else {
            return;
        };
        let text = prompt.input.read(cx).value().trim().to_string();
        match prompt.kind {
            ShellPromptKind::Rename => {
                if text.is_empty() || text == prompt.default_text {
                    self.local_shell_prefs.rename(&prompt.id, "");
                    self.status =
                        SharedString::from(format!("已恢复默认名称 {}", prompt.default_text));
                } else {
                    self.local_shell_prefs.rename(&prompt.id, &text);
                    self.status = SharedString::from(format!("已重命名为 {text}"));
                }
            }
            ShellPromptKind::WorkingDirectory => {
                if text.is_empty() || text == prompt.default_text {
                    self.local_shell_prefs.set_cwd(&prompt.id, "");
                    self.status = "已恢复默认起始目录（进程当前目录）".into();
                } else if !std::path::Path::new(&text).is_dir() {
                    // 目录不存在就不写：否则每次开会话都会静默回落到默认目录，
                    // 用户以为设置生效了。
                    self.status = SharedString::from(format!("目录不存在：{text}"));
                    self.shell_prompt = Some(prompt);
                    cx.notify();
                    return;
                } else {
                    self.local_shell_prefs.set_cwd(&prompt.id, &text);
                    self.status = SharedString::from(format!("起始目录已设为 {text}"));
                }
            }
            ShellPromptKind::Arguments => {
                let args = crate::local_shell::split_args(&text);
                if args.is_empty() {
                    self.local_shell_prefs.set_args(&prompt.id, "");
                    self.status = "已清除自定义启动参数".into();
                } else {
                    // 规范化存储：引号按分词规则重排，回填时与解析结果一致。
                    let normalized = crate::local_shell::join_args(&args);
                    self.local_shell_prefs.set_args(&prompt.id, &normalized);
                    self.status = SharedString::from(format!("启动参数已设为 {normalized}"));
                }
            }
            ShellPromptKind::Environment => match crate::local_shell::parse_env(&text) {
                Err(error) => {
                    self.status = SharedString::from(error);
                    self.shell_prompt = Some(prompt);
                    cx.notify();
                    return;
                }
                Ok(env) if env.is_empty() => {
                    self.local_shell_prefs.set_env(&prompt.id, "");
                    self.status = "已清除自定义环境变量".into();
                }
                Ok(env) => {
                    let normalized = crate::local_shell::env_text(&env);
                    let count = env.len();
                    self.local_shell_prefs.set_env(&prompt.id, &normalized);
                    self.status = SharedString::from(format!("已设置 {count} 个环境变量"));
                }
            },
        }
        self.persist_local_shells(cx);
        cx.notify();
    }

    /// 「浏览…」选起始目录（系统目录选择对话框），回填弹窗输入框。
    pub(super) fn browse_shell_cwd(&mut self, cx: &mut Context<Self>) {
        let Some(prompt) = self.shell_prompt.as_ref() else {
            return;
        };
        let input = prompt.input.clone();
        let entity_id = cx.entity_id();
        let rx = cx.prompt_for_paths(rgpui::PathPromptOptions {
            files: false,
            directories: true,
            multiple: false,
            prompt: Some("选择起始目录".into()),
        });
        cx.spawn(async move |_, cx| {
            if let Ok(Ok(Some(paths))) = rx.await
                && let Some(path) = paths.first()
            {
                let value = path.to_string_lossy().into_owned();
                let _ = cx.with_window(entity_id, move |window, cx| {
                    input.update(cx, |state, cx| {
                        state.set_value(value, window, cx);
                    });
                });
            }
        })
        .detach();
    }

    /// 关闭重命名弹窗，不写入。
    pub(super) fn cancel_shell_prompt(&mut self, cx: &mut Context<Self>) {
        self.shell_prompt = None;
        cx.notify();
    }

    /// 右键「恢复默认名称」：直接清掉别名，不弹窗。
    pub(super) fn reset_shell_name(&mut self, profile: &ShellProfile, cx: &mut Context<Self>) {
        self.local_shell_prefs.rename(&profile.id(), "");
        self.status = SharedString::from(format!("已恢复默认名称 {}", profile.name));
        self.persist_local_shells(cx);
        cx.notify();
    }

    /// 右键「恢复默认起始目录」：清掉该 profile 的目录设置。
    pub(super) fn reset_shell_cwd(&mut self, profile: &ShellProfile, cx: &mut Context<Self>) {
        self.local_shell_prefs.set_cwd(&profile.id(), "");
        self.status = SharedString::from(format!(
            "{} 的起始目录已恢复为进程当前目录",
            self.shell_name(profile)
        ));
        self.persist_local_shells(cx);
        cx.notify();
    }

    /// 右键「清除启动参数」。
    pub(super) fn reset_shell_args(&mut self, profile: &ShellProfile, cx: &mut Context<Self>) {
        self.local_shell_prefs.set_args(&profile.id(), "");
        self.status = SharedString::from(format!("已清除 {} 的启动参数", self.shell_name(profile)));
        self.persist_local_shells(cx);
        cx.notify();
    }

    /// 右键「清除环境变量」。
    pub(super) fn reset_shell_env(&mut self, profile: &ShellProfile, cx: &mut Context<Self>) {
        self.local_shell_prefs.set_env(&profile.id(), "");
        self.status = SharedString::from(format!("已清除 {} 的环境变量", self.shell_name(profile)));
        self.persist_local_shells(cx);
        cx.notify();
    }

    /// 保存本机 shell 自定义名（库可用入库，否则退回 settings.json）。
    pub(super) fn persist_local_shells(&mut self, cx: &mut Context<Self>) {
        let json = self.local_shell_prefs.to_json();
        if let Some(vault) = self.vault.clone() {
            cx.spawn(async move |_, _| {
                if let Err(error) = vault.set_setting("local_shells", &json).await {
                    log::warn!("保存本机 shell 名称失败: {error}");
                }
            })
            .detach();
        } else {
            self.store.save_local_shells(&json);
        }
    }

    /// 启动时静默补扫一次：`detect()` 只查文件，WSL 得跑 `wsl.exe -l`，
    /// 不值得卡在启动路径上，但列表应当开箱即完整，所以后台补一次、不打扰状态栏。
    pub(super) fn auto_scan_local_shells(&mut self, cx: &mut Context<Self>) {
        self.fetch_local_shells(true, cx);
    }

    /// 一键扫描本机 shell（含 WSL）：`wsl.exe -l` 是子进程，丢给阻塞线程池，UI 不卡。
    pub(super) fn scan_local_shells(&mut self, cx: &mut Context<Self>) {
        self.fetch_local_shells(false, cx);
    }

    /// `quiet` 为真时不动状态栏（启动补扫用），扫到的列表与图标照常刷新。
    pub(super) fn fetch_local_shells(&mut self, quiet: bool, cx: &mut Context<Self>) {
        if self.scanning_shells {
            return;
        }
        self.scanning_shells = true;
        if !quiet {
            self.status = "正在扫描本机 shell…".into();
        }
        cx.notify();
        let runtime = rgpui::tokio::Tokio::handle(cx);
        cx.spawn(async move |this, cx| {
            // rgpui 的 `cx.spawn` 跑在自己的执行器上，`tokio::task::spawn_blocking`
            // 会因"没有 reactor"直接 panic；阻塞任务必须显式交给 tokio 运行时句柄。
            let found = runtime.spawn_blocking(crate::local_shell::scan).await;
            let _ = this.update(cx, |this, cx| {
                this.scanning_shells = false;
                // 扫描任务失败（如子进程处理里 panic）不能折成空列表把用户
                // 原有的 shell 配置抹掉：保留原列表，只报状态。
                match found {
                    Ok(found) => {
                        let count = found.len();
                        this.local_shells = found;
                        this.refresh_shell_icons();
                        if !quiet {
                            // 主机列表恒在左侧边栏，文案里的方位照实写。
                            let side = "左侧";
                            this.status = if count == 0 {
                                "扫描完成：没找到可用的本机 shell".into()
                            } else {
                                format!(
                                    "扫描完成：找到 {count} 个本机 shell，在{side}「本地终端」里点一下即可打开"
                                )
                                .into()
                            };
                        }
                    }
                    Err(error) => {
                        log::error!("本机 shell 扫描失败: {error}");
                        if !quiet {
                            this.status = "本机 shell 扫描失败，已保留原有列表".into();
                        }
                    }
                }
                cx.notify();
            });
        })
        .detach();
    }

    /// 给列表里每个 shell 取一次系统图标（同一 exe 只取一次，如各 WSL 发行版共用 wsl.exe）。
    pub(super) fn refresh_shell_icons(&mut self) {
        let programs: Vec<String> = self
            .local_shells
            .iter()
            .map(|profile| profile.program.clone())
            .collect();
        for program in programs {
            if self.shell_icons.contains_key(&program) {
                continue;
            }
            let Some(bytes) = crate::local_shell::icon_png(&program) else {
                continue;
            };
            self.shell_icons.insert(
                program,
                Arc::new(Image::from_bytes(ImageFormat::Png, bytes)),
            );
        }
    }

    /// Ctrl+Shift+T / 菜单项：新开一个本地终端标签。
    pub(super) fn new_local_terminal(
        &mut self,
        _: &NewLocalTerminal,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.open_local_terminal(None, cx);
    }

    /// 主机右键菜单「Ping」/「路由追踪」：在本地 PTY 里跑系统命令。
    ///
    /// 主机名先过白名单再拼命令行：空白 / 引号 / 管道会经 `cmd /k` 变成注入面。
    pub(super) fn open_network_tool(&mut self, host: &str, tool: NetTool, cx: &mut Context<Self>) {
        let Some(host) = crate::local_shell::sanitize_host(host) else {
            self.status = tr!(cx, "网络诊断：主机名含不支持的字符");
            cx.notify();
            return;
        };
        let Some(profile) = crate::local_shell::diagnostic_profile(tool, &host) else {
            self.status = tr!(cx, "网络诊断目前只在 Windows 上提供");
            cx.notify();
            return;
        };
        self.open_local_terminal(Some(&profile), cx);
    }

    /// 打开本地终端（路线图 §五 一期）：交给 rgpui-term 的本地 PTY 后端拉起选定 shell。
    /// 不指定档案时取列表第一项（`detect()` 把 PowerShell 7 排最前）。
    /// 进程拉起与通道握手是异步的：建好才落标签，失败只写状态栏，不留空标签。
    pub(super) fn open_local_terminal(
        &mut self,
        profile: Option<&ShellProfile>,
        cx: &mut Context<Self>,
    ) {
        let chosen = profile
            .cloned()
            .or_else(|| self.local_shells.first().cloned());
        let Some(chosen) = chosen else {
            self.status = "未找到可用的本地 shell".into();
            cx.notify();
            return;
        };
        // WSL 等带参数的档案要拼成整条命令行；后端只收一个 shell 字符串。
        let profile_id = chosen.id();
        let extra_args = self.local_shell_prefs.args_of(&profile_id);
        let program = crate::local_shell::command_line_with(&chosen, &extra_args);
        let env = self.local_shell_prefs.env_map_of(&profile_id);
        let label = self.shell_name(&chosen).to_string();
        // 平台在建标签时就定下来：PowerShell/CMD 是 Windows 命令，WSL 是 Linux，Git Bash 只算 POSIX 工具。
        let platform = Platform::from_shell_kind(chosen.kind);
        let working_directory = self
            .local_shell_prefs
            .cwd_of(&profile_id)
            .map(std::path::PathBuf::from)
            // 目录事后被删/改名时静默回落，不阻断开会话。
            .filter(|dir| dir.is_dir())
            .or_else(|| std::env::current_dir().ok());
        let cwd = working_directory
            .as_ref()
            .map(|dir| dir.display().to_string())
            .unwrap_or_default();
        self.status = SharedString::from(format!("正在启动 {label}…"));
        cx.notify();
        let Some(window_id) = cx.active_window().map(|handle| handle.window_id().as_u64()) else {
            self.status = SharedString::from("启动本地终端失败：找不到当前窗口".to_string());
            cx.notify();
            return;
        };
        let builder =
            TerminalBuilder::new(working_directory, Some(program), env, None, window_id, cx);
        cx.spawn(async move |this, cx| {
            let builder = builder.await;
            let Some(entity) = this.upgrade() else {
                return;
            };
            let entity_id = entity.entity_id();
            let _ = cx.with_window(entity_id, move |window, cx| {
                entity.update(cx, |this, cx| match builder {
                    Ok(builder) => this.add_local_tab(builder, label, cwd, platform, window, cx),
                    Err(error) => {
                        log::error!("启动本地终端失败: {error}");
                        this.status = SharedString::from(format!("启动本地终端失败：{label}"));
                        cx.notify();
                    }
                });
            });
        })
        .detach();
    }

    /// 把建好的本地 PTY 终端落成标签；shell 进程退出即自动移除。
    pub(super) fn add_local_tab(
        &mut self,
        builder: TerminalBuilder,
        label: String,
        cwd: String,
        platform: Option<Platform>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let id = self.next_id;
        self.next_id += 1;
        let terminal = cx.new(|cx| builder.subscribe(cx));
        let mirror_tx = self.mirror_tx.clone();
        terminal.update(cx, |terminal, _| {
            // 与 SSH / Telnet（connect.rs）保持一致：先归一化粘贴行尾再镜像。
            // Windows 剪贴板的多行文本是 CRLF，bracketed-paste 下原样转发会把
            // \r 留在 PowerShell（PSReadLine）的行缓冲里（回显 ^M、续行失效）。
            terminal.add_middleware(Arc::new(NormalizePastedLineEndings));
            terminal.add_middleware(Arc::new(MirrorInput {
                tab_id: id,
                tx: mirror_tx,
            }));
        });
        let style = self.terminal_text_style(cx);
        let terminal_view =
            cx.new(|cx| TerminalView::new_with_style(terminal.clone(), style, window, cx));
        let addr = if cwd.is_empty() {
            label.clone().into()
        } else {
            format!("{label} · {cwd}").into()
        };
        let title_ordinal = self.next_title_ordinal(&label, None);
        self.tabs.push(SessionTab {
            id,
            title: label.clone().into(),
            base_title: label.clone().into(),
            custom_title: None,
            title_ordinal,
            // 标签名固定用 shell 档案名：OSC 标题在 Windows 上常是整个
            // 可执行文件路径（如 C:\Program Files\PowerShell\7\pwsh.exe），
            // 放进标签里过长；工作目录只留在状态栏的 addr 里。
            follow_osc_title: false,
            addr,
            auth: None,
            remote_info: None,
            platform,
            terminal_view: Some(terminal_view),
            desktop: None,
            ended: false,
            failed: false,
            last_error: None,
            resume: None,
            disconnect: None,
            commands: None,
            forwards: Vec::new(),
            files: FileBrowser::default(),
            terminal_cwd: None,
            zmodem: None,
            zmodem_progress: None,
            auto_retry_attempt: 0,
            auto_retry_pending: false,
            connected_at: None,
        });
        self.view_split = None;
        self.active_tab = Some(id);
        self.reveal_tab(id);
        self.sync_tray(cx);
        // 本地 shell 退出（exit / Ctrl+D / 进程被杀）与 SSH 会话结束一样直接收标签。
        cx.subscribe(&terminal, move |this, _, event: &TerminalEvent, cx| {
            if matches!(event, TerminalEvent::CloseTerminal) {
                this.remove_tab(id, cx);
            }
        })
        .detach();
        self.watch_terminal(id, &terminal, cx);
        self.status = SharedString::from(format!("本地终端 {label}"));
        cx.notify();
    }

    /// 设置「终端」分页里的本机 shell 一行：扫描结果计数 + 一键扫描按钮。
    pub(super) fn render_local_shell_row(&self, pal: &Palette, cx: &Context<Self>) -> AnyElement {
        let count = self.local_shells.len();
        let scanning = cx.entity();
        h_flex()
            .gap_2()
            .items_center()
            .child(
                div()
                    .w(px(72.0))
                    .text_sm()
                    .text_color(pal.muted)
                    .child(tr!(cx, "本机 shell")),
            )
            .child(div().text_sm().child(if count == 0 {
                tr!(cx, "未扫描到")
            } else {
                tr!(cx, "已扫到 {count} 个", count = count)
            }))
            .child(
                Button::new("scan-local-shells")
                    .label(if self.scanning_shells {
                        tr!(cx, "扫描中…")
                    } else {
                        tr!(cx, "一键扫描")
                    })
                    .secondary()
                    .outline()
                    .on_click(move |_, _, cx| {
                        scanning.update(cx, |this, cx| this.scan_local_shells(cx));
                    }),
            )
            .into_any_element()
    }

    /// 内置「本地终端」分组：父行折叠/展开，子行是扫描到的每个本机 shell，点一下直接开对应终端。
    /// 它不是凭据库里的主机，所以没有 ★ / 编辑 / 删除，也不参与分组与收藏排序；
    /// 搜索时按「本地终端」与各 shell 名匹配，命中即展开。
    /// 子行图标取该 exe 的系统默认图标（`SHGetFileInfoW`），取不到回退字形。
    pub(super) fn render_local_terminal_rows(
        &self,
        pal: &Palette,
        query: &str,
        cx: &mut Context<Self>,
    ) -> Vec<AnyElement> {
        let entity = cx.entity();
        let searching = !query.is_empty();
        let hit = query.is_empty()
            || "本地终端".contains(query)
            || self
                .local_shells
                .iter()
                .any(|profile| self.shell_matches(profile, query));
        if !hit {
            return Vec::new();
        }
        let count = self.local_shells.len();
        let group_label = tr!(cx, "本地终端");
        let collapsed = !searching && self.sidebar.local_collapsed;
        let parent = Self::row_base("local-terminal-row", pal, px(2.0))
            .font_weight(rgpui::FontWeight(600.0))
            .on_click(cx.listener(|this, _, _, cx| {
                this.sidebar.toggle_local();
                this.persist_sidebar(cx);
                cx.notify();
            }))
            .child(
                Icon::new(if collapsed {
                    IconName::ChevronRight
                } else {
                    IconName::ChevronDown
                })
                .xsmall()
                .text_color(pal.muted),
            )
            .child(
                Icon::new(IconName::SquareTerminal)
                    .small()
                    .text_color(pal.link),
            )
            .child(div().flex_1().min_w_0().text_ellipsis().child(group_label))
            .when(count > 0, |this| {
                this.child(
                    div()
                        .flex_shrink_0()
                        .text_xs()
                        .text_color(pal.muted)
                        .child(count.to_string()),
                )
            })
            .context_menu(move |menu, _, _| {
                let scanning = entity.clone();
                menu.item(
                    PopupMenuItem::new("一键扫描本机 shell…").on_click(move |_, _, cx| {
                        scanning.update(cx, |this, cx| this.scan_local_shells(cx));
                    }),
                )
                .item(PopupMenuItem::new("新建主机…").on_click({
                    let entity = entity.clone();
                    move |_, window, cx| {
                        entity.update(cx, |this, cx| this.open_form(window, cx));
                    }
                }))
                .item(PopupMenuItem::new("新建分组…").on_click({
                    let entity = entity.clone();
                    move |_, window, cx| {
                        entity.update(cx, |this, cx| this.open_group_prompt(None, window, cx));
                    }
                }))
            })
            .into_any_element();
        if collapsed {
            return vec![parent];
        }
        // 下标必须是 `local_shells` 里的真实下标：先 enumerate 再过滤，
        // 否则搜索态下点行会按过滤后的序号打开另一台 shell。
        let rows: Vec<AnyElement> = self
            .local_shells
            .iter()
            .enumerate()
            .filter(|(_, profile)| self.shell_matches(profile, query))
            .map(|(index, profile)| self.render_shell_row(index, profile, pal, cx))
            .collect();
        std::iter::once(parent).chain(rows).collect()
    }

    /// 「本地终端」分组下的一台 shell：系统图标 + 展示名，点开即开该 shell 的终端标签。
    pub(super) fn render_shell_row(
        &self,
        index: usize,
        profile: &ShellProfile,
        pal: &Palette,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let entity = cx.entity();
        let name = self.shell_name(profile);
        let profile_id = profile.id();
        let renamed = self.local_shell_prefs.name_of(&profile_id).is_some();
        let custom_cwd = self.local_shell_prefs.cwd_of(&profile_id).is_some();
        let custom_args = !self.local_shell_prefs.args_of(&profile_id).is_empty();
        let custom_env = !self.local_shell_prefs.env_of(&profile_id).is_empty();
        let system_name = profile.name.clone();
        let icon: AnyElement = match self.shell_icons.get(&profile.program).cloned() {
            Some(image) => img(image)
                .size_4()
                .flex_shrink_0()
                .text_color(pal.text)
                .into_any_element(),
            None => Icon::new(IconName::SquareTerminal)
                .small()
                .text_color(pal.muted)
                .into_any_element(),
        };
        Self::row_base(
            SharedString::from(format!("local-shell-row-{index}")),
            pal,
            px(22.0),
        )
        .on_click(cx.listener(move |this, _, _, cx| {
            let chosen = this.local_shells.get(index).cloned();
            this.open_local_terminal(chosen.as_ref(), cx);
        }))
        .child(icon)
        .child(div().flex_1().min_w_0().text_ellipsis().child(name.clone()))
        .context_menu(move |menu, _, _| {
            let opening = entity.clone();
            let menu = menu.item(PopupMenuItem::new(format!("打开 {name}")).on_click(
                move |_, _, cx| {
                    opening.update(cx, |this, cx| {
                        let chosen = this.local_shells.get(index).cloned();
                        this.open_local_terminal(chosen.as_ref(), cx);
                    });
                },
            ));
            let renaming = entity.clone();
            let menu = menu.item(
                PopupMenuItem::new("重命名…").on_click(move |_, window, cx| {
                    renaming.update(cx, |this, cx| {
                        let Some(profile) = this.local_shells.get(index).cloned() else {
                            return;
                        };
                        this.open_shell_prompt(&profile, window, cx);
                    });
                }),
            );
            let choosing_cwd = entity.clone();
            let menu = menu.item(
                PopupMenuItem::new("起始目录…").on_click(move |_, window, cx| {
                    choosing_cwd.update(cx, |this, cx| {
                        let Some(profile) = this.local_shells.get(index).cloned() else {
                            return;
                        };
                        this.open_shell_cwd_prompt(&profile, window, cx);
                    });
                }),
            );
            let editing_args = entity.clone();
            let menu = menu.item(
                PopupMenuItem::new("启动参数…").on_click(move |_, window, cx| {
                    editing_args.update(cx, |this, cx| {
                        let Some(profile) = this.local_shells.get(index).cloned() else {
                            return;
                        };
                        this.open_shell_args_prompt(&profile, window, cx);
                    });
                }),
            );
            let editing_env = entity.clone();
            let menu = menu.item(
                PopupMenuItem::new("环境变量…").on_click(move |_, window, cx| {
                    editing_env.update(cx, |this, cx| {
                        let Some(profile) = this.local_shells.get(index).cloned() else {
                            return;
                        };
                        this.open_shell_env_prompt(&profile, window, cx);
                    });
                }),
            );
            let menu = if custom_cwd {
                let resetting = entity.clone();
                menu.item(
                    PopupMenuItem::new("恢复默认起始目录").on_click(move |_, _, cx| {
                        resetting.update(cx, |this, cx| {
                            let Some(profile) = this.local_shells.get(index).cloned() else {
                                return;
                            };
                            this.reset_shell_cwd(&profile, cx);
                        });
                    }),
                )
            } else {
                menu
            };
            let menu = if custom_args {
                let resetting = entity.clone();
                menu.item(
                    PopupMenuItem::new("清除启动参数").on_click(move |_, _, cx| {
                        resetting.update(cx, |this, cx| {
                            let Some(profile) = this.local_shells.get(index).cloned() else {
                                return;
                            };
                            this.reset_shell_args(&profile, cx);
                        });
                    }),
                )
            } else {
                menu
            };
            let menu = if custom_env {
                let resetting = entity.clone();
                menu.item(
                    PopupMenuItem::new("清除环境变量").on_click(move |_, _, cx| {
                        resetting.update(cx, |this, cx| {
                            let Some(profile) = this.local_shells.get(index).cloned() else {
                                return;
                            };
                            this.reset_shell_env(&profile, cx);
                        });
                    }),
                )
            } else {
                menu
            };
            if renamed {
                let resetting = entity.clone();
                menu.item(
                    PopupMenuItem::new(format!("恢复默认名称（{system_name}）")).on_click(
                        move |_, _, cx| {
                            resetting.update(cx, |this, cx| {
                                let Some(profile) = this.local_shells.get(index).cloned() else {
                                    return;
                                };
                                this.reset_shell_name(&profile, cx);
                            });
                        },
                    ),
                )
            } else {
                menu
            }
        })
        .into_any_element()
    }

    /// 本地 shell 重命名弹窗：与分组重命名同款居中模态。
    pub(super) fn render_shell_prompt(&self, cx: &Context<Self>) -> AnyElement {
        let pal = self.palette(cx);
        let Some(prompt) = self.shell_prompt.as_ref() else {
            return div().into_any_element();
        };
        let default_text = prompt.default_text.clone();
        let (title, field_label, hint) = match prompt.kind {
            ShellPromptKind::Rename => (
                "重命名本地终端",
                "显示名称",
                format!(
                    "只改侧边栏与标签上的叫法，不影响启动的 shell 本身；留空恢复默认（{default_text}）。"
                ),
            ),
            ShellPromptKind::WorkingDirectory => (
                "起始目录",
                "工作目录",
                format!(
                    "新会话在此目录下启动；留空或保持默认（{default_text}）时用进程当前目录。\
                     目录不存在时不会保存。"
                ),
            ),
            ShellPromptKind::Arguments => (
                "启动参数",
                "参数",
                "附加在 shell 可执行文件后的参数，空格分隔，含空格的路径用双引号包住；\
                 留空＝不加参数。WSL 的 -d 发行版由应用自动拼接，不必重复填写。"
                    .to_string(),
            ),
            ShellPromptKind::Environment => (
                "环境变量",
                "变量（每行一个 KEY=VALUE）",
                "空行与 # 开头的注释忽略；值里可以有 =。留空＝不注入额外变量。\
                 回车换行，点「保存」提交。"
                    .to_string(),
            ),
        };
        let width = if prompt.kind.multi_line() {
            460.0
        } else {
            380.0
        };
        let input_row = h_flex()
            .w_full()
            .gap_2()
            .items_center()
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .child(styled_input(Input::new(&prompt.input), &pal)),
            )
            .when(prompt.kind == ShellPromptKind::WorkingDirectory, |row| {
                row.child(
                    Button::new("browse-shell-cwd")
                        .label("浏览…")
                        .ghost()
                        .on_click(cx.listener(|this, _, _, cx| {
                            this.browse_shell_cwd(cx);
                        })),
                )
            });
        v_flex()
            .id("shell-backdrop")
            .absolute()
            .top(px(0.0))
            .left(px(0.0))
            .size_full()
            .items_center()
            .justify_center()
            .bg(rgpui::hsla(0.0, 0.0, 0.0, 0.45))
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(|_, _, _, cx| cx.stop_propagation()),
            )
            .child(
                v_flex()
                    .id("shell-form")
                    .w(px(width))
                    .gap_3()
                    .p_5()
                    .rounded(px(8.0))
                    .bg(pal.card_bg)
                    .border_1()
                    .border_color(pal.border)
                    .shadow_md()
                    .on_mouse_down(
                        MouseButton::Left,
                        cx.listener(|_, _, _, cx| cx.stop_propagation()),
                    )
                    .child(
                        h_flex()
                            .w_full()
                            .items_center()
                            .justify_between()
                            .child(
                                div()
                                    .text_base()
                                    .font_weight(rgpui::FontWeight(600.0))
                                    .child(title),
                            )
                            .child(Button::new("close-shell").label("×").ghost().on_click(
                                cx.listener(|this, _, _, cx| {
                                    this.cancel_shell_prompt(cx);
                                }),
                            )),
                    )
                    .child(
                        v_flex()
                            .gap_1()
                            .child(div().text_sm().child(field_label))
                            .child(input_row),
                    )
                    .child(div().text_xs().text_color(pal.muted).child(hint))
                    .child(
                        h_flex()
                            .gap_2()
                            .justify_end()
                            .child(
                                Button::new("commit-shell")
                                    .label("保存")
                                    .primary()
                                    .on_click(cx.listener(|this, _, _, cx| {
                                        this.commit_shell_prompt(cx);
                                    })),
                            )
                            .child(Button::new("cancel-shell").label("取消").ghost().on_click(
                                cx.listener(|this, _, _, cx| {
                                    this.cancel_shell_prompt(cx);
                                }),
                            )),
                    ),
            )
            .into_any_element()
    }
}

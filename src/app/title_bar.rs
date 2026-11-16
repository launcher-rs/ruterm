//! //! 自绘标题栏：窗口控制按钮、品牌段、多级菜单（会话 / 凭据 / 外观 / 帮助）。

use rgpui::prelude::*;
use rgpui::title_bar::TITLE_BAR_HEIGHT;
use rgpui::{
    AnyElement, Button, ButtonVariants, Context, DropdownMenu, FontWeight, Hsla, Icon, IconName,
    InteractiveElement, IntoElement, ParentElement, PopupMenu, PopupMenuItem, SharedString,
    StatefulInteractiveElement, Styled, Window, WindowControlArea, div, h_flex, px,
};

use crate::CommandHistory as OpenCommandHistory;
use crate::MacroManager as OpenMacroManager;
use crate::SnippetManager as OpenSnippetManager;
use crate::tr;
use crate::{
    BroadcastCommand, CloseSplit, CloseTab, FindInTerminal, LockVault, NewHost, NewLocalTerminal,
    NewRemoteDesktop, NextTab, OpenForward, OpenSettings, PrevTab, SplitDown, SplitRight,
    ToggleFilePanel, ToggleLocalPanel, ToggleSidebar, ToggleSyncInput, ToggleTheme,
};

use super::*;

impl RuTermApp {
    /// 标题栏右侧的窗口控制按钮：登记为 `WindowControlArea`，交给系统
    /// HTMINBUTTON / HTMAXBUTTON / HTCLOSE（关闭仍会走 `on_window_should_close`，
    /// 即隐藏到托盘）。
    #[allow(clippy::too_many_arguments)]
    pub(super) fn window_control(
        id: &'static str,
        icon: IconName,
        area: WindowControlArea,
        fg: Hsla,
        hover_bg: Hsla,
        active_bg: Hsla,
        hover_fg: Hsla,
    ) -> AnyElement {
        div()
            .id(id)
            .flex()
            .w(TITLE_BAR_HEIGHT)
            .h_full()
            .flex_shrink_0()
            .justify_center()
            .content_center()
            .items_center()
            .text_color(fg)
            .hover(|style| style.bg(hover_bg).text_color(hover_fg))
            .active(|style| style.bg(active_bg).text_color(hover_fg))
            .window_control_area(area)
            .child(Icon::new(icon).small())
            .into_any_element()
    }

    /// 自定义标题栏：品牌 + 菜单（会话 / 凭据 / 外观 / 帮助）+ 快捷按钮 + 窗口控制。
    ///
    /// 这里不套框架的 `TitleBar`：它把 children 整体放进 `WindowControlArea::Drag`
    /// 命中区，而 Windows 的 WM_NCHITTEST 只取**第一个**命中的控制区，父级 Drag 先入表，
    /// 栏内按钮一律被判定为 caption，点击被系统吃掉（hover 仍在、click 永不触发）。
    /// 改成把拖拽区拆成"品牌段"与"中间空档"两个兄弟节点，交互控件落在 Drag 区之外。
    pub(super) fn render_title_bar(
        &self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let pal = self.palette(cx);
        let bar_bg = cx.theme().tokens.title_bar;
        let bar_border = cx.theme().title_bar_border;
        let fg = cx.theme().foreground;
        let plain_hover_bg = cx.theme().secondary_hover;
        let plain_active_bg = cx.theme().secondary_active;
        let plain_hover_fg = cx.theme().secondary_foreground;
        let close_hover_bg = cx.theme().danger;
        let close_active_bg = cx.theme().danger_active;
        let close_hover_fg = cx.theme().danger_foreground;
        let dark = cx.theme().is_dark();
        let maximized = window.is_maximized();
        // 只有口令档（已设置主密码）才有"锁定/解锁"概念，keyring 档免密无锁可上。
        let passphrase_mode =
            crate::vault::read_unlock_marker(&self.mode) == crate::vault::UnlockBackend::Passphrase;
        let locked = passphrase_mode && self.vault.is_none();
        let vault_state = if !passphrase_mode {
            tr!(cx, "免主密码（系统密钥环）")
        } else if locked {
            tr!(cx, "主密码档 · 已锁定")
        } else {
            tr!(cx, "主密码档 · 已解锁")
        };
        let entity = cx.entity();

        // 会话菜单：新建主机、本地终端、标签切换、侧栏折叠、设置入口、托盘与退出。
        // 前六项挂 action（而非 on_click）：rgpui 会按键绑定在项右侧自动显示
        // Ctrl+N / Ctrl+Shift+T / Ctrl+Tab / Ctrl+,。
        let split_open = self.view_split.is_some();
        let sync_on = self.sync_input;
        let live_tabs = self.live_tab_count();
        let any_tab = !self.tabs.is_empty();
        let sidebar_open = self.sidebar_visible(cx);
        let hosts_open = sidebar_open && self.sidebar_page == SidebarPage::Hosts;
        let file_panel_open = sidebar_open && self.sidebar_page == SidebarPage::Files;
        let local_panel_open = sidebar_open && self.sidebar_page == SidebarPage::Local;
        let history_panel_open = self.history_panel_open;
        let snippets_panel_open = self.snippets_panel_open;
        // 菜单打开时把最近连接的 id 解析成当前列表里的条目（主机可能已删除或重排）。
        let recent_items: Vec<(usize, SharedString)> = self
            .recent_hosts
            .iter()
            .filter_map(|id| {
                let index = self.hosts.iter().position(|host| host.id == *id)?;
                let host = &self.hosts[index];
                Some((
                    index,
                    tr!(
                        cx,
                        "{name}（{addr}）",
                        name = host.name.clone(),
                        addr = host.addr()
                    ),
                ))
            })
            .collect();
        let menu_entity = entity.clone();
        // 「会话」只留主机与窗口收场：面板开关 / 分屏 / 标签进「视图」，
        // 终端里的操作（查找、同步、广播、键宏、转发）进「终端」。
        let session_menu = move |menu: PopupMenu,
                                 window: &mut Window,
                                 cx: &mut Context<PopupMenu>| {
            let importing = menu_entity.clone();
            let importing_json = menu_entity.clone();
            let exporting_json = menu_entity.clone();
            let mut menu = menu
                .item(PopupMenuItem::new(tr!(cx, "新建主机…")).action(Box::new(NewHost)))
                .item(PopupMenuItem::new(tr!(cx, "导入 ~/.ssh/config…")).on_click(
                    move |_, _, cx| {
                        importing.update(cx, |this, cx| this.import_ssh_config(cx));
                    },
                ))
                .item(
                    PopupMenuItem::new(tr!(cx, "导入主机配置 JSON…"))
                        .disabled(locked)
                        .on_click(move |_, _, cx| {
                            importing_json.update(cx, |this, cx| this.import_host_configs(cx));
                        }),
                )
                .item(
                    PopupMenuItem::new(tr!(cx, "导出主机配置 JSON…"))
                        .disabled(locked)
                        .on_click(move |_, _, cx| {
                            exporting_json.update(cx, |this, cx| this.export_host_configs(cx));
                        }),
                );
            // 最近连接收进二级菜单：主菜单不再被历史条目撑长。
            if !recent_items.is_empty() {
                let sub_entity = menu_entity.clone();
                let sub_items = recent_items.clone();
                menu = menu.submenu(tr!(cx, "最近连接"), window, cx, move |menu, _, _| {
                    let entity = sub_entity.clone();
                    // 构建器会被反复调用（菜单重建），所以每次都要拿一份副本。
                    sub_items
                        .clone()
                        .into_iter()
                        .fold(menu, |menu, (index, label)| {
                            let entity = entity.clone();
                            menu.item(PopupMenuItem::new(label).on_click(move |_, window, cx| {
                                entity.update(cx, |this, cx| {
                                    this.click_host(index, window, cx);
                                });
                            }))
                        })
                });
            }
            let menu = menu
                .item(PopupMenuItem::new(tr!(cx, "本地终端…")).action(Box::new(NewLocalTerminal)))
                .item(PopupMenuItem::new(tr!(cx, "远程桌面…")).action(Box::new(NewRemoteDesktop)))
                .item(PopupMenuItem::new(tr!(cx, "局域网发现…")).on_click({
                    let entity = menu_entity.clone();
                    move |_, _, cx| {
                        entity.update(cx, |this, cx| this.open_discovery(cx));
                    }
                }))
                .separator()
                .item(PopupMenuItem::new(tr!(cx, "设置…")).action(Box::new(OpenSettings)));
            // 托盘关闭时不给「隐藏到托盘」：窗口藏起来就再也点不回来。
            let menu = if crate::tray_enabled() {
                menu.item(PopupMenuItem::new(tr!(cx, "隐藏到托盘")).on_click(
                    move |_, window, _| {
                        window.hide_window();
                    },
                ))
            } else {
                menu
            };
            menu.item(
                PopupMenuItem::new(tr!(cx, "退出 RuTerm"))
                    .danger(true)
                    .on_click(move |_, _, cx| cx.quit()),
            )
        };

        let view_entity = entity.clone();
        let view_menu = move |menu: PopupMenu, _: &mut Window, cx: &mut Context<PopupMenu>| {
            let menu = menu
                .item(
                    PopupMenuItem::new(tr!(cx, "主机列表"))
                        .checked(hosts_open)
                        .on_click({
                            let entity = view_entity.clone();
                            move |_, window, cx| {
                                entity.update(cx, |this, cx| {
                                    this.set_sidebar_page(SidebarPage::Hosts, window, cx);
                                });
                            }
                        }),
                )
                .item(
                    PopupMenuItem::new(tr!(cx, "文件浏览器"))
                        .checked(file_panel_open)
                        .action(Box::new(ToggleFilePanel)),
                )
                .item(
                    PopupMenuItem::new(tr!(cx, "本机文件页"))
                        .checked(local_panel_open)
                        .action(Box::new(ToggleLocalPanel)),
                )
                .item(
                    PopupMenuItem::new(tr!(cx, "命令片段"))
                        .checked(snippets_panel_open)
                        .on_click({
                            let entity = view_entity.clone();
                            move |_, _, cx| {
                                entity.update(cx, |this, cx| {
                                    this.toggle_snippets_panel(cx);
                                });
                            }
                        }),
                )
                .item(
                    PopupMenuItem::new(tr!(cx, "命令历史"))
                        .checked(history_panel_open)
                        .on_click({
                            let entity = view_entity.clone();
                            move |_, _, cx| {
                                entity.update(cx, |this, cx| {
                                    this.toggle_history_panel(cx);
                                });
                            }
                        }),
                )
                // 只折叠面板，最外缘的图标栏始终留着（RustRover 的工具窗口栏）。
                .item(
                    PopupMenuItem::new(tr!(cx, "折叠 / 展开侧栏面板"))
                        .action(Box::new(ToggleSidebar)),
                )
                .separator()
                .item(PopupMenuItem::new(tr!(cx, "左右分屏")).action(Box::new(SplitRight)))
                .item(PopupMenuItem::new(tr!(cx, "上下分屏")).action(Box::new(SplitDown)));
            let menu = if split_open {
                menu.item(PopupMenuItem::new(tr!(cx, "取消分屏")).action(Box::new(CloseSplit)))
            } else {
                menu
            };
            menu.separator()
                .item(PopupMenuItem::new(tr!(cx, "下一个标签")).action(Box::new(NextTab)))
                .item(PopupMenuItem::new(tr!(cx, "上一个标签")).action(Box::new(PrevTab)))
                .item(
                    PopupMenuItem::new(tr!(cx, "关闭标签"))
                        .disabled(!any_tab)
                        .action(Box::new(CloseTab)),
                )
        };

        let terminal_menu = move |menu: PopupMenu, _: &mut Window, cx: &mut Context<PopupMenu>| {
            menu.item(PopupMenuItem::new(tr!(cx, "终端内查找…")).action(Box::new(FindInTerminal)))
                .item(
                    PopupMenuItem::new(tr!(cx, "同步输入"))
                        .checked(sync_on)
                        .disabled(live_tabs < 2)
                        .action(Box::new(ToggleSyncInput)),
                )
                .item(
                    PopupMenuItem::new(tr!(cx, "命令发到所有会话…"))
                        .disabled(live_tabs == 0)
                        .action(Box::new(BroadcastCommand)),
                )
                .item(PopupMenuItem::new(tr!(cx, "键宏…")).action(Box::new(OpenMacroManager)))
                .item(PopupMenuItem::new(tr!(cx, "命令片段…")).action(Box::new(OpenSnippetManager)))
                .item(PopupMenuItem::new(tr!(cx, "命令历史…")).action(Box::new(OpenCommandHistory)))
                .item(PopupMenuItem::new(tr!(cx, "端口转发…")).action(Box::new(OpenForward)))
        };

        // 凭据菜单：解锁 / 主密码设置 / 锁定，末尾附当前档位（只读）。
        let unlock = entity.clone();
        let settings = entity.clone();
        let vault_menu = move |menu: PopupMenu, _: &mut Window, cx: &mut Context<PopupMenu>| {
            let unlock = unlock.clone();
            let settings = settings.clone();
            let menu = if locked {
                menu.item(PopupMenuItem::new(tr!(cx, "解锁凭据库…")).on_click(
                    move |_, window, cx| {
                        unlock.update(cx, |this, cx| this.open_unlock_dialog(window, cx));
                    },
                ))
            } else {
                menu.item(PopupMenuItem::new(tr!(cx, "主密码与凭据库设置…")).on_click(
                    move |_, window, cx| {
                        settings.update(cx, |this, cx| {
                            this.open_settings_at(SETTINGS_TAB_VAULT, window, cx);
                        });
                    },
                ))
            };
            let menu = if passphrase_mode && !locked {
                menu.item(PopupMenuItem::new(tr!(cx, "锁定凭据库")).action(Box::new(LockVault)))
            } else {
                menu
            };
            menu.separator()
                .item(PopupMenuItem::new(vault_state.clone()).disabled(true))
        };

        // 外观菜单：明暗二选一（当前项打勾）+ 语言二选一，外加设置面板的
        // 外观 / 状态栏 / 终端分页。语言项显示各语言自署名，不跟着翻译。
        let to_dark = entity.clone();
        let to_light = entity.clone();
        let appearance = entity.clone();
        let status = entity.clone();
        let terminal = entity.clone();
        let locale = crate::i18n::current_locale(cx);
        let appearance_menu =
            move |menu: PopupMenu, _: &mut Window, cx: &mut Context<PopupMenu>| {
                let to_dark = to_dark.clone();
                let to_light = to_light.clone();
                let appearance = appearance.clone();
                let status = status.clone();
                let terminal = terminal.clone();
                let menu = menu
                    .item(
                        PopupMenuItem::new(tr!(cx, "暗色主题"))
                            .checked(dark)
                            .on_click(move |_, window, cx| {
                                to_dark.update(cx, |this, cx| this.apply_theme(true, window, cx));
                            }),
                    )
                    .item(
                        PopupMenuItem::new(tr!(cx, "亮色主题"))
                            .checked(!dark)
                            .on_click(move |_, window, cx| {
                                to_light.update(cx, |this, cx| this.apply_theme(false, window, cx));
                            }),
                    )
                    // 明暗二选一是"设为"，这条是"翻转"，与 Ctrl+T 同义，故挂 action 显示按键。
                    .item(PopupMenuItem::new(tr!(cx, "切换明暗主题")).action(Box::new(ToggleTheme)))
                    .separator();
                let menu = crate::i18n::LANGUAGES
                    .iter()
                    .fold(menu, |menu, (code, native_name)| {
                        let entity = appearance.clone();
                        menu.item(
                            PopupMenuItem::new(*native_name)
                                .checked(*code == locale)
                                .on_click(move |_, _, cx| {
                                    entity.update(cx, |this, cx| this.apply_language(code, cx));
                                }),
                        )
                    });
                menu.separator()
                    .item(PopupMenuItem::new(tr!(cx, "外观设置…")).on_click(
                        move |_, window, cx| {
                            appearance.update(cx, |this, cx| {
                                this.open_settings_at(SETTINGS_TAB_APPEARANCE, window, cx);
                            });
                        },
                    ))
                    .item(PopupMenuItem::new(tr!(cx, "状态栏显示项…")).on_click(
                        move |_, window, cx| {
                            status.update(cx, |this, cx| {
                                this.open_settings_at(SETTINGS_TAB_STATUS, window, cx);
                            });
                        },
                    ))
                    .item(PopupMenuItem::new(tr!(cx, "终端外观…")).on_click(
                        move |_, window, cx| {
                            terminal.update(cx, |this, cx| {
                                this.open_settings_at(SETTINGS_TAB_TERMINAL, window, cx);
                            });
                        },
                    ))
            };

        // 帮助菜单只留版本信息：快捷键已随各菜单项右侧显示，不再单列一份文字清单。
        let help_menu = move |menu: PopupMenu, _: &mut Window, cx: &mut Context<PopupMenu>| {
            menu.label("RuTerm").item(
                PopupMenuItem::new(tr!(
                    cx,
                    "版本 {version}",
                    version = env!("CARGO_PKG_VERSION")
                ))
                .disabled(true),
            )
        };

        let menu_button = |id: &'static str, label: SharedString| {
            Button::new(id)
                .label(label)
                .compact()
                .ghost()
                .text_color(fg)
        };

        h_flex()
            .id("title-bar")
            .flex_shrink_0()
            .w_full()
            .h(TITLE_BAR_HEIGHT)
            .items_center()
            .pl(px(12.0))
            .border_b_1()
            .border_color(bar_border)
            .bg(bar_bg)
            .text_color(fg)
            // 可拖动段一：品牌与锁定提示。
            .child(
                h_flex()
                    .id("titlebar-brand")
                    .h_full()
                    .items_center()
                    .gap_2()
                    .pr_3()
                    .flex_shrink_0()
                    .window_control_area(WindowControlArea::Drag)
                    .child(
                        div()
                            .text_base()
                            .font_weight(FontWeight(700.0))
                            .child("RuTerm"),
                    )
                    .when(locked, |this| {
                        this.child(
                            div()
                                .text_sm()
                                .text_color(pal.muted)
                                .child(tr!(cx, "已锁定")),
                        )
                    }),
            )
            // 菜单区：交互控件，必须落在 Drag 命中区之外。
            .child(
                h_flex()
                    .items_center()
                    .gap_1()
                    .flex_shrink_0()
                    .child(menu_button("menu-session", tr!(cx, "会话")).dropdown_menu(session_menu))
                    .child(menu_button("menu-view", tr!(cx, "视图")).dropdown_menu(view_menu))
                    .child(
                        menu_button("menu-terminal", tr!(cx, "终端")).dropdown_menu(terminal_menu),
                    )
                    .child(menu_button("menu-vault", tr!(cx, "凭据")).dropdown_menu(vault_menu))
                    .child(
                        menu_button("menu-appearance", tr!(cx, "外观"))
                            .dropdown_menu(appearance_menu),
                    )
                    .child(menu_button("menu-help", tr!(cx, "帮助")).dropdown_menu(help_menu)),
            )
            // 可拖动段二：菜单与右侧按钮之间的空档（会话标签在下方标签栏，不占标题栏）。
            .child(
                div()
                    .id("titlebar-gap")
                    .flex_1()
                    .min_w_0()
                    .h_full()
                    .window_control_area(WindowControlArea::Drag),
            )
            // 右侧：锁定入口 + 细分隔线 + 系统窗口按钮。
            // 文件 / 转发的入口在边栏的竖向导航里，标题栏只留全局操作。
            .child(
                h_flex()
                    .items_center()
                    .gap_1()
                    .flex_shrink_0()
                    .pr_3()
                    // 已设主密码才出现锁定按钮：锁定后需重新输入口令才能用库。
                    .when(passphrase_mode && !locked, |this| {
                        this.child(
                            Button::new("lock-vault")
                                .label("🔒")
                                .compact()
                                .ghost()
                                .text_color(fg)
                                .tooltip(tr!(cx, "锁定凭据库（需主密码解锁）"))
                                .on_click(cx.listener(|this, _, window, cx| {
                                    this.lock_vault(window, cx);
                                })),
                        )
                    }),
            )
            // 内容按钮与窗口按钮之间的细分隔线，明确"这是两条不同的按钮带"。
            .child(div().w(px(1.0)).h(px(16.0)).flex_shrink_0().bg(bar_border))
            .child(Self::window_control(
                "win-minimize",
                IconName::WindowMinimize,
                WindowControlArea::Min,
                fg,
                plain_hover_bg,
                plain_active_bg,
                plain_hover_fg,
            ))
            .child(Self::window_control(
                "win-maximize",
                if maximized {
                    IconName::WindowRestore
                } else {
                    IconName::WindowMaximize
                },
                WindowControlArea::Max,
                fg,
                plain_hover_bg,
                plain_active_bg,
                plain_hover_fg,
            ))
            .child(Self::window_control(
                "win-close",
                IconName::WindowClose,
                WindowControlArea::Close,
                fg,
                close_hover_bg,
                close_active_bg,
                close_hover_fg,
            ))
            .into_any_element()
    }
}

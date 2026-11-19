//! //! 设置弹窗：分页导航与各分页（外观 / 状态栏 / 终端 / 会话）渲染，会话偏好持久化。

use rgpui::components::Select;
use rgpui::input_ui::{Input, InputEvent, InputState};
use rgpui::prelude::*;
use rgpui::{
    AnyElement, Button, ButtonVariants, Checkbox, Context, Entity, FontWeight, InteractiveElement,
    IntoElement, MouseButton, ParentElement, ScrollableElement, SharedString,
    StatefulInteractiveElement, Styled, Window, div, h_flex, px, v_flex,
};

use crate::OpenSettings;
use crate::remote::Metric;
use crate::tr;

use super::*;

/// 设置弹窗状态：主密码表单（含当前口令校验）与应用偏好。
pub(super) struct SettingsState {
    /// 当前分页（见 `SETTINGS_TAB_*`）。
    pub(super) tab: usize,
    /// 已是口令档时要求的"当前主密码"——应用可能在托盘里挂很久，
    /// 不验证就能换密钥等于没有锁。库未解锁时不显示。
    pub(super) current: Option<Entity<InputState>>,
    /// 新口令两栏：提交成功后连同输入一并丢弃，明文不留存界面里；
    /// 需要再改一次时点"更改主密码"重建输入框。
    pub(super) password: Option<Entity<InputState>>,
    pub(super) confirm: Option<Entity<InputState>>,
    pub(super) error: Option<SharedString>,
    pub(super) info: Option<SharedString>,
    /// 搬迁进行中：输入框与保存按钮禁用，避免重复触发。
    pub(super) busy: bool,
}

/// 主密码操作：设置 / 更换为口令派生密钥，或取消并换回系统凭据管理器密钥。
#[derive(Clone, Copy, PartialEq, Eq)]
pub(super) enum MasterPasswordAction {
    Set,
    Cancel,
}

/// 设置弹窗分页：凭据库与主密码。
pub(super) const SETTINGS_TAB_VAULT: usize = 0;
/// 设置弹窗分页：外观。
pub(super) const SETTINGS_TAB_APPEARANCE: usize = 1;
/// 设置弹窗分页：状态栏显示项。
pub(super) const SETTINGS_TAB_STATUS: usize = 2;
/// 设置弹窗分页：终端外观（字号 / 行高 / 配色方案）。
pub(super) const SETTINGS_TAB_TERMINAL: usize = 3;
/// 设置弹窗分页：会话行为（自动重连 / 连接超时）。
pub(super) const SETTINGS_TAB_SESSION: usize = 4;

/// 设置弹窗固定宽高：切分页不重排，弹窗不随内容忽大忽小。
pub(super) const SETTINGS_DIALOG_W: f32 = 640.0;
pub(super) const SETTINGS_DIALOG_H: f32 = 470.0;
/// 内容区高度：弹窗高 − 上下内边距（20×2）− 标题行 − 两条间距 − 底部按钮行。
/// 超出部分由内容区自身滚动，不再把弹窗撑高。
pub(super) const SETTINGS_CONTENT_H: f32 = 352.0;
/// 左侧分页导航宽度。
pub(super) const SETTINGS_NAV_W: f32 = 148.0;

impl RuTermApp {
    /// 打开设置弹窗：主密码管理与主题偏好（主密码相关操作集中在设置里，
    /// 侧边栏只留入口）。
    pub(super) fn open_settings(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.open_settings_at(SETTINGS_TAB_VAULT, window, cx);
    }

    /// 打开设置弹窗并直接停在指定分页（标题栏菜单按入口意图分页）。
    pub(super) fn open_settings_at(
        &mut self,
        tab: usize,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        // 库内终端偏好可能在上次打开之后才到达，先把自定义色回填到输入框。
        self.sync_custom_color_inputs(window, cx);
        if let Some(state) = self.settings.as_mut() {
            // 已开着：只换分页（搬迁途中锁住分页，避免切走看不到结果）。
            if !state.busy {
                state.tab = tab;
                cx.notify();
            }
            return;
        }
        let passphrase_mode =
            crate::vault::read_unlock_marker(&self.mode) == crate::vault::UnlockBackend::Passphrase;
        // 已是口令档且处于解锁状态：换密钥必须先证明知道当前主密码。
        let needs_current = passphrase_mode && self.vault.is_some();
        let current = needs_current
            .then(|| cx.new(|cx| InputState::new(window, cx).placeholder(tr!(cx, "当前主密码"))));
        let password = cx.new(|cx| {
            InputState::new(window, cx).placeholder(crate::i18n::translate(
                cx,
                password_placeholder(passphrase_mode),
                &[],
            ))
        });
        let confirm = cx.new(|cx| InputState::new(window, cx).placeholder(tr!(cx, "再次输入确认")));
        // 任一输入框回车即保存，不必再点按钮。
        for input in [&password, &confirm].into_iter().chain(current.iter()) {
            cx.subscribe(input, |this, _, event: &InputEvent, cx| {
                if matches!(event, InputEvent::PressEnter { shift: false, .. }) {
                    this.apply_rekey(cx);
                }
            })
            .detach();
        }
        current
            .as_ref()
            .unwrap_or(&password)
            .update(cx, |state, cx| state.focus(window, cx));
        self.settings = Some(SettingsState {
            tab,
            current,
            password: Some(password),
            confirm: Some(confirm),
            error: None,
            info: None,
            busy: false,
        });
        cx.notify();
    }

    /// 切换设置弹窗分页；主密码搬迁途中锁住分页，免得用户切走看不到结果。
    pub(super) fn select_settings_tab(&mut self, ix: usize, cx: &mut Context<Self>) {
        let Some(state) = self.settings.as_mut() else {
            return;
        };
        if state.busy || state.tab == ix {
            return;
        }
        state.tab = ix;
        cx.notify();
    }

    /// 重建口令表单（成功回显后输入框已销毁，"更改主密码"再次开启）。
    pub(super) fn reopen_password_form(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self
            .settings
            .as_ref()
            .is_some_and(|state| state.password.is_some())
        {
            return;
        }
        let passphrase_mode =
            crate::vault::read_unlock_marker(&self.mode) == crate::vault::UnlockBackend::Passphrase;
        let current = passphrase_mode
            .then(|| cx.new(|cx| InputState::new(window, cx).placeholder(tr!(cx, "当前主密码"))));
        let password = cx.new(|cx| {
            InputState::new(window, cx).placeholder(crate::i18n::translate(
                cx,
                password_placeholder(passphrase_mode),
                &[],
            ))
        });
        let confirm = cx.new(|cx| InputState::new(window, cx).placeholder(tr!(cx, "再次输入确认")));
        for input in [&password, &confirm].into_iter().chain(current.iter()) {
            cx.subscribe(input, |this, _, event: &InputEvent, cx| {
                if matches!(event, InputEvent::PressEnter { shift: false, .. }) {
                    this.apply_rekey(cx);
                }
            })
            .detach();
        }
        let focus = current.as_ref().unwrap_or(&password).clone();
        if let Some(state) = self.settings.as_mut() {
            state.error = None;
            state.info = None;
            state.current = current;
            state.password = Some(password);
            state.confirm = Some(confirm);
        }
        focus.update(cx, |field, cx| field.focus(window, cx));
        cx.notify();
    }

    /// 关闭设置弹窗（搬迁进行中不许关闭，避免半途中止留下半套文件）。
    pub(super) fn close_settings(&mut self, cx: &mut Context<Self>) {
        if self.settings.as_ref().is_some_and(|state| state.busy) {
            return;
        }
        self.settings = None;
        cx.notify();
    }

    /// Ctrl+, / 菜单项与 ⚙：打开设置弹窗。
    pub(super) fn open_settings_action(
        &mut self,
        _: &OpenSettings,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.open_settings(window, cx);
    }

    /// 设置居中弹窗：左侧竖排分页导航（凭据库与主密码 / 外观 / 状态栏 / 终端 / 会话），右侧为当前分页内容。
    pub(super) fn render_settings_dialog(&self, cx: &Context<Self>) -> AnyElement {
        let pal = self.palette(cx);
        let Some(state) = self.settings.as_ref() else {
            return div().into_any_element();
        };
        let passphrase_mode =
            crate::vault::read_unlock_marker(&self.mode) == crate::vault::UnlockBackend::Passphrase;
        let unlocked = self.vault.is_some();
        let dark = cx.theme().is_dark();
        let body = match state.tab {
            SETTINGS_TAB_APPEARANCE => self.render_appearance_tab(&pal, dark, cx),
            SETTINGS_TAB_STATUS => self.render_status_tab(&pal, cx),
            SETTINGS_TAB_TERMINAL => self.render_terminal_tab(&pal, cx),
            SETTINGS_TAB_SESSION => self.render_session_tab(&pal, cx),
            _ => self.render_vault_tab(&pal, state, passphrase_mode, unlocked, cx),
        };
        let entity = cx.entity();
        // 竖排分页条目：不套底色不描边，选中项靠左侧强调色竖条 + 粗体区分，
        // 未选中项只弱化文字色（竖条留空占位，文字位置不跳）。
        let tab_item =
            |id: &'static str, label: SharedString, ix: usize, entity: Entity<RuTermApp>| {
                let selected = state.tab == ix;
                h_flex()
                    .id(id)
                    .h(px(30.0))
                    .items_center()
                    .gap_2()
                    .pr_3()
                    .rounded(px(6.0))
                    .text_sm()
                    .text_color(if selected { pal.text } else { pal.muted })
                    .when(selected, |item| item.font_weight(FontWeight(600.0)))
                    .hover(|style| style.bg(pal.card_hover).text_color(pal.text))
                    .on_click(move |_, _, cx| {
                        entity.update(cx, |this, cx| this.select_settings_tab(ix, cx));
                    })
                    .child(
                        div()
                            .w(px(2.0))
                            .h(px(16.0))
                            .flex_shrink_0()
                            .rounded(px(1.0))
                            .bg(if selected { pal.accent } else { pal.card_bg }),
                    )
                    .child(label)
            };
        v_flex()
            .id("settings-backdrop")
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
                    .id("settings-dialog")
                    // 尺寸固定：切分页不再让弹窗忽大忽小，超出部分交给内容区滚动。
                    .w(px(SETTINGS_DIALOG_W))
                    .h(px(SETTINGS_DIALOG_H))
                    .flex_shrink_0()
                    .gap_3()
                    .p_5()
                    .overflow_hidden()
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
                        div()
                            .text_base()
                            .font_weight(FontWeight(600.0))
                            .flex_shrink_0()
                            .child(tr!(cx, "设置")),
                    )
                    .child(
                        h_flex()
                            .flex_shrink_0()
                            .items_stretch()
                            .gap_4()
                            .child(
                                v_flex()
                                    .w(px(SETTINGS_NAV_W))
                                    .flex_shrink_0()
                                    .gap_1()
                                    .child(tab_item(
                                        "settings-tab-vault",
                                        tr!(cx, "凭据库与主密码"),
                                        SETTINGS_TAB_VAULT,
                                        entity.clone(),
                                    ))
                                    .child(tab_item(
                                        "settings-tab-appearance",
                                        tr!(cx, "外观"),
                                        SETTINGS_TAB_APPEARANCE,
                                        entity.clone(),
                                    ))
                                    .child(tab_item(
                                        "settings-tab-status",
                                        tr!(cx, "状态栏"),
                                        SETTINGS_TAB_STATUS,
                                        entity.clone(),
                                    ))
                                    .child(tab_item(
                                        "settings-tab-terminal",
                                        tr!(cx, "终端"),
                                        SETTINGS_TAB_TERMINAL,
                                        entity.clone(),
                                    ))
                                    .child(tab_item(
                                        "settings-tab-session",
                                        tr!(cx, "会话"),
                                        SETTINGS_TAB_SESSION,
                                        entity,
                                    )),
                            )
                            // 导航与内容之间只留一条分隔线，不再各套一个框。
                            .child(div().w(px(1.0)).flex_shrink_0().bg(pal.border))
                            .child(
                                v_flex()
                                    .flex_1()
                                    .min_w_0()
                                    // 显式定高：滚动容器要有确定边界，否则内容只会溢出被裁。
                                    .h(px(SETTINGS_CONTENT_H))
                                    .pl_5()
                                    .overflow_y_scrollbar()
                                    .child(body),
                            ),
                    )
                    .child(
                        h_flex().flex_shrink_0().gap_2().justify_end().child(
                            Button::new("settings-close")
                                .label(tr!(cx, "关闭"))
                                .secondary()
                                .outline()
                                .on_click(cx.listener(|this, _, _, cx| {
                                    this.close_settings(cx);
                                })),
                        ),
                    ),
            )
            .into_any_element()
    }

    /// 「外观」分页：明暗主题（与 `Ctrl+T`、标题栏 ☀☾ 同一开关）、边栏位置与界面语言。
    pub(super) fn render_appearance_tab(
        &self,
        pal: &Palette,
        dark: bool,
        cx: &Context<Self>,
    ) -> AnyElement {
        let locale = crate::i18n::current_locale(cx);
        v_flex()
            .id("settings-tab-appearance")
            .gap_3()
            .child(
                div()
                    .text_sm()
                    .text_color(pal.muted)
                    .child(tr!(cx, "主题切换立即生效并写入凭据库，下次启动沿用。")),
            )
            .child(
                h_flex()
                    .gap_2()
                    .items_center()
                    .child(div().text_sm().text_color(pal.muted).child(tr!(
                        cx,
                        "当前主题：{theme}",
                        theme = if dark {
                            tr!(cx, "暗色")
                        } else {
                            tr!(cx, "亮色")
                        }
                    )))
                    .child(
                        Button::new("settings-toggle-theme")
                            .label(if dark {
                                tr!(cx, "切换到亮色")
                            } else {
                                tr!(cx, "切换到暗色")
                            })
                            .secondary()
                            .outline()
                            .on_click(cx.listener(|this, _, window, cx| {
                                this.toggle_theme(window, cx);
                            })),
                    ),
            )
            .child(
                h_flex()
                    .gap_2()
                    .items_center()
                    .child(
                        div()
                            .text_sm()
                            .text_color(pal.muted)
                            .child(tr!(cx, "边栏位置")),
                    )
                    .child({
                        let left = self.sidebar.side == SidebarSide::Left;
                        let button = Button::new("settings-side-left").label(tr!(cx, "左侧"));
                        let button = if left {
                            button.primary()
                        } else {
                            button.secondary().outline()
                        };
                        button.on_click(cx.listener(|this, _, _, cx| {
                            this.set_sidebar_side(SidebarSide::Left, cx);
                        }))
                    })
                    .child({
                        let right = self.sidebar.side == SidebarSide::Right;
                        let button = Button::new("settings-side-right").label(tr!(cx, "右侧"));
                        let button = if right {
                            button.primary()
                        } else {
                            button.secondary().outline()
                        };
                        button.on_click(cx.listener(|this, _, _, cx| {
                            this.set_sidebar_side(SidebarSide::Right, cx);
                        }))
                    })
                    .child(
                        div()
                            .text_xs()
                            .text_color(pal.muted)
                            .child(tr!(cx, "导航栏始终贴在窗口外缘")),
                    ),
            )
            .child(
                h_flex()
                    .gap_2()
                    .items_center()
                    .child(
                        div()
                            .text_sm()
                            .text_color(pal.muted)
                            .child(tr!(cx, "界面语言")),
                    )
                    // 语言名恒用该语言本身书写，切过去之后才认得出这是刚才点的语言。
                    .children(crate::i18n::LANGUAGES.iter().map(|(code, native_name)| {
                        let on = *code == locale;
                        let button =
                            Button::new(format!("settings-locale-{code}")).label(*native_name);
                        let button = if on {
                            button.primary()
                        } else {
                            button.secondary().outline()
                        };
                        button.on_click(cx.listener(move |this, _, _, cx| {
                            this.apply_language(code, cx);
                        }))
                    }))
                    .child(
                        div()
                            .text_xs()
                            .text_color(pal.muted)
                            .child(tr!(cx, "立即生效；菜单与面板即刻改用该语言")),
                    ),
            )
            .into_any_element()
    }

    /// 「状态栏」分页：勾选底部状态栏要显示哪些条目，改动即时生效并写入凭据库。
    pub(super) fn render_status_tab(&self, pal: &Palette, cx: &Context<Self>) -> AnyElement {
        let entity = cx.entity();
        let row =
            |id: String, label: String, on: bool, pref: StatusPref, entity: Entity<RuTermApp>| {
                Checkbox::new(id)
                    .label(label)
                    .checked(on)
                    .on_change(move |value, _window, cx| {
                        entity.update(cx, |this, cx| this.set_status_pref(pref, value, cx));
                    })
            };
        let mut metrics = Vec::new();
        for metric in Metric::ALL {
            metrics.push(row(
                format!("status-{}", metric.id()),
                crate::i18n::translate(cx, metric.label(), &[]).to_string(),
                self.status_bar.metric_on(metric),
                StatusPref::Metric(metric),
                entity.clone(),
            ));
        }
        let mut readouts = Vec::new();
        for key in ReadoutKey::ALL {
            readouts.push(row(
                format!("status-readout-{}", key.id()),
                crate::i18n::translate(cx, key.label(), &[]).to_string(),
                self.status_bar.readout.on(key),
                StatusPref::Readout(key),
                entity.clone(),
            ));
        }
        v_flex()
            .id("settings-tab-status")
            .gap_3()
            .child(div().text_sm().text_color(pal.muted).child(tr!(
                cx,
                "勾选底部状态栏要显示的条目，取消勾选即隐藏；改动立即生效并写入凭据库。"
            )))
            .child(
                v_flex()
                    .gap_2()
                    .child(
                        div()
                            .text_sm()
                            .text_color(pal.muted)
                            .child(tr!(cx, "会话信息")),
                    )
                    .child(row(
                        "status-addr".into(),
                        tr!(cx, "连接地址（user@host:port）").to_string(),
                        self.status_bar.addr,
                        StatusPref::Addr,
                        entity.clone(),
                    ))
                    .child(row(
                        "status-auth".into(),
                        tr!(cx, "认证方式").to_string(),
                        self.status_bar.auth,
                        StatusPref::Auth,
                        entity.clone(),
                    )),
            )
            .child(
                v_flex()
                    .gap_2()
                    .child(div().text_sm().text_color(pal.muted).child(tr!(
                        cx,
                        "远程主机信息（连接后经 exec 探测取得，远端没探到的项自动不显示）"
                    )))
                    .children(metrics),
            )
            .child(
                v_flex()
                    .gap_2()
                    .child(div().text_sm().text_color(pal.muted).child(tr!(
                        cx,
                        "终端状态（读自活动终端，本地终端与远程会话都适用）"
                    )))
                    .children(readouts),
            )
            .into_any_element()
    }

    /// 「终端」分页：字号、行高与配色方案，改动即刻刷到所有已开终端并写入凭据库。
    pub(super) fn render_terminal_tab(&self, pal: &Palette, cx: &Context<Self>) -> AnyElement {
        let entity = cx.entity();
        let themes = Self::terminal_themes(cx);
        let theme_index = themes
            .iter()
            .position(|name| *name == self.terminal.theme)
            .unwrap_or(0);
        // 步进按钮：− / + 各一档，值显示在中间，比数字框少一层提交路径。
        let row = |label: SharedString, value: String, minus, plus| {
            h_flex()
                .gap_2()
                .items_center()
                .child(
                    div()
                        .w(px(72.0))
                        .text_sm()
                        .text_color(pal.muted)
                        .child(label),
                )
                .child(minus)
                .child(
                    div()
                        .w(px(64.0))
                        .text_center()
                        .text_sm()
                        .flex_shrink_0()
                        .child(value),
                )
                .child(plus)
        };
        v_flex()
            .id("settings-tab-terminal")
            .gap_3()
            .child(
                div()
                    .text_sm()
                    .text_color(pal.muted)
                    .child(tr!(cx, "终端外观只影响会话终端（界面主题在「外观」分页里），改动立即生效并写入凭据库。")),
            )
            .child(row(
                tr!(cx, "字号"),
                format!("{} px", self.terminal.font_size as i32),
                stepper(
                    "terminal-font-minus",
                    "−",
                    cx.listener(|this, _, _, cx| this.nudge_font_size(-1, cx)),
                ),
                stepper(
                    "terminal-font-plus",
                    "+",
                    cx.listener(|this, _, _, cx| this.nudge_font_size(1, cx)),
                ),
            ))
            .child(row(
                tr!(cx, "行高"),
                format!("{:.1}", self.terminal.line_height),
                stepper(
                    "terminal-line-minus",
                    "−",
                    cx.listener(|this, _, _, cx| this.nudge_line_height(-1, cx)),
                ),
                stepper(
                    "terminal-line-plus",
                    "+",
                    cx.listener(|this, _, _, cx| this.nudge_line_height(1, cx)),
                ),
            ))
            .child(
                h_flex()
                    .gap_2()
                    .items_center()
                    .child(
                        div()
                            .w(px(72.0))
                            .text_sm()
                            .text_color(pal.muted)
                            .child(tr!(cx, "配色方案")),
                    )
                    .child({
                        let options = themes.clone();
                        Select::new(themes)
                            .id("terminal-theme")
                            .selected(Some(theme_index))
                            .w(px(200.0))
                            .on_change(move |index, _name, _window, cx| {
                                let Some(name) = options.get(index).cloned() else {
                                    return;
                                };
                                let entity = entity.clone();
                                entity.update(cx, |this, cx| {
                                    this.set_terminal_theme(&name, cx);
                                });
                            })
                    }),
            )
            .when(self.terminal.theme == TERMINAL_THEME_CUSTOM, |this| {
                this.child(self.render_custom_colors(pal, cx))
            })
            .child(div().mt_2().h(px(1.0)).w_full().bg(pal.border))
            .child({
                let entity = cx.entity();
                Checkbox::new("terminal-inline-completion")
                    .label(tr!(cx, "终端内自动补全"))
                    .checked(self.terminal.inline_completion)
                    .on_change(move |value, _window, cx| {
                        entity.update(cx, |this, cx| this.set_inline_completion(value, cx));
                    })
            })
            .child(
                div()
                    .text_xs()
                    .text_color(pal.muted)
                    .child(tr!(cx, "输入命令时自动弹出片段候选：↑↓ 选择、Tab / Enter 补全、Esc 关闭；补全只替换正在输入的那个词，不会执行命令。关闭后按键完全交给 shell。")),
            )
            .child({
                let entity = cx.entity();
                Checkbox::new("terminal-shell-integration")
                    .label(tr!(cx, "让远端 shell 上报当前目录"))
                    .checked(self.terminal.shell_integration)
                    .on_change(move |value, _window, cx| {
                        entity.update(cx, |this, cx| this.set_shell_integration(value, cx));
                    })
            })
            .child(
                div()
                    .text_xs()
                    .text_color(pal.muted)
                    .child(tr!(cx, "连上 bash / zsh 会话时追加一条提示符钩子，用 OSC 7 上报 $PWD；文件面板的「定位 / 跟随」据此跳到终端所在目录。关掉或换成别的 shell 时，仍按命令行里的 cd 猜测目录。改动只对之后建立的会话生效。")),
            )
            .child(self.render_local_shell_row(pal, cx))
            .child(
                div()
                    .text_xs()
                    .text_color(pal.muted)
                    .child(tr!(
                        cx,
                        "「一键扫描」会实际调用 wsl.exe 列出 WSL 发行版，故放在按钮后面而不是启动时；扫描结果列在{side}「本地终端」分组里，点一下即开对应终端。",
                        side = match self.sidebar.side {
                            SidebarSide::Left => tr!(cx, "左侧"),
                            SidebarSide::Right => tr!(cx, "右侧"),
                        }
                    )),
            )
            .into_any_element()
    }

    /// 自定义 16 色编辑器：前景 / 背景两行，下面 8 行「普通 | 明亮」同色成对。
    /// 输入框只认 `#RGB` / `#RRGGBB`，非法内容不提交，色块保持上一个有效颜色。
    pub(super) fn render_custom_colors(&self, pal: &Palette, cx: &Context<Self>) -> AnyElement {
        let colors = &self.terminal.custom;
        // 单个色块 + 输入框；下标按 `TerminalColors` 字段序列取。
        let cell = |index: usize| {
            h_flex()
                .gap_1()
                .items_center()
                .child(
                    div()
                        .size(px(14.0))
                        .flex_shrink_0()
                        .rounded(px(3.0))
                        .border_1()
                        .border_color(pal.border)
                        .bg(hex_to_hsla(colors.hex_field(index)).unwrap_or(pal.border)),
                )
                .child(styled_input(Input::new(&self.custom_color_inputs[index]), pal).w(px(90.0)))
        };
        let labeled = |label: SharedString, index: usize| {
            h_flex()
                .w(px(150.0))
                .gap_1()
                .items_center()
                .child(
                    div()
                        .w(px(32.0))
                        .text_sm()
                        .text_color(pal.muted)
                        .child(label),
                )
                .child(cell(index))
        };
        let mut grid = v_flex().gap_1().child(
            h_flex()
                .gap_3()
                .child(labeled(tr!(cx, "前景"), 0))
                .child(labeled(tr!(cx, "背景"), 1)),
        );
        for (i, name) in ANSI_COLOR_NAMES.iter().enumerate().take(8) {
            grid = grid.child(
                h_flex()
                    .gap_3()
                    .child(labeled(SharedString::from(*name), CUSTOM_ANSI_BASE + i))
                    .child(labeled(SharedString::from(*name), CUSTOM_BRIGHT_BASE + i)),
            );
        }
        v_flex()
            .gap_2()
            .rounded(px(6.0))
            .border_1()
            .border_color(pal.border)
            .p_3()
            .child(
                h_flex()
                    .gap_2()
                    .items_center()
                    .justify_between()
                    .child(
                        div()
                            .text_sm()
                            .text_color(pal.text)
                            .child(tr!(cx, "自定义 16 色（左列普通 · 右列明亮）")),
                    )
                    .child(
                        Button::new("terminal-custom-reset")
                            .label(tr!(cx, "恢复 One Dark"))
                            .secondary()
                            .outline()
                            .on_click(cx.listener(|this, _, window, cx| {
                                this.reset_custom_colors(window, cx);
                            })),
                    ),
            )
            .child(grid)
            .child(div().text_xs().text_color(pal.muted).child(tr!(
                cx,
                "输入 #RGB 或 #RRGGBB，回车前即生效；格式不合法时保持上一个有效颜色。"
            )))
            .into_any_element()
    }

    /// 「会话」分页：断线自动重连与连接超时，改动即时生效并写入凭据库。
    pub(super) fn render_session_tab(&self, pal: &Palette, cx: &Context<Self>) -> AnyElement {
        let auto = cx.entity();
        let tray = cx.entity();
        v_flex()
            .id("settings-tab-session")
            .gap_3()
            .child(
                div()
                    .text_sm()
                    .text_color(pal.muted)
                    .child(tr!(cx, "连接意外中断（断网、服务端掉线）后自动重建 SSH 会话；远端主动 exit 不触发自动重连。")),
            )
            .child(
                Checkbox::new("session-auto-reconnect")
                    .label(tr!(cx, "断线后自动重连"))
                    .checked(self.session_prefs.auto_reconnect)
                    .on_change(move |value, _window, cx| {
                        auto.update(cx, |this, cx| this.set_auto_reconnect(value, cx));
                    }),
            )
            .child(div().text_xs().text_color(pal.muted).child(tr!(
                cx,
                "首次连接就失败（地址写错、端口不通）不自动重试；断线后最多连续重试 {max} 次，退避 1 / 2 / 5 / 10 / 20 秒，会话稳定运行 30 秒后重新计数。等待期间可在横幅里取消或按回车立即重连。",
                max = AUTO_RECONNECT_MAX
            )))
            .child(
                h_flex()
                    .gap_2()
                    .items_center()
                    .child(
                        div()
                            .w(px(72.0))
                            .text_sm()
                            .text_color(pal.muted)
                            .child(tr!(cx, "连接超时")),
                    )
                    .child(stepper(
                        "session-timeout-minus",
                        "−",
                        cx.listener(|this, _, _, cx| this.nudge_connect_timeout(-1, cx)),
                    ))
                    .child(
                        div()
                            .w(px(64.0))
                            .text_center()
                            .text_sm()
                            .flex_shrink_0()
                            .child(tr!(
                                cx,
                                "{seconds} 秒",
                                seconds = self.session_prefs.connect_timeout
                            )),
                    )
                    .child(stepper(
                        "session-timeout-plus",
                        "+",
                        cx.listener(|this, _, _, cx| this.nudge_connect_timeout(1, cx)),
                    )),
            )
            .child(
                div()
                    .text_xs()
                    .text_color(pal.muted)
                    .child(tr!(cx, "超时覆盖 DNS、TCP、握手、认证与开启 shell 的全过程；超时即结束本次尝试并报「连接超时」。")),
            )
            .child(div().mt_2().h(px(1.0)).w_full().bg(pal.border))
            .child(
                Checkbox::new("session-tray")
                    .label(tr!(cx, "启用系统托盘"))
                    .checked(crate::tray_enabled())
                    .on_change(move |value, _window, cx| {
                        tray.update(cx, |this, cx| this.set_tray_enabled(value, cx));
                    }),
            )
            .child(
                div()
                    .text_xs()
                    .text_color(pal.muted)
                    .child(tr!(cx, "开启后关闭窗口会隐藏到托盘，会话在后台保活；关闭（默认）则关窗直接退出。运行中关闭图标要等下次启动才完全移除。")),
            )
            .into_any_element()
    }

    /// 会话行为改动：记账 → 持久化（库内优先，降级时写 settings.json）。
    pub(super) fn set_session_prefs(&mut self, mut prefs: SessionPrefs, cx: &mut Context<Self>) {
        prefs.clamp();
        if prefs == self.session_prefs {
            return;
        }
        self.session_prefs = prefs;
        cx.notify();
        self.persist_session(cx);
    }

    /// 开关断线自动重连。
    pub(super) fn set_auto_reconnect(&mut self, auto_reconnect: bool, cx: &mut Context<Self>) {
        let mut prefs = self.session_prefs.clone();
        prefs.auto_reconnect = auto_reconnect;
        self.set_session_prefs(prefs, cx);
    }

    /// 连接超时增减一档。
    pub(super) fn nudge_connect_timeout(&mut self, steps: i32, cx: &mut Context<Self>) {
        let mut prefs = self.session_prefs.clone();
        prefs.nudge_connect_timeout(steps);
        self.set_session_prefs(prefs, cx);
    }

    /// 开关系统托盘：立即创建 / 置空图标并写启动设置。
    ///
    /// 托盘必须在建窗前决定，所以进 settings.json 而不是库内 `settings` 表；
    /// win32 后端没有删除图标的接口，运行中关闭只是把图标置空，完整移除靠下次启动。
    pub(super) fn set_tray_enabled(&mut self, enabled: bool, cx: &mut Context<Self>) {
        if crate::tray_enabled() == enabled {
            return;
        }
        crate::TRAY_ENABLED.store(enabled, std::sync::atomic::Ordering::Relaxed);
        self.store.save_tray(enabled);
        if let Some(vault) = self.vault.clone() {
            let value = if enabled { "1" } else { "0" };
            cx.spawn(async move |_, _| {
                if let Err(error) = vault.set_setting("tray", value).await {
                    log::warn!("写入库内托盘偏好失败: {error}");
                }
            })
            .detach();
        }
        if enabled {
            cx.set_tray_icon(Some(crate::TRAY_ICON));
            self.sync_tray(cx);
            self.status = "已启用系统托盘：关闭窗口将隐藏到托盘".into();
        } else {
            cx.set_tray_icon(None);
            self.status = "已关闭系统托盘：之后关窗直接退出，图标在下次启动时完全移除".into();
        }
        cx.notify();
    }

    /// 会话行为偏好落库；加密库不可用时退回旧 settings.json。
    pub(super) fn persist_session(&self, cx: &mut Context<Self>) {
        let json = self.session_prefs.to_json();
        let Some(vault) = self.vault.clone() else {
            self.store.save_session(&json);
            return;
        };
        cx.spawn(async move |_, _| {
            if let Err(error) = vault.set_setting("session", &json).await {
                log::warn!("写入库内会话偏好失败: {error}");
            }
        })
        .detach();
    }
}

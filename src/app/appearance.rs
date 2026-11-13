//! //! 外观：明暗主题与语言切换、终端字号 / 行高 / 配色（含自定义 16 色）的即时生效与持久化。


use rgpui::prelude::*;
use rgpui::{
    AbsoluteLength, App, Context, Hsla, SharedString, Window, px,
};
use rgpui_term::{
    TextStyle, ThemeDefinition,
    ThemeManager,
};

use crate::{ToggleTheme};

use super::*;


impl RuTermApp {
    /// 切换明暗主题：翻转当前模式后落到 `apply_theme`。
    pub(super) fn toggle_theme(&self, window: &mut Window, cx: &mut Context<Self>) {
        let dark = cx.theme().is_dark();
        self.apply_theme(!dark, window, cx);
    }


    /// 设定明暗主题：翻转全局 Theme 模式并持久化，界面（含状态栏/按钮）随之重取配色。
    pub(super) fn apply_theme(&self, dark: bool, window: &mut Window, cx: &mut Context<Self>) {
        let mode = if dark {
            rgpui::theme::ThemeMode::Dark
        } else {
            rgpui::theme::ThemeMode::Light
        };
        rgpui::theme::Theme::change(mode, Some(window), cx);
        if let Some(vault) = self.vault.clone() {
            let tag = if dark { "dark" } else { "light" };
            let tag = SharedString::from(tag);
            cx.spawn(async move |_, _| {
                if let Err(error) = vault.set_setting("theme", &tag).await {
                    log::warn!("写入库内主题失败: {error}");
                }
            })
            .detach();
        } else {
            // 降级模式（无加密库）沿用旧 settings.json。
            self.store.save_theme(dark);
        }
        // 明暗主题不是秘密：同步写入启动镜像，锁库时首帧也能直接用上次主题。
        if self.vault.is_some() {
            self.store.save_theme_mirror(dark);
        }
        cx.notify();
    }


    /// 切换界面语言：改全局 i18n 管理器、持久化后整体重绘。
    ///
    /// 与主题不同，这里两份存储都写：库内是权威值，`locale.txt` 是启动镜像——
    /// 窗口首帧早于解锁，只有本地文件能让菜单第一时间就用对语言。
    pub(super) fn apply_language(&mut self, locale: &str, cx: &mut Context<Self>) {
        if crate::i18n::current_locale(cx) == locale {
            return;
        }
        crate::i18n::set_locale(locale, cx);
        self.store.save_language(locale);
        if let Some(vault) = self.vault.clone() {
            let locale = locale.to_string();
            cx.spawn(async move |_, _| {
                if let Err(error) = vault.set_setting("language", &locale).await {
                    log::warn!("写入库内界面语言失败: {error}");
                }
            })
            .detach();
        }
        // 托盘菜单不在渲染树里，得显式重建一次才会取到新语言的词条。
        self.sync_tray(cx);
        cx.notify();
    }


    /// Ctrl+T / 菜单项：切换明暗主题。
    pub(super) fn toggle_theme_action(
        &mut self,
        _: &ToggleTheme,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.toggle_theme(window, cx);
    }


    /// 终端底色：内边距要与终端内容同色，否则在浅色主题下会显成一圈白框。
    pub(super) fn terminal_bg(&self, cx: &App) -> Hsla {
        let Some(manager) = cx.try_global::<ThemeManager>() else {
            return self.palette(cx).window_bg;
        };
        manager
            .get_theme(&self.terminal.theme)
            .or_else(|| Some(manager.current_theme()))
            .map(|theme| theme.background)
            .unwrap_or_else(|| self.palette(cx).window_bg)
    }


    /// 由偏好组装终端渲染样式。配色取自 rgpui-term 的 `ThemeManager` 预设
    /// （方案名读不到时回落当前主题，避免解析不到颜色）；字体族沿用默认等宽字体。
    pub(super) fn terminal_text_style(&self, cx: &App) -> TextStyle {
        let mut style = TextStyle {
            font_size: AbsoluteLength::Pixels(px(self.terminal.font_size)),
            line_height_multiplier: self.terminal.line_height,
            ..Default::default()
        };
        let theme = cx
            .try_global::<ThemeManager>()
            .and_then(|manager| manager.get_theme(&self.terminal.theme).cloned())
            .or_else(|| {
                cx.try_global::<ThemeManager>()
                    .map(|manager| manager.current_theme().clone())
            });
        if let Some(theme) = theme {
            style.foreground = theme.foreground;
            style.background = theme.background;
            style.theme = theme;
        }
        style
    }


    /// 把当前样式刷到所有已开终端：设置里改一项即刻见效，无需重连。
    pub(super) fn apply_terminal_style(&self, style: &TextStyle, cx: &mut App) {
        for tab in &self.tabs {
            let style = style.clone();
            if let Some(view) = tab.terminal_view.as_ref() {
                view.update(cx, |view, cx| {
                    *view.text_style_mut() = style;
                    cx.notify();
                });
            }
        }
    }


    /// 终端外观改动：记账 → 刷新所有终端 → 持久化（库内优先，降级时写 settings.json）。
    pub(super) fn set_terminal_prefs(&mut self, mut prefs: TerminalPrefs, cx: &mut Context<Self>) {
        prefs.clamp();
        if prefs == self.terminal {
            return;
        }
        self.terminal = prefs;
        // 自定义色改了就重新注册，之后 `terminal_text_style` 才取得到新配色。
        Self::register_custom_theme(&self.terminal.custom, cx);
        let style = self.terminal_text_style(cx);
        self.apply_terminal_style(&style, cx);
        cx.notify();
        self.persist_terminal(cx);
    }


    /// 字号增减一档（`steps` 为正负档数）。
    pub(super) fn nudge_font_size(&mut self, steps: i32, cx: &mut Context<Self>) {
        let mut prefs = self.terminal.clone();
        prefs.nudge_font_size(steps);
        self.set_terminal_prefs(prefs, cx);
    }


    /// 行高增减一档。
    pub(super) fn nudge_line_height(&mut self, steps: i32, cx: &mut Context<Self>) {
        let mut prefs = self.terminal.clone();
        prefs.nudge_line_height(steps);
        self.set_terminal_prefs(prefs, cx);
    }


    /// 切换终端配色方案。
    pub(super) fn set_terminal_theme(&mut self, name: &str, cx: &mut Context<Self>) {
        let mut prefs = self.terminal.clone();
        prefs.theme = name.to_string();
        self.set_terminal_prefs(prefs, cx);
    }


    /// 开关终端内自动补全：关闭后按键拦截器直接让路，浮层与行跟踪一起清掉。
    pub(super) fn set_inline_completion(&mut self, enabled: bool, cx: &mut Context<Self>) {
        let mut prefs = self.terminal.clone();
        prefs.inline_completion = enabled;
        self.set_terminal_prefs(prefs, cx);
        if !enabled {
            self.close_completion(cx);
            self.typed_line = Some(String::new());
        }
    }


    /// 把「自定义」配色注册进全局 `ThemeManager`（重复调用即覆盖）。
    pub(super) fn register_custom_theme(colors: &TerminalColors, cx: &mut App) {
        if !cx.has_global::<ThemeManager>() {
            return;
        }
        ThemeManager::global_mut(cx).add_theme(ThemeDefinition {
            name: TERMINAL_THEME_CUSTOM.to_string(),
            theme: colors.to_config().to_theme(),
        });
    }


    /// 某个自定义色输入框内容变化：合法十六进制才提交，非法输入保留上一个有效色。
    pub(super) fn commit_custom_color(&mut self, index: usize, cx: &mut Context<Self>) {
        let Some(input) = self.custom_color_inputs.get(index) else {
            return;
        };
        let text = input.read(cx).value().to_string();
        let Some(normalized) = normalize_hex(&text) else {
            return;
        };
        if self.terminal.custom.hex_field(index) == normalized {
            return;
        }
        let mut prefs = self.terminal.clone();
        prefs.custom.set_hex_field(index, normalized);
        self.set_terminal_prefs(prefs, cx);
    }


    /// 恢复自定义配色的默认值（One Dark），并同步输入框显示。
    pub(super) fn reset_custom_colors(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let mut prefs = self.terminal.clone();
        prefs.custom = TerminalColors::default();
        self.set_terminal_prefs(prefs, cx);
        self.sync_custom_color_inputs(window, cx);
    }


    /// 把偏好里的颜色回填到输入框（打开设置弹窗时调用，库内值可能晚于启动到达）。
    pub(super) fn sync_custom_color_inputs(&self, window: &mut Window, cx: &mut App) {
        for (index, input) in self.custom_color_inputs.iter().enumerate() {
            let value = self.terminal.custom.hex_field(index);
            input.update(cx, |state, cx| {
                if state.value().as_ref() != value {
                    state.set_value(value, window, cx);
                }
            });
        }
    }


    /// 可选的终端配色方案名（`ThemeManager` 预设，已按名排序）。
    pub(super) fn terminal_themes(cx: &App) -> Vec<SharedString> {
        match cx.try_global::<ThemeManager>() {
            Some(manager) => manager
                .available_themes()
                .into_iter()
                .map(SharedString::from)
                .collect(),
            None => vec![TERMINAL_THEME_DEFAULT.into()],
        }
    }


    /// 终端偏好落库；加密库不可用时退回旧 settings.json。
    pub(super) fn persist_terminal(&self, cx: &mut Context<Self>) {
        let json = self.terminal.to_json();
        let Some(vault) = self.vault.clone() else {
            self.store.save_terminal(&json);
            return;
        };
        cx.spawn(async move |_, _| {
            if let Err(error) = vault.set_setting("terminal", &json).await {
                log::warn!("写入库内终端偏好失败: {error}");
            }
        })
        .detach();
    }


}

//! //! 边栏：页面枚举、图标轨 / 面板渲染、宽度拖拽与折叠，以及各页的路由。

use std::time::Duration;

use crate::tr;
use crate::{FileIcon, ToggleFilePanel, ToggleSidebar};
use rgpui::components::split_pane::CollapsiblePane;
use rgpui::input_ui::Input;
use rgpui::prelude::*;
use rgpui::{
    AnyElement, App, Context, DragMoveEvent, ElementSize, Empty, Icon, IconName,
    InteractiveElement, IntoElement, MouseButton, MouseDownEvent, MouseUpEvent, ParentElement,
    Render, ScrollableElement, SharedString, StatefulInteractiveElement, Styled, Tooltip, Window,
    div, px, v_flex,
};

use super::*;

/// 导航轨宽度＝图标按钮宽度：图标两侧不留空白，轨道尽量窄。
const RAIL_WIDTH: f32 = 30.0;

/// 边栏分隔条的拖拽载荷。搭 rgpui 原生拖拽会话：面板内的表格等元素会拦下
/// 冒泡的鼠标移动，只有拖拽会话能在指针划过它们时继续送达移动事件。
#[derive(Clone)]
pub(super) struct SplitDividerDrag;

impl Render for SplitDividerDrag {
    fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
        Empty
    }
}

/// 边栏页面：主机列表、远端文件、本机文件、端口转发共用同一条边栏。
/// 命令历史不在这里，而是窗口右侧的独立面板（见 `history_ui.rs`）。
#[derive(Clone, Copy, PartialEq, Eq)]
pub(super) enum SidebarPage {
    Hosts,
    Files,
    Local,
    Forward,
}

/// 边栏默认宽度占位（约 1/3）。边栏恒在窗口左侧，比例即面板份额。
pub(super) const DEFAULT_SIDEBAR_RATIO: f32 = 0.32;

impl RuTermApp {
    /// 边栏面板是分隔容器的第一面板，折叠它即收起边栏。
    pub(super) const SIDEBAR_PANE: CollapsiblePane = CollapsiblePane::First;

    /// 边栏是否可见（由分隔面板的折叠状态决定）。
    pub(super) fn sidebar_visible(&self, cx: &App) -> bool {
        !self.split_state.read(cx).is_collapsed()
    }

    /// 展开边栏并切到某页（不负责该页的数据准备）。
    pub(super) fn show_sidebar_page(&mut self, page: SidebarPage, cx: &mut Context<Self>) {
        self.sidebar_page = page;
        if self.split_state.read(cx).is_collapsed() {
            self.split_state.update(cx, |state, cx| state.expand(cx));
        }
    }

    /// 从导航 / 菜单切到某页：需要时展开边栏并准备该页数据。
    pub(super) fn set_sidebar_page(
        &mut self,
        page: SidebarPage,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.show_sidebar_page(page, cx);
        match page {
            SidebarPage::Hosts => {}
            SidebarPage::Files => self.ensure_files_shown(cx),
            SidebarPage::Local => self.ensure_local_shown(cx),
            SidebarPage::Forward => self.open_forward(window, cx),
        }
        cx.notify();
    }

    /// 切换「文件」页；已经停留在文件页时再按一次收起边栏。
    pub(super) fn toggle_file_panel(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.sidebar_visible(cx) && self.sidebar_page == SidebarPage::Files {
            let pane = Self::SIDEBAR_PANE;
            self.split_state
                .update(cx, |state, cx| state.toggle_collapse(pane, cx));
        } else {
            self.set_sidebar_page(SidebarPage::Files, window, cx);
        }
        cx.notify();
    }

    /// Ctrl+B / 菜单项：折叠或展开边栏。
    pub(super) fn toggle_sidebar(
        &mut self,
        _: &ToggleSidebar,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let pane = Self::SIDEBAR_PANE;
        self.split_state.update(cx, |state, cx| {
            state.toggle_collapse(pane, cx);
        });
        cx.notify();
    }

    /// 边栏面板占分隔容器的宽度比例。
    pub(super) fn panel_ratio(&self, cx: &App) -> f32 {
        self.split_state.read(cx).ratio()
    }

    /// 按面板占比写回分隔状态。
    pub(super) fn set_panel_ratio(&mut self, panel_ratio: f32, cx: &mut Context<Self>) {
        self.split_state
            .update(cx, |state, cx| state.set_ratio(panel_ratio, cx));
    }

    /// 分隔条按下：记下起点，拖动时按容器宽度把指针位移换算成占比。
    pub(super) fn begin_split_drag(
        &mut self,
        event: &MouseDownEvent,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.split_drag = Some((event.position.x.as_f32(), self.panel_ratio(cx)));
        cx.notify();
    }

    pub(super) fn drag_split(
        &mut self,
        event: &DragMoveEvent<SplitDividerDrag>,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let x = event.event.position.x.as_f32();
        if self.split_drag.is_none() {
            self.split_drag = Some((x, self.panel_ratio(cx)));
        }
        let Some((start_x, start_ratio)) = self.split_drag else {
            return;
        };
        let width = self.split_width.get();
        if width <= 0.0 {
            return;
        }
        let delta = (x - start_x) / width;
        // 两侧都留 260px：拖到最小即停住，完全收起交给 Ctrl+B。
        let min_ratio = 260.0 / width;
        let max_ratio = 1.0 - min_ratio;
        let target = (start_ratio + delta).clamp(min_ratio, max_ratio.max(min_ratio));
        self.set_panel_ratio(target, cx);
    }

    pub(super) fn end_split_drag(
        &mut self,
        _: &MouseUpEvent,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.split_drag.take().is_some() {
            cx.notify();
        }
    }

    /// 边栏与主区之间的分隔条：4px 可见，按下即可拖动。
    pub(super) fn render_split_divider(&self, cx: &mut Context<Self>) -> AnyElement {
        let pal = self.palette(cx);
        div()
            .id("sidebar-split-divider")
            .occlude()
            .flex_shrink_0()
            .w(px(4.0))
            .h_full()
            .bg(pal.border)
            .cursor_col_resize()
            .hover(move |style| style.bg(pal.accent))
            .on_mouse_down(MouseButton::Left, cx.listener(Self::begin_split_drag))
            .on_drag(SplitDividerDrag, |drag, _, _, cx| {
                cx.stop_propagation();
                cx.new(|_| drag.clone())
            })
            .on_drag_move(cx.listener(Self::drag_split))
            .on_mouse_up_out(MouseButton::Left, cx.listener(Self::end_split_drag))
            .into_any_element()
    }

    /// `Ctrl+Shift+B`：切到文件页；已在文件页时收起边栏。
    pub(super) fn toggle_file_panel_action(
        &mut self,
        _: &ToggleFilePanel,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.toggle_file_panel(window, cx);
    }

    /// 宽度与折叠由外层 SplitPane 负责，这里只填充第一面板区域。
    /// 主机列表页：搜索框 + 主机树（待输入密码时换成密码面板）。
    pub(super) fn render_hosts_page(&self, cx: &mut Context<Self>) -> AnyElement {
        let pal = self.palette(cx);
        let panel = if self.pending.is_some() {
            self.render_pending_panel(cx)
        } else {
            self.render_host_list(cx)
        };

        // 搜索框只在主机列表模式下出现（待输入密码时隐藏；表单已是居中弹窗，不占列表）。
        let list_mode = self.pending.is_none();

        v_flex()
            .size_full()
            .bg(pal.sidebar_bg)
            .p_3()
            .gap_2()
            .when(list_mode, |this| {
                this.child(styled_input(Input::new(&self.search), &pal))
            })
            .child(
                div()
                    .id("sidebar-list")
                    .flex_1()
                    .min_h_0()
                    .overflow_y_scrollbar()
                    .child(panel),
            )
            .into_any_element()
    }

    /// 边栏导航条：贴窗口外缘的竖排图标。
    ///
    /// 学RustRover：只画图标，不排文字；文字交给悬停 tooltip。
    /// 它独立于边栏面板，面板收起时依然留在窗口外缘，
    /// 相当于 IDE 左侧那条工具窗口栏。
    pub(super) fn render_sidebar_rail(&self, cx: &mut Context<Self>) -> AnyElement {
        let pal = self.palette(cx);
        let pages = [
            (
                SidebarPage::Hosts,
                "rail-hosts",
                tr!(cx, "主机"),
                Icon::new(IconName::User),
            ),
            (
                SidebarPage::Files,
                "rail-files",
                tr!(cx, "文件"),
                Icon::new(IconName::FolderOpen),
            ),
            (
                SidebarPage::Local,
                "rail-local",
                tr!(cx, "本机"),
                Icon::new(IconName::HardDrive),
            ),
            (
                SidebarPage::Forward,
                "rail-forward",
                tr!(cx, "转发"),
                Icon::new(IconName::Network),
            ),
        ];
        let mut rail = v_flex()
            .id("sidebar-rail")
            .w(px(RAIL_WIDTH))
            .flex_shrink_0()
            .min_w_0()
            .h_full()
            .items_center()
            .py_2()
            .gap_1()
            .bg(pal.sidebar_bg);
        // 转发跑在后台：收起面板也要能从导航上看出「还有几条在生效」。
        let forward_count = self
            .active_tab
            .and_then(|id| self.tabs.iter().find(|tab| tab.id == id))
            .map_or(0, |tab| tab.forwards.len());
        for (page, id, label, icon) in pages {
            let active = self.sidebar_page == page;
            // 转发的生效条数是常驻状态，压成图标角标，不占竖向空间。
            let badge =
                (page == SidebarPage::Forward && forward_count > 0).then_some(forward_count);
            let item = self.render_rail_item(
                id,
                label,
                icon,
                active,
                badge,
                // 高亮条画在贴窗口外缘的那一侧，这条轨恒在左缘。
                true,
                &pal,
                move |this, window, cx| {
                    // 再点当前页的图标＝收起面板，和 IDE 里的工具窗口一个脾气。
                    if this.sidebar_page == page && this.sidebar_visible(cx) {
                        let pane = Self::SIDEBAR_PANE;
                        this.split_state
                            .update(cx, |state, cx| state.toggle_collapse(pane, cx));
                        cx.notify();
                        return;
                    }
                    this.set_sidebar_page(page, window, cx)
                },
                cx,
            );
            rail = rail.child(item);
        }
        // 导航轨贴窗口左缘，右边框把它和面板分开。
        rail.border_r_1()
            .border_color(pal.border)
            .into_any_element()
    }

    /// 右侧工具窗口栏：贴窗口右缘，和左侧导航轨同一形状。
    ///
    /// 每个按钮控制右侧停靠区里的一个分区（当前是片段、历史）；
    /// 以后新增右侧面板时，在这里追加一项、并在停靠区里加对应分区即可。
    pub(super) fn render_right_rail(&self, cx: &mut Context<Self>) -> AnyElement {
        let pal = self.palette(cx);
        let mut rail = v_flex()
            .id("right-rail")
            .w(px(RAIL_WIDTH))
            .flex_shrink_0()
            .min_w_0()
            .h_full()
            .items_center()
            .py_2()
            .gap_1()
            .bg(pal.sidebar_bg);
        let snippets = self.render_rail_item(
            "rail-snippets",
            tr!(cx, "命令片段"),
            Icon::new(FileIcon::Braces),
            self.snippets_panel_open,
            None,
            // 高亮条贴窗口右缘。
            false,
            &pal,
            move |this, _, cx| this.toggle_snippets_panel(cx),
            cx,
        );
        let history = self.render_rail_item(
            "rail-history",
            tr!(cx, "历史"),
            Icon::new(FileIcon::History),
            self.history_panel_open,
            None,
            false,
            &pal,
            move |this, _, cx| this.toggle_history_panel(cx),
            cx,
        );
        rail = rail.child(snippets).child(history);
        rail.border_l_1()
            .border_color(pal.border)
            .into_any_element()
    }

    /// 工具窗口栏上的一个图标按钮：左侧导航轨与右侧面板轨共用。
    #[allow(clippy::too_many_arguments)]
    fn render_rail_item(
        &self,
        id: &'static str,
        label: SharedString,
        icon: Icon,
        active: bool,
        badge: Option<usize>,
        stripe_at_left: bool,
        pal: &Palette,
        on_click: impl Fn(&mut Self, &mut Window, &mut Context<Self>) + 'static,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let color = if active { pal.accent } else { pal.muted };
        let tip = match badge {
            Some(count) => tr!(
                cx,
                "{label}（{count} 条生效）",
                label = label.clone(),
                count = count
            ),
            None => label.clone(),
        };
        // 活动项的高亮条：RustRover 在选中图标外侧画一道竖条。
        let stripe = div()
            .absolute()
            .top(px(7.0))
            .bottom(px(7.0))
            .w(px(2.0))
            .rounded_full()
            .bg(pal.accent)
            .when(stripe_at_left, |this| this.left(px(0.0)))
            .when(!stripe_at_left, |this| this.right(px(0.0)));
        v_flex()
            .id(id)
            .relative()
            .w(px(RAIL_WIDTH))
            .h(px(RAIL_WIDTH))
            .items_center()
            .justify_center()
            .rounded(px(6.0))
            .cursor_pointer()
            .hover(|style| style.bg(pal.card_hover))
            .when(active, |this| this.bg(pal.card_hover))
            .child(
                icon.with_size(ElementSize::ElementSize(px(18.0)))
                    .text_color(color),
            )
            .when_some(badge, |this, count| {
                this.child(
                    div()
                        .absolute()
                        .top(px(2.0))
                        .right(px(2.0))
                        .h(px(15.0))
                        .min_w(px(15.0))
                        .px_1()
                        .items_center()
                        .justify_center()
                        .rounded_full()
                        // 角标用强调色打底、边栏底色当字色，明暗主题下都读得清。
                        .bg(pal.accent)
                        .text_xs()
                        .text_color(pal.sidebar_bg)
                        .child(count.to_string()),
                )
            })
            .when(active, |this| this.child(stripe))
            .tooltip(move |window, cx| Tooltip::new(tip.clone()).build(window, cx))
            // 稍作延迟再浮出，避免鼠标路过时闪一下文字。
            .tooltip_show_delay(Duration::from_millis(300))
            .on_click(cx.listener(move |this, _, window, cx| {
                on_click(this, window, cx);
            }))
            .into_any_element()
    }

    /// 边栏面板：按当前页渲染主机列表 / 文件 / 转发。
    pub(super) fn render_sidebar_panel(&self, cx: &mut Context<Self>) -> AnyElement {
        let pal = self.palette(cx);
        let content = match self.sidebar_page {
            SidebarPage::Hosts => self.render_hosts_page(cx),
            SidebarPage::Files => self.render_files_section(cx),
            SidebarPage::Local => self.render_local_section(cx),
            SidebarPage::Forward => self.render_forward_section(cx),
        };
        // 页面内容需要在竖向 flex 里拿满高度：块级容器会让 flex_1 的子元素塌成 0。
        v_flex()
            .flex_1()
            .min_w_0()
            .h_full()
            .overflow_hidden()
            .bg(pal.sidebar_bg)
            .child(content)
            .into_any_element()
    }

    pub(super) fn persist_sidebar(&self, cx: &mut Context<Self>) {
        let json = self.sidebar.to_json();
        let Some(vault) = self.vault.clone() else {
            self.store.save_sidebar(&json);
            return;
        };
        cx.spawn(async move |_, _| {
            if let Err(error) = vault.set_setting("sidebar", &json).await {
                log::warn!("写入库内分组偏好失败: {error}");
            }
        })
        .detach();
    }
}

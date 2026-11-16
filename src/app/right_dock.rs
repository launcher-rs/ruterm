//! 右侧停靠区：命令片段（上）与命令历史（下）共用同一块面板。
//!
//! 左缘分隔条调宽度；两块同时展开时，中间分隔条调上下占比。
//! 任一区展开就保留停靠区，两块都收起时终端拿回整块宽度。

use rgpui::prelude::*;
use rgpui::{
    AnyElement, Context, DragMoveEvent, IntoElement, MouseButton, MouseDownEvent, MouseUpEvent,
    ParentElement, Styled, Window, canvas, div, px, relative, v_flex,
};

use super::*;

/// 右侧停靠区的默认宽度和可拖拽范围。
pub(super) const RIGHT_PANEL_DEFAULT_WIDTH: f32 = 340.0;
const RIGHT_PANEL_MIN_WIDTH: f32 = 260.0;
const RIGHT_PANEL_MAX_WIDTH: f32 = 560.0;
/// 上下两块同时展开时，片段区默认占的高度比。
pub(super) const SNIPPET_SHARE_DEFAULT: f32 = 0.42;
/// 上下两块各自的最小高度：拖到这里就停，彻底收起交给导轨按钮。
const SECTION_MIN_HEIGHT: f32 = 120.0;

/// 停靠区左缘分隔条的拖拽载荷；与边栏分隔条分开，避免拖一处两边都动。
#[derive(Clone)]
pub(super) struct RightDockDividerDrag;

impl Render for RightDockDividerDrag {
    fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
        rgpui::Empty
    }
}

/// 停靠区内部上下分界条的拖拽载荷。
#[derive(Clone)]
pub(super) struct DockSplitDrag;

impl Render for DockSplitDrag {
    fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
        rgpui::Empty
    }
}

impl RuTermApp {
    /// 任一分区展开就显示停靠区。
    pub(super) fn right_dock_open(&self) -> bool {
        self.snippets_panel_open || self.history_panel_open
    }

    /// 片段区开关：只剩这一块展开时自动占满高度。
    pub(super) fn toggle_snippets_panel(&mut self, cx: &mut Context<Self>) {
        self.snippets_panel_open = !self.snippets_panel_open;
        if self.snippets_panel_open {
            // 打开时对齐当前会话平台；没有自己的片段就先落在内置命令页，别开出一块空地。
            if let Some(platform) = self.active_session_platform() {
                self.snippet_panel_platform = platform;
            }
            if self.snippets.is_empty() {
                self.snippet_panel_builtin = true;
            }
        } else {
            // 收起面板也丢弃未保存的编辑，避免下次打开弹出半截表单。
            self.snippet_edit = None;
        }
        cx.notify();
    }

    /// 右侧停靠区：上片段、下历史，中间夹一条可拖的上下分隔条。
    pub(super) fn render_right_dock(&self, cx: &mut Context<Self>) -> AnyElement {
        let width = self.right_panel_width;
        let both = self.snippets_panel_open && self.history_panel_open;
        let share = self.snippet_share;
        let height_sink = self.right_panel_height.clone();
        let mut dock = v_flex()
            .id("right-dock")
            .relative()
            .w(px(width))
            .h_full()
            .min_w_0()
            .overflow_hidden()
            // 回填停靠区高度给上下拖拽换算占比；绝对定位不参与布局。
            .child(
                canvas(
                    move |bounds, _, _| height_sink.set(bounds.size.height.as_f32()),
                    |_, _, _, _| {},
                )
                .absolute()
                .size_full(),
            );
        if self.snippets_panel_open {
            dock = dock.child(
                div()
                    .w_full()
                    .min_h_0()
                    .overflow_hidden()
                    .when(both, |this| this.h(relative(share)))
                    .when(!both, |this| this.flex_1())
                    .child(self.render_snippets_section(width, cx)),
            );
        }
        if both {
            dock = dock.child(self.render_dock_splitter(cx));
        }
        if self.history_panel_open {
            dock = dock.child(
                div()
                    .w_full()
                    .min_h_0()
                    .flex_1()
                    .overflow_hidden()
                    .child(self.render_history_panel(width, cx)),
            );
        }
        dock.into_any_element()
    }

    /// 上下两块之间的分隔条：4px 可见，按下即可拖动。
    fn render_dock_splitter(&self, cx: &mut Context<Self>) -> AnyElement {
        let pal = self.palette(cx);
        div()
            .id("right-dock-splitter")
            .occlude()
            .flex_shrink_0()
            .w_full()
            .h(px(4.0))
            .bg(pal.border)
            .cursor_row_resize()
            .hover(move |style| style.bg(pal.accent))
            .on_mouse_down(MouseButton::Left, cx.listener(Self::begin_dock_split))
            .on_drag(DockSplitDrag, |drag, _, _, cx| {
                cx.stop_propagation();
                cx.new(|_| drag.clone())
            })
            .on_drag_move(cx.listener(Self::drag_dock_split))
            .on_mouse_up_out(MouseButton::Left, cx.listener(Self::end_dock_split))
            .into_any_element()
    }

    fn begin_dock_split(&mut self, event: &MouseDownEvent, _: &mut Window, cx: &mut Context<Self>) {
        self.right_split_drag = Some((event.position.y.as_f32(), self.snippet_share));
        cx.notify();
    }

    fn drag_dock_split(
        &mut self,
        event: &DragMoveEvent<DockSplitDrag>,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let y = event.event.position.y.as_f32();
        let Some((start_y, start_share)) = self.right_split_drag else {
            return;
        };
        let height = self.right_panel_height.get();
        if height <= 0.0 {
            return;
        }
        let min_share = (SECTION_MIN_HEIGHT / height).min(0.5);
        let share = (start_share + (y - start_y) / height).clamp(min_share, 1.0 - min_share);
        if (share - self.snippet_share).abs() > f32::EPSILON {
            self.snippet_share = share;
            cx.notify();
        }
    }

    fn end_dock_split(&mut self, _: &MouseUpEvent, _: &mut Window, cx: &mut Context<Self>) {
        if self.right_split_drag.take().is_some() {
            cx.notify();
        }
    }

    /// 主区与停靠区之间的分隔条：拖动改停靠区宽度。
    pub(super) fn render_right_dock_divider(&self, cx: &mut Context<Self>) -> AnyElement {
        let pal = self.palette(cx);
        div()
            .id("right-dock-divider")
            .occlude()
            .flex_shrink_0()
            .w(px(4.0))
            .h_full()
            .bg(pal.border)
            .cursor_col_resize()
            .hover(move |style| style.bg(pal.accent))
            .on_mouse_down(MouseButton::Left, cx.listener(Self::begin_right_dock_drag))
            .on_drag(RightDockDividerDrag, |drag, _, _, cx| {
                cx.stop_propagation();
                cx.new(|_| drag.clone())
            })
            .on_drag_move(cx.listener(Self::drag_right_dock))
            .on_mouse_up_out(MouseButton::Left, cx.listener(Self::end_right_dock_drag))
            .into_any_element()
    }

    fn begin_right_dock_drag(
        &mut self,
        event: &MouseDownEvent,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.right_panel_drag = Some((event.position.x.as_f32(), self.right_panel_width));
        cx.notify();
    }

    fn drag_right_dock(
        &mut self,
        event: &DragMoveEvent<RightDockDividerDrag>,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let x = event.event.position.x.as_f32();
        let Some((start_x, start_width)) = self.right_panel_drag else {
            return;
        };
        let total = self.split_width.get();
        // 左侧给终端至少留 320px；总宽不够时退回最小面板宽，宁可挤终端的标签栏。
        let max = (total - 320.0)
            .min(RIGHT_PANEL_MAX_WIDTH)
            .max(RIGHT_PANEL_MIN_WIDTH);
        let width = (start_width + start_x - x).clamp(RIGHT_PANEL_MIN_WIDTH, max);
        if (width - self.right_panel_width).abs() > f32::EPSILON {
            self.right_panel_width = width;
            cx.notify();
        }
    }

    fn end_right_dock_drag(&mut self, _: &MouseUpEvent, _: &mut Window, cx: &mut Context<Self>) {
        if self.right_panel_drag.take().is_some() {
            cx.notify();
        }
    }
}

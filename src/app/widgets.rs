//! //! 跨模块共用的小部件与帮助函数：输入框样式、步进按钮、右键菜单构建等。


use rgpui::input_ui::{Input, InputState};
use rgpui::{
    App, Button, ButtonVariants, ClickEvent, Context, Entity, IconName, IntoElement, ParentElement, PopupMenu, PopupMenuItem, SharedString,
    Styled, Window, div, px, v_flex,
};

use crate::sessions::AuthMethod;
use crate::tr;

use super::*;

/// 认证方式的展示名（"SSH Agent" 是产品名，两种语言都原样显示）。
pub(super) fn auth_label(cx: &App, method: AuthMethod) -> SharedString {
    match method {
        AuthMethod::Agent => tr!(cx, "SSH Agent"),
        AuthMethod::IdentityFile => tr!(cx, "私钥"),
        AuthMethod::Password => tr!(cx, "密码"),
    }
}

/// 设置分页里的 −/＋ 步进按钮：中性描边小按钮，与「关闭」等按钮保持同一观感。
pub(super) fn stepper(
    id: &'static str,
    label: &'static str,
    on_click: impl Fn(&ClickEvent, &mut Window, &mut App) + Send + Sync + 'static,
) -> Button {
    Button::new(id)
        .label(label)
        .secondary()
        .outline()
        .compact()
        .on_click(on_click)
}

/// 带标题的输入框行。
pub(super) fn labeled_field(
    label: &'static str,
    input: &Entity<InputState>,
    pal: &Palette,
) -> impl IntoElement {
    v_flex()
        .gap_1()
        .child(div().text_sm().child(label))
        .child(styled_input(Input::new(input), pal))
}

/// 统一输入框外观：随主题取底色/文字/边框，保证未聚焦时文字与边框都清晰可辨。
/// TEMP 调试：就地改名链路追踪。
pub(super) fn rdlog(msg: &str) {
    use std::io::Write;
    if let Ok(mut f) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open("temp/rename-debug.log")
    {
        let _ = writeln!(f, "{msg}");
    }
}

pub(super) fn styled_input(input: Input, pal: &Palette) -> Input {
    input
        .bg(pal.input_bg)
        .text_color(pal.input_text)
        .border_1()
        .border_color(pal.input_border)
}

impl RuTermApp {
    /// 侧边栏右键菜单内容，按目标（空白处 / 分组 / 主机行）给出不同条目。
    /// 顶部常驻的「+ 新建主机」大按钮已取消，新建一律走这里或「会话」菜单。
    ///
    /// 走框架的 `PopupMenu`（`deferred + anchored` 独立图层）而不是手搭浮层：
    /// 手搭的绝对定位浮层留在普通图层里，会被后画的兄弟节点压住 hitbox，
    /// 菜单项收不到 mouse-up，点下去只关菜单不触发。
    pub(super) fn context_menu(
        target: MenuTarget,
        groups: Vec<SharedString>,
        current_group: Option<SharedString>,
        entity: Entity<RuTermApp>,
        menu: PopupMenu,
    ) -> PopupMenu {
        let menu = menu.min_w(px(168.0));
        // 「新建主机…」落在右键目标所在的分组：分组上直接进该组，
        // 主机上沿用该主机的组，空白处则是未分组。
        let target_group = match &target {
            MenuTarget::Group(name) if name.as_str() != UNGROUPED => Some(name.clone()),
            MenuTarget::Host(_) => current_group.clone(),
            _ => None,
        };
        let menu = match &target {
            MenuTarget::Host(index) => {
                let index = *index;
                menu.item(Self::menu_item(
                    entity.clone(),
                    "编辑",
                    None,
                    false,
                    false,
                    move |this, window, cx| {
                        this.open_form_at(Some(index), None, window, cx);
                    },
                ))
                .separator()
                .item(Self::menu_item(
                    entity.clone(),
                    "上移一位",
                    None,
                    false,
                    false,
                    move |this, _, cx| this.reorder_host(index, ReorderAction::Up, cx),
                ))
                .item(Self::menu_item(
                    entity.clone(),
                    "下移一位",
                    None,
                    false,
                    false,
                    move |this, _, cx| this.reorder_host(index, ReorderAction::Down, cx),
                ))
                .item(Self::menu_item(
                    entity.clone(),
                    "移到组内最前",
                    None,
                    false,
                    false,
                    move |this, _, cx| this.reorder_host(index, ReorderAction::Top, cx),
                ))
                .separator()
            }
            _ => menu,
        };
        let menu = menu
            .item(Self::menu_item(
                entity.clone(),
                "新建主机…",
                None,
                false,
                false,
                move |this, window, cx| {
                    this.open_form_in_group(target_group.clone(), window, cx);
                },
            ))
            .item(Self::menu_item(
                entity.clone(),
                "本地终端…",
                Some(IconName::SquareTerminal),
                false,
                false,
                |this, _, cx| {
                    this.open_local_terminal(None, cx);
                },
            ))
            .item(Self::menu_item(
                entity.clone(),
                "新建分组…",
                Some(IconName::Folder),
                false,
                false,
                |this, window, cx| {
                    this.open_group_prompt(None, window, cx);
                },
            ));
        let menu = match &target {
            MenuTarget::Group(name) if name.as_str() != UNGROUPED => {
                let renaming = name.clone();
                let deleting = name.clone();
                menu.separator()
                    .item(Self::menu_item(
                        entity.clone(),
                        "重命名分组…",
                        None,
                        false,
                        false,
                        move |this, window, cx| {
                            this.open_group_prompt(Some(renaming.clone()), window, cx);
                        },
                    ))
                    .item(Self::menu_item(
                        entity.clone(),
                        "删除分组",
                        None,
                        true,
                        false,
                        move |this, _, cx| {
                            this.delete_group(&deleting, cx);
                        },
                    ))
            }
            _ => menu,
        };
        let menu = match &target {
            MenuTarget::Host(index) => {
                let index = *index;
                let current = current_group.clone();
                let mut menu = menu.separator().label("移动到分组").item(Self::menu_item(
                    entity.clone(),
                    UNGROUPED,
                    None,
                    false,
                    current.is_none(),
                    move |this, _, cx| {
                        this.move_host(index, None, cx);
                    },
                ));
                for name in groups {
                    let group = name.clone();
                    let checked = current.as_ref() == Some(&name);
                    menu = menu.item(Self::menu_item(
                        entity.clone(),
                        name,
                        Some(IconName::Folder),
                        false,
                        checked,
                        move |this, _, cx| {
                            this.move_host(index, Some(group.clone()), cx);
                        },
                    ));
                }
                menu
            }
            _ => menu,
        };
        // 空白处额外给一个批量入口（主机行菜单已经够长，不重复放）。
        let menu = match &target {
            MenuTarget::Blank => menu.separator().item(Self::menu_item(
                entity.clone(),
                "导入 ~/.ssh/config…",
                None,
                false,
                false,
                |this, _, cx| this.import_ssh_config(cx),
            )),
            _ => menu,
        };
        match target {
            MenuTarget::Host(index) => menu.separator().item(Self::menu_item(
                entity,
                "删除",
                Some(IconName::Delete),
                true,
                false,
                move |this, _, cx| {
                    this.ask_delete(index, cx);
                },
            )),
            _ => menu,
        }
    }


    /// 右键菜单的一项：回调要求 `Send + Sync`，故只捕获实体与标签，不捕获 `&self`。
    pub(super) fn menu_item(
        entity: Entity<RuTermApp>,
        label: impl Into<SharedString>,
        icon: Option<IconName>,
        danger: bool,
        checked: bool,
        run: impl Fn(&mut RuTermApp, &mut Window, &mut Context<RuTermApp>) + Send + Sync + 'static,
    ) -> PopupMenuItem {
        let label = label.into();
        let mut item = PopupMenuItem::new(label).danger(danger).checked(checked);
        if let Some(icon) = icon {
            item = item.icon(icon);
        }
        item.on_click(move |_, window, cx| {
            entity.update(cx, |this, cx| run(this, window, cx));
        })
    }


}

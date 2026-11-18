//! 本机文件页：浏览本机目录，与远端 SFTP 面板共用同一张文件表格。
//!
//! 目录枚举走后台线程（[`RuTermApp::local_load`]），慢盘与网络位置不会卡住 UI 线程；
//! 面板本身只读不写——删除 / 重命名 / 新建这类破坏性动作留给系统文件管理器。

use std::path::{Path, PathBuf};
use std::time::UNIX_EPOCH;

use anyhow::Context as _;
use rgpui::input_ui::Input;
use rgpui::prelude::*;
use rgpui::{
    AnyElement, Button, ButtonVariants, ClipboardItem, Context, ContextMenuExt, Disableable,
    DragMoveEvent, IconName, InteractiveElement, IntoElement, ParentElement, PopupMenuItem,
    ScrollableElement, SharedString, StatefulInteractiveElement, Styled, Window, div, h_flex, px,
    v_flex,
};
use serde::{Deserialize, Serialize};

use crate::tr;

use super::*;

/// 本机目录浏览状态：与远端 [`FileBrowser`] 同构，但住在全局，不随标签生灭。
pub(super) struct LocalBrowser {
    /// 正在显示的目录；None 表示还没成功列出过任何目录。
    pub(super) path: Option<PathBuf>,
    /// 上一次列成功的目录：面板首次展示时回到这里，而不是每次从主目录起步。
    pub(super) listed: bool,
    pub(super) entries: Vec<crate::remote_fs::Entry>,
    pub(super) loading: bool,
    pub(super) error: Option<SharedString>,
    /// 排序依据与方向；目录始终在前（与远端面板同一套规则）。
    pub(super) sort: FileSortKey,
    pub(super) sort_desc: bool,
    /// 类型 / 大小 / 修改时间 / 权限 四列宽度；名称列占剩余空间。
    pub(super) cols: [f32; 4],
    /// 显示隐藏文件（`.` 前缀，或 Windows 的隐藏属性）。
    pub(super) show_hidden: bool,
}

impl Default for LocalBrowser {
    fn default() -> Self {
        Self {
            path: None,
            listed: false,
            entries: Vec::new(),
            loading: false,
            error: None,
            sort: FileSortKey::Name,
            sort_desc: false,
            cols: [
                FILE_COL_KIND_W,
                FILE_COL_SIZE_W,
                FILE_COL_TIME_W,
                FILE_COL_PERM_W,
            ],
            show_hidden: false,
        }
    }
}

impl LocalBrowser {
    /// 从持久化偏好起屏：目录只记着，首次展示时才真的去列（启动路径上不碰磁盘）。
    pub(super) fn from_prefs(prefs: &LocalFilesPrefs) -> Self {
        Self {
            path: prefs.path.as_deref().map(PathBuf::from),
            show_hidden: prefs.show_hidden,
            ..Self::default()
        }
    }

    pub(super) fn sort_entries(&mut self) {
        sort_entries(&mut self.entries, self.sort, self.sort_desc);
    }
}

/// 本机文件页偏好（隐藏文件开关 + 最后停留的目录），存加密库 `settings.local_files`。
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub(super) struct LocalFilesPrefs {
    #[serde(default)]
    pub(super) show_hidden: bool,
    #[serde(default)]
    pub(super) path: Option<String>,
}

impl LocalFilesPrefs {
    pub(super) fn to_json(&self) -> String {
        serde_json::to_string(self).unwrap_or_default()
    }

    pub(super) fn from_json(json: &str) -> Self {
        serde_json::from_str(json).unwrap_or_default()
    }
}

/// 用户主目录：Windows 看 `USERPROFILE`，其余看 `HOME`。
fn home_dir() -> Option<PathBuf> {
    std::env::var_os("USERPROFILE")
        .or_else(|| std::env::var_os("HOME"))
        .map(PathBuf::from)
        .filter(|dir| dir.is_dir())
}

/// 隐藏项判定：Unix 看 `.` 前缀，Windows 看隐藏属性位（`FILE_ATTRIBUTE_HIDDEN`）。
/// 两套规则并存是因为本机页要能看远端面板看不到的东西——自己的机器。
fn is_hidden(name: &str, metadata: &std::fs::Metadata) -> bool {
    if name.starts_with('.') {
        return true;
    }
    #[cfg(windows)]
    {
        use std::os::windows::fs::MetadataExt as _;
        // 2 == FILE_ATTRIBUTE_HIDDEN；系统文件不带这个位，所以不会被当成隐藏项藏起来。
        metadata.file_attributes() & 2 != 0
    }
    #[cfg(not(windows))]
    {
        let _ = metadata;
        false
    }
}

/// 权限列的模式位：Unix 取真实 st_mode 低 9 位，Windows 没有 POSIX 权限，
/// 留 None 让权限格显示 `—`，比编造一个 `rw-rw-rw-` 诚实。
fn mode_of(metadata: &std::fs::Metadata) -> Option<u32> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        Some(metadata.permissions().mode() & 0o7777)
    }
    #[cfg(not(unix))]
    {
        let _ = metadata;
        None
    }
}

/// 列一个本机目录，返回规范化后的路径与条目（目录在前、同类按名称升序）。
///
/// 在后台线程里跑：`read_dir` 是同步阻塞调用，落在网络盘或超大目录时会卡住整个 UI。
fn list_local_dir(
    path: &Path,
    show_hidden: bool,
) -> anyhow::Result<(PathBuf, Vec<crate::remote_fs::Entry>)> {
    // 只做词法规范化（展开 `..`、补成绝对路径），不碰文件系统：`\\?\` 前缀的
    // canonicalize 结果摆在路径栏里没法读。
    let absolute = std::path::absolute(path).unwrap_or_else(|_| path.to_path_buf());
    let read_dir = std::fs::read_dir(&absolute)
        .with_context(|| format!("读取目录失败：{}", absolute.display()))?;

    let mut entries = Vec::new();
    for item in read_dir.flatten() {
        let name = item.file_name().to_string_lossy().into_owned();
        let Ok(metadata) = item.metadata() else {
            // 单项读不到（权限、正被删除）跳过：一行的失败不该让整个目录空掉。
            continue;
        };
        if !show_hidden && is_hidden(&name, &metadata) {
            continue;
        }
        let file_type = metadata.file_type();
        let is_dir = file_type.is_dir();
        entries.push(crate::remote_fs::Entry {
            name,
            is_dir,
            is_symlink: file_type.is_symlink(),
            size: if is_dir { 0 } else { metadata.len() },
            modified: metadata
                .modified()
                .ok()
                .and_then(|time| time.duration_since(UNIX_EPOCH).ok())
                .map(|duration| duration.as_secs().min(u64::from(u32::MAX)) as u32),
            permissions: mode_of(&metadata),
        });
    }
    entries.sort_by(|left, right| {
        right
            .is_dir
            .cmp(&left.is_dir)
            .then_with(|| left.name.to_lowercase().cmp(&right.name.to_lowercase()))
            .then_with(|| left.name.cmp(&right.name))
    });
    Ok((absolute, entries))
}

impl RuTermApp {
    /// 本机页首次展示（或从菜单再次进入）时列目录：优先上次停留的目录，否则主目录。
    pub(super) fn ensure_local_shown(&mut self, cx: &mut Context<Self>) {
        if self.local.listed || self.local.loading {
            return;
        }
        let initial = self.local.path.clone().or_else(home_dir);
        self.local_load(initial, cx);
    }

    /// 列出本机目录；`None` 表示沿用当前目录，都没有则退回主目录。
    pub(super) fn local_load(&mut self, path: Option<PathBuf>, cx: &mut Context<Self>) {
        let Some(target) = path.or_else(|| self.local.path.clone()).or_else(home_dir) else {
            self.local.loading = false;
            self.local.error = Some("找不到可用的本机目录".into());
            cx.notify();
            return;
        };
        let show_hidden = self.local.show_hidden;
        self.local.loading = true;
        self.local.error = None;
        cx.notify();

        cx.spawn(async move |this, cx| {
            let outcome = cx
                .background_spawn(async move { list_local_dir(&target, show_hidden) })
                .await;
            let _ = this.update(cx, |this, cx| {
                this.local.loading = false;
                match outcome {
                    Ok((resolved, entries)) => {
                        this.local.path = Some(resolved);
                        this.local.listed = true;
                        this.local.entries = entries;
                        this.local.sort_entries();
                        this.local.error = None;
                        this.persist_local_files(cx);
                    }
                    Err(error) => {
                        this.local.error = Some(crate::session::error_summary(&error).into());
                    }
                }
                cx.notify();
            });
        })
        .detach();
    }

    /// 进入上级目录；已经到顶（盘符根 / 文件系统根）时不动。
    pub(super) fn local_up(&mut self, cx: &mut Context<Self>) {
        let parent = self
            .local
            .path
            .as_ref()
            .and_then(|path| path.parent())
            .map(PathBuf::from);
        if parent.is_some() {
            self.local_load(parent, cx);
        }
    }

    pub(super) fn local_refresh(&mut self, cx: &mut Context<Self>) {
        let path = self.local.path.clone();
        self.local_load(path, cx);
    }

    /// 「主目录」按钮：无论现在翻到哪，一键回自己的家目录。
    pub(super) fn local_go_home(&mut self, cx: &mut Context<Self>) {
        self.local_load(home_dir(), cx);
    }

    /// 双击条目：目录就进去，文件交给系统默认程序。
    ///
    /// 本机页不「下载到临时目录再打开」——文件本来就在本机，直接开才是它该有的手感。
    pub(super) fn local_open_entry(&mut self, name: &str, is_dir: bool, cx: &mut Context<Self>) {
        let Some(dir) = self.local.path.clone() else {
            return;
        };
        let target = dir.join(name);
        if is_dir {
            self.local_load(Some(target), cx);
            return;
        }
        self.status = format!("已交给系统默认程序打开：{name}").into();
        open_path_with_default(&target);
        cx.notify();
    }

    /// 行菜单「复制路径」：整条绝对路径进剪贴板，粘到终端里就能用。
    pub(super) fn local_copy_path(&mut self, name: &str, cx: &mut Context<Self>) {
        let Some(text) = self
            .local
            .path
            .as_ref()
            .map(|dir| dir.join(name).to_string_lossy().into_owned())
        else {
            return;
        };
        cx.write_to_clipboard(ClipboardItem::new_string(text.clone()));
        self.status = "路径已复制到剪贴板".into();
        cx.notify();
    }

    /// 能接收上传的远端目录：判定与远端面板一致（活动标签、会话活着、已列出目录）。
    fn remote_upload_dir(&self) -> Option<(usize, String)> {
        let tab = self
            .active_tab
            .and_then(|id| self.tabs.iter().find(|tab| tab.id == id))?;
        if tab.commands.is_none() || tab.ended {
            return None;
        }
        Some((tab.id, tab.files.path.clone()?))
    }

    /// 行菜单「上传到远端当前目录」：本机文件 → 活动标签的 SFTP 目录。
    ///
    /// 只送单个文件；目录递归上传远端 SFTP 侧还没做，硬传会变成「跳过」。
    pub(super) fn local_upload_to_remote(&mut self, name: &str, cx: &mut Context<Self>) {
        let Some((tab_id, dir)) = self.remote_upload_dir() else {
            return;
        };
        let Some(path) = self.local.path.as_ref().map(|dir| dir.join(name)) else {
            return;
        };
        self.upload_paths(tab_id, dir, vec![path], cx);
    }

    /// 「显示隐藏文件」开关：翻完立刻重列当前目录，否则点了没变化。
    pub(super) fn toggle_local_hidden(&mut self, cx: &mut Context<Self>) {
        self.local.show_hidden = !self.local.show_hidden;
        self.persist_local_files(cx);
        let path = self.local.path.clone();
        self.local_load(path, cx);
    }

    /// 保存本机页偏好（库可用入库，否则退回 settings.json）。
    pub(super) fn persist_local_files(&self, cx: &mut Context<Self>) {
        let prefs = LocalFilesPrefs {
            show_hidden: self.local.show_hidden,
            path: self
                .local
                .path
                .as_ref()
                .map(|path| path.to_string_lossy().into_owned()),
        };
        let json = prefs.to_json();
        let Some(vault) = self.vault.clone() else {
            self.store.save_local_files(&json);
            return;
        };
        cx.spawn(async move |_, _| {
            if let Err(error) = vault.set_setting("local_files", &json).await {
                log::warn!("写入库内本机文件页偏好失败: {error}");
            }
        })
        .detach();
    }

    /// Ctrl+Shift+L / 菜单项：切到「本机」页；已在该页且面板可见时再按一次收起边栏。
    pub(super) fn toggle_local_panel(
        &mut self,
        action: &crate::ToggleLocalPanel,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let _ = action;
        if self.sidebar_visible(cx) && self.sidebar_page == SidebarPage::Local {
            let pane = self.sidebar_pane();
            self.split_state
                .update(cx, |state, cx| state.toggle_collapse(pane, cx));
        } else {
            self.set_sidebar_page(SidebarPage::Local, window, cx);
        }
        cx.notify();
    }

    /// 本机页：路径栏 + 与远端同一张表格 + 底部摘要与隐藏文件开关。
    pub(super) fn render_local_section(&self, cx: &mut Context<Self>) -> AnyElement {
        let pal = self.palette(cx);
        let narrow = self.pane_narrow(cx);
        let target = Some(PaneTarget::Local);
        let path = self.local.path.clone();
        let entries = self.local.entries.clone();
        let cols = self.local.cols;
        let loading = self.local.loading;
        let error = self.local.error.clone();
        let show_hidden = self.local.show_hidden;

        let editing = self
            .path_edit
            .as_ref()
            .filter(|(pane, _)| *pane == PaneTarget::Local)
            .map(|(_, input)| input.clone());
        let mut path_row = h_flex()
            .w_full()
            .flex_shrink_0()
            .px_2()
            .py_1()
            .gap_1()
            .items_center()
            .border_b_1()
            .border_color(pal.border);
        path_row = path_row.child(if let Some(input) = editing {
            div()
                .flex_1()
                .min_w_0()
                .child(styled_input(Input::new(&input).text_xs(), &pal))
                .into_any_element()
        } else {
            div()
                .flex_1()
                .min_w_0()
                .id("local-path-click")
                .cursor_pointer()
                .text_ellipsis()
                .text_xs()
                .text_color(pal.muted)
                .child(match path.as_ref() {
                    Some(path) => path.to_string_lossy().into_owned(),
                    None => tr!(cx, "点击编辑路径，回车跳转（可直接输入 D:\\ 换盘）").to_string(),
                })
                .on_click(cx.listener(|this, _, window, cx| {
                    this.start_path_edit(PaneTarget::Local, window, cx);
                }))
                .into_any_element()
        });
        let up_enabled = path.as_ref().and_then(|path| path.parent()).is_some();
        path_row = path_row
            .child(
                Button::new("local-up")
                    .icon(IconName::ArrowUp)
                    .compact()
                    .ghost()
                    .tab_index(-1)
                    .disabled(!up_enabled)
                    .tooltip(if up_enabled {
                        tr!(cx, "上级目录")
                    } else {
                        tr!(cx, "已经在根目录（换盘请在路径栏输入盘符）")
                    })
                    .on_click(cx.listener(|this, _, _, cx| this.local_up(cx))),
            )
            .child(
                Button::new("local-refresh")
                    .label("⟳")
                    .compact()
                    .ghost()
                    .tab_index(-1)
                    .tooltip(tr!(cx, "刷新目录"))
                    .on_click(cx.listener(|this, _, _, cx| this.local_refresh(cx))),
            )
            .child(
                Button::new("local-home")
                    .icon(IconName::Folder)
                    .compact()
                    .ghost()
                    .tab_index(-1)
                    .tooltip(tr!(cx, "回到用户主目录"))
                    .on_click(cx.listener(|this, _, _, cx| this.local_go_home(cx))),
            );

        let count = entries.len();
        let total: u64 = entries
            .iter()
            .filter(|entry| !entry.is_dir)
            .map(|entry| entry.size)
            .sum();
        let mut summary_parts: Vec<SharedString> = vec![tr!(cx, "{count} 个项目", count = count)];
        if total > 0 {
            summary_parts.push(SharedString::from(crate::transfer::format_bytes(total)));
        }
        let summary = SharedString::from(
            summary_parts
                .iter()
                .map(|part| part.as_str())
                .collect::<Vec<_>>()
                .join(" · "),
        );

        let entity = cx.entity();
        // 上传落点跟随活动标签的远端目录；没目录就没有「上传到远端当前目录」这一项可用。
        let remote_ready = self.remote_upload_dir().is_some();
        let list_body: AnyElement = if loading && entries.is_empty() {
            v_flex()
                .flex_1()
                .items_center()
                .justify_center()
                .p_3()
                .child(
                    div()
                        .text_xs()
                        .text_color(pal.muted)
                        .child(tr!(cx, "正在读取本机目录…")),
                )
                .into_any_element()
        } else if entries.is_empty() {
            v_flex()
                .flex_1()
                .items_center()
                .justify_center()
                .p_3()
                .child(
                    div()
                        .text_xs()
                        .text_color(pal.muted)
                        .child(if error.is_some() {
                            tr!(cx, "目录读取失败")
                        } else {
                            tr!(cx, "空目录")
                        }),
                )
                .into_any_element()
        } else {
            v_flex()
                .py_1()
                .gap_1()
                .children(entries.iter().map(|entry| {
                    let name = entry.name.clone();
                    let is_dir = entry.is_dir;
                    let cells = RuTermApp::file_row_children(entry, cols, narrow, pal, cx);
                    let dbl_name = name.clone();
                    let open_label = if is_dir {
                        tr!(cx, "打开目录")
                    } else {
                        tr!(cx, "用默认程序打开")
                    };
                    let copy_label = tr!(cx, "复制路径");
                    let upload_label = if remote_ready {
                        tr!(cx, "上传到远端当前目录")
                    } else {
                        tr!(cx, "上传到远端当前目录（需连上 SSH 并打开远端文件页）")
                    };
                    let opener = entity.clone();
                    let copier = entity.clone();
                    let uploader = entity.clone();
                    h_flex()
                        .id(SharedString::from(format!("local-row-{name}")))
                        .w_full()
                        .px_2()
                        .py_1()
                        .gap_1()
                        .items_center()
                        .rounded(px(4.0))
                        .hover(|style| style.bg(pal.card_hover))
                        .cursor_pointer()
                        .children(cells)
                        .on_double_click(cx.listener(move |this, _, _, cx| {
                            this.local_open_entry(&dbl_name, is_dir, cx);
                        }))
                        // 右键菜单的构造闭包是 'static（只拿得到 Entity），取不到 cx，
                        // 所以词条在行渲染时先取好再移进闭包。
                        .context_menu(move |menu, _, _| {
                            let opener = opener.clone();
                            let opening_name = name.clone();
                            let copier = copier.clone();
                            let copy_name = name.clone();
                            let menu = menu
                                .item(PopupMenuItem::new(open_label.clone()).on_click(
                                    move |_, _, cx| {
                                        opener.update(cx, |this, cx| {
                                            this.local_open_entry(&opening_name, is_dir, cx);
                                        });
                                    },
                                ))
                                .item(PopupMenuItem::new(copy_label.clone()).on_click(
                                    move |_, _, cx| {
                                        copier.update(cx, |this, cx| {
                                            this.local_copy_path(&copy_name, cx);
                                        });
                                    },
                                ));
                            if is_dir {
                                return menu;
                            }
                            let uploading = uploader.clone();
                            let upload_name = name.clone();
                            menu.item(
                                PopupMenuItem::new(upload_label.clone())
                                    .disabled(!remote_ready)
                                    .on_click(move |_, _, cx| {
                                        uploading.update(cx, |this, cx| {
                                            this.local_upload_to_remote(&upload_name, cx);
                                        });
                                    }),
                            )
                        })
                }))
                .into_any_element()
        };

        let list = div()
            .id("local-file-list")
            .flex_1()
            .min_h_0()
            .overflow_y_scrollbar()
            .child(list_body);

        let head_labels = (
            tr!(cx, "名称"),
            tr!(cx, "类型"),
            tr!(cx, "大小"),
            tr!(cx, "修改时间"),
            tr!(cx, "权限"),
        );
        let head_row = h_flex()
            .w_full()
            .flex_shrink_0()
            .px_2()
            .py_1()
            .gap_1()
            .items_center()
            .border_b_1()
            .border_color(pal.border)
            .child(div().w(px(FILE_ROW_LEAD_W)).flex_shrink_0())
            .child(self.file_sort_header(
                "local-sort-name",
                head_labels.0,
                FileSortKey::Name,
                None,
                false,
                target,
                cx,
            ))
            .child(self.file_sort_header(
                "local-sort-kind",
                head_labels.1,
                FileSortKey::Kind,
                Some((0, cols[0])),
                false,
                target,
                cx,
            ))
            .child(self.file_sort_header(
                "local-sort-size",
                head_labels.2,
                FileSortKey::Size,
                Some((1, cols[1])),
                true,
                target,
                cx,
            ))
            .when(!narrow, |this| {
                this.child(self.file_sort_header(
                    "local-sort-time",
                    head_labels.3.clone(),
                    FileSortKey::Modified,
                    Some((2, cols[2])),
                    false,
                    target,
                    cx,
                ))
                .child(self.file_sort_header(
                    "local-sort-perm",
                    head_labels.4,
                    FileSortKey::Permissions,
                    Some((3, cols[3])),
                    false,
                    target,
                    cx,
                ))
            })
            .on_drag_move::<ColumnDrag>(cx.listener(
                |this, event: &DragMoveEvent<ColumnDrag>, _, cx| {
                    let drag = *event.drag(cx);
                    let Some(start) = this.col_drag else {
                        return;
                    };
                    if start.target != drag.target || start.col != drag.col {
                        return;
                    }
                    let width = (start.start_w + (event.event.position.x.as_f32() - start.start_x))
                        .clamp(FILE_COL_MIN_W, FILE_COL_MAX_W);
                    if this.set_pane_col(drag.target, drag.col, width) {
                        cx.notify();
                    }
                },
            ));

        let mut footer = v_flex()
            .w_full()
            .flex_shrink_0()
            .px_2()
            .py_1()
            .gap_1()
            .border_t_1()
            .border_color(pal.border);
        if let Some(error) = error {
            footer = footer.child(div().text_xs().text_color(pal.danger).child(error));
        }
        footer = footer.child(
            h_flex()
                .w_full()
                .items_center()
                .justify_between()
                .child(div().text_xs().text_color(pal.muted).child(summary))
                .child(
                    Button::new("local-hidden-toggle")
                        .icon(if show_hidden {
                            IconName::Eye
                        } else {
                            IconName::EyeOff
                        })
                        .compact()
                        .ghost()
                        .tab_index(-1)
                        .selected(show_hidden)
                        .tooltip(tr!(cx, "显示隐藏文件（点开切换，选择会记住）"))
                        .on_click(cx.listener(|this, _, _, cx| this.toggle_local_hidden(cx))),
                ),
        );

        v_flex()
            .id("local-files-section")
            .flex_1()
            .min_h_0()
            .w_full()
            .child(path_row)
            .child(head_row)
            .child(list)
            .child(footer)
            .into_any_element()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 本机列目录：隐藏项按开关过滤、目录排在前面、路径不存在要报错而不是吞掉。
    #[test]
    fn list_local_dir_filters_hidden_and_reports_errors() {
        let root = std::env::temp_dir().join(format!("ruterm-local-files-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).unwrap();
        std::fs::create_dir_all(root.join("sub")).unwrap();
        std::fs::write(root.join("note.txt"), b"hi").unwrap();
        std::fs::write(root.join(".secret"), b"hi").unwrap();

        let names = |show_hidden: bool| -> Vec<String> {
            let mut names: Vec<_> = list_local_dir(&root, show_hidden)
                .unwrap()
                .1
                .into_iter()
                .map(|entry| entry.name)
                .collect();
            // read_dir 的顺序不保证，比对前先按名称排好。
            names.sort();
            names
        };
        assert_eq!(names(false), ["note.txt", "sub"]);
        assert_eq!(names(true), [".secret", "note.txt", "sub"]);
        assert!(list_local_dir(&root.join("does-not-exist"), false).is_err());

        let mut entries = list_local_dir(&root, true).unwrap().1;
        entries.sort_by_key(|entry| entry.name.clone());
        // 目录恒在前，升序时点文件排在普通文件之前。
        sort_entries(&mut entries, FileSortKey::Name, false);
        let sorted: Vec<_> = entries.iter().map(|entry| entry.name.as_str()).collect();
        assert_eq!(sorted, ["sub", ".secret", "note.txt"]);

        let _ = std::fs::remove_dir_all(&root);
    }
}

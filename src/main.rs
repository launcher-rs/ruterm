//! RuTerm：基于 rgpui + rgpui-term 外接 IO 后端的 SSH 客户端。
//!
//! 形态为 GUI 会话管理器：无参数启动即进入主界面，左侧是持久化的主机账号列表，
//! 点击账号即连接；密码认证的账号在连接时临时输入密码（绝不落盘），私钥/Agent
//! 认证直接连接。仍兼容一次性命令行快速连接（带目标参数时首启动即连上该主机）。

#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod app;
mod config;
mod desktop;
mod discovery;
mod i18n;
mod instance;
mod local_shell;
mod macros;
mod paths;
mod remote;
mod remote_fs;
mod session;
mod sessions;
mod ssh;
mod ssh_config;
mod telnet;
mod transfer;
mod vault;
mod zmodem;

use rgpui::prelude::*;
use rgpui::{
    App, Bounds, Icon, IconNamed, IntoElement, KeyBinding, NoAction, SharedString, TrayIconEvent,
    Window, WindowBounds, WindowOptions, actions, icon_named, px, size,
};

use crate::app::RuTermApp;
use crate::config::SshTarget;
use crate::paths::StorageMode;
use crate::sessions::SessionStore;

/// 系统托盘开关：启动前按 settings.json 设定，设置里改动即时生效。
/// 关闭时连图标都不创建，关窗即退出；win32 没有可靠的「删除图标」接口，
/// 运行中关闭只是把图标置空，完整移除要等下次启动。
pub(crate) static TRAY_ENABLED: std::sync::atomic::AtomicBool =
    std::sync::atomic::AtomicBool::new(false);

/// 托盘图标（32px PNG，内嵌避免依赖运行时路径）。
pub(crate) const TRAY_ICON: &[u8] = include_bytes!("../assets/icons/tray-32.png");

/// 当前是否启用托盘（窗口关闭回调与托盘同步都读这里）。
pub(crate) fn tray_enabled() -> bool {
    TRAY_ENABLED.load(std::sync::atomic::Ordering::Relaxed)
}

// 文件面板自己的图标（来自 lucide，ISC 许可）：rgpui 内置集合里没有合适的
// 「定位」「跟随」语义。`icon_named!` 会把 assets/lucide 下的 SVG 编译进二进制。
icon_named!(FileIcon, "assets/lucide");

impl RenderOnce for FileIcon {
    fn render(self, _: &mut Window, _: &mut App) -> impl IntoElement {
        Icon::new(self)
    }
}

// 应用级动作：既绑到全局快捷键，也挂在标题栏菜单项上。rgpui 会按键绑定
// 在菜单项右侧自动渲染按键（`PopupMenu::render_key_binding`），因此不再需要在
// 「帮助」菜单里用纯文字罗列快捷键。
actions!(
    ruterm,
    [
        ToggleSidebar,
        ToggleFilePanel,
        ToggleLocalPanel,
        ToggleTheme,
        NewHost,
        NewLocalTerminal,
        NewRemoteDesktop,
        NextTab,
        PrevTab,
        CloseTab,
        CancelRename,
        OpenSettings,
        OpenForward,
        SplitRight,
        SplitDown,
        CloseSplit,
        FindInTerminal,
        CloseFind,
        ToggleSyncInput,
        BroadcastCommand,
        CancelBroadcast,
        MacroManager,
        CloseMacroManager,
        // 终端剪贴板：Ctrl+C 有选区就复制、没选区把 ^C 送给 shell；
        // Ctrl+Shift+C 只复制（不误发中断），Ctrl+V 粘贴。
        TerminalCopy,
        TerminalCopyText,
        TerminalPaste
    ]
);

fn main() {
    rgpui::init_logging();

    // 带目标参数 → 一次性快速连接兜底；无参数 → 纯 GUI 会话管理器。
    let quick = if std::env::args().nth(1).is_some() {
        match SshTarget::from_args() {
            Ok(target) => Some(target),
            Err(error) => {
                eprintln!("{error}");
                std::process::exit(2);
            }
        }
    } else {
        None
    };

    // 单实例：关窗口只是隐藏到托盘，再双击 exe 会又起一个进程读写同一个加密库。
    // 抢不到锁的这次启动负责把主实例的窗口带回前台，然后自己退出。
    let claim = instance::claim();
    if let instance::Claim::Activated = claim {
        println!("RuTerm 已在运行（可能藏在系统托盘里），已把它的窗口带到前台。");
        return;
    }

    let app = rgpui_platform::application();
    app.run(move |cx| {
        rgpui::tokio::init(cx);
        // 主题 + 输入子系统必须初始化：否则输入框无配色且
        // 退格/删除等键位动作未注册（无法编辑已输入内容）。
        rgpui::theme::init(cx);
        // theme::init 默认切到亮色；库内主题要等开库后才能读到，这里先按
        // 旧 settings.json（降级/迁移前）或默认暗色起窗，避免闪一下亮色。
        let store = SessionStore::new(&StorageMode::detect());
        let settings = store.load_settings();
        let dark = settings.dark();
        // 托盘必须在建窗前决定：关闭时窗口直接退出，不留下后台进程。
        TRAY_ENABLED.store(settings.tray, std::sync::atomic::Ordering::Relaxed);
        // 语言管理器同样必须在开窗前装好：首帧的标题栏菜单、侧栏与状态栏就要取词。
        // 读的是 locale.txt 镜像（不是 settings.json，见 SessionStore::locale_path）。
        i18n::install(&store.load_language(), cx);
        rgpui::theme::Theme::change(
            if dark {
                rgpui::theme::ThemeMode::Dark
            } else {
                rgpui::theme::ThemeMode::Light
            },
            None,
            cx,
        );
        rgpui::input_ui::init(cx);
        // 弹窗菜单的键位（Esc 关闭、方向键选择、回车确认）：不初始化时右键菜单
        // 只能用鼠标点，键盘完全关不掉。
        rgpui::menu::init(cx);
        // 全局快捷键：与菜单项上的 action 同名，菜单里会自动显示这组按键。
        cx.bind_keys([
            KeyBinding::new("ctrl-b", ToggleSidebar, None),
            // 文件面板：与「会话」菜单里的“远端文件面板”同一条路径。
            KeyBinding::new("ctrl-shift-b", ToggleFilePanel, None),
            // 本机文件页：远端面板是 ctrl-shift-b，本机加一个 L（local）。
            KeyBinding::new("ctrl-shift-l", ToggleLocalPanel, None),
            KeyBinding::new("ctrl-t", ToggleTheme, None),
            KeyBinding::new("ctrl-n", NewHost, None),
            KeyBinding::new("ctrl-shift-t", NewLocalTerminal, None),
            // 远程桌面（VNC）：R 取 remote，与本地终端的 T 相邻。
            KeyBinding::new("ctrl-shift-r", NewRemoteDesktop, None),
            // 标签多到溢出后光靠鼠标滚动不够快，配上循环切换。
            KeyBinding::new("ctrl-tab", NextTab, None),
            KeyBinding::new("ctrl-shift-tab", PrevTab, None),
            KeyBinding::new("ctrl-pagedown", NextTab, None),
            KeyBinding::new("ctrl-pageup", PrevTab, None),
            // 关标签：终端里的 Ctrl+W 要留给 shell（删词），所以在 Terminal 上下文用 NoAction
            // 抢占、让它原样落到 PTY；焦点在边栏 / 标签栏 / 设置里时 Ctrl+W 才关标签。
            // 想在手敲终端时关标签，用不跟 shell 冲突的 Ctrl+Shift+W。
            KeyBinding::new("ctrl-w", CloseTab, None),
            KeyBinding::new("ctrl-w", NoAction {}, Some("Terminal")),
            KeyBinding::new("ctrl-shift-w", CloseTab, None),
            // 就地改标签名时 Esc 取消（与查找条同一套路，不影响终端里的 Esc）。
            KeyBinding::new("escape", CancelRename, Some("TabRename")),
            KeyBinding::new("ctrl-,", OpenSettings, None),
            // 终端内查找：Esc 只在查找条里关闭（终端里的 Esc 仍原样送给 shell）。
            KeyBinding::new("ctrl-f", FindInTerminal, None),
            KeyBinding::new("escape", CloseFind, Some("FindBar")),
            KeyBinding::new("escape", CancelBroadcast, Some("BroadcastDialog")),
            // 文件传输：给当前活动会话开 SFTP 上传 / 下载弹窗。
            // 端口转发：给当前活动会话配置本地监听 → 远端目标。
            KeyBinding::new("ctrl-shift-f", OpenForward, None),
            // 分屏：左右 / 上下；同方向再按一次收起，换方向原地换排布。
            KeyBinding::new("ctrl-shift-d", SplitRight, None),
            KeyBinding::new("ctrl-shift-e", SplitDown, None),
            // 键宏管理器：与「会话」菜单里的「键宏…」同一条路径；Esc 只在该弹窗内关闭。
            KeyBinding::new("ctrl-shift-m", MacroManager, None),
            KeyBinding::new("escape", CloseMacroManager, Some("MacroManager")),
            // Tab 在 rgpui 的 Root 上下文里被绑成焦点切换，而按键绑定分发早于 on_key_down，
            // 终端元素因此永远收不到 \x09（shell 补全失效）。在 Terminal 上下文用 NoAction
            // 抢占：它比 Root 更深，分发解析到它就停止，按键原样落到 on_key_down 转发给 PTY。
            KeyBinding::new("tab", NoAction {}, Some("Terminal")),
            KeyBinding::new("shift-tab", NoAction {}, Some("Terminal")),
            // 终端复制粘贴：只在终端持有焦点时生效。Ctrl+C 走「有选区复制、无选区
            // 中断」的经典终端语义；Ctrl+Shift+C 永远只复制，避免误发 ^C。
            // Ctrl+V 被抢占后不会再作为 ^V 送给 shell，与 Windows Terminal 一致。
            KeyBinding::new("ctrl-c", TerminalCopy, Some("Terminal")),
            KeyBinding::new("ctrl-shift-c", TerminalCopyText, Some("Terminal")),
            KeyBinding::new("ctrl-v", TerminalPaste, Some("Terminal")),
            KeyBinding::new("ctrl-shift-v", TerminalPaste, Some("Terminal")),
            KeyBinding::new("shift-insert", TerminalPaste, Some("Terminal")),
        ]);
        let opened = cx.open_window(
            WindowOptions {
                window_bounds: Some(WindowBounds::Windowed(Bounds::centered(
                    None,
                    size(px(1100.0), px(720.0)),
                    cx,
                ))),
                titlebar: Some(rgpui::TitlebarOptions {
                    title: Some("RuTerm".into()),
                    // 隐藏系统标题栏，改由应用自绘（品牌 + 菜单 + 窗口控制按钮）。
                    appears_transparent: true,
                    ..Default::default()
                }),
                // 客户区装饰：窗口样式改为 WS_THICKFRAME 无边框（DWM 仍画圆角与可拖边框），
                // 拖拽与窗口控制由标题栏里显式登记的 WindowControlArea 命中区交给系统。
                app_owns_titlebar_drag: true,
                ..Default::default()
            },
            |window, cx| {
                let quick = quick.clone();
                let app = cx.new(|cx| RuTermApp::new(quick, window, cx));
                // 启用托盘时关闭按钮改为隐藏：窗口不销毁，会话后台保活；
                // 返回 false 中断关闭，真正退出走托盘"退出"（cx.quit 直接投 WM_QUIT，不经此回调）。
                // 未启用托盘则按普通窗口关闭，直接退出进程。
                window.on_window_should_close(cx, |window, _cx| {
                    if tray_enabled() {
                        window.hide_window();
                        false
                    } else {
                        true
                    }
                });
                app
            },
        );
        let window_handle = match opened {
            Ok(handle) => handle,
            Err(_) => {
                cx.quit();
                return;
            }
        };
        cx.activate(true);

        // 单实例锁的持有侧：后来的进程敲门 → 把主窗口带回前台。
        // 收敲门必须跑在 tokio 线程池上（rgpui 的前台执行器没有 IO reactor），
        // 因此用通道把事件递回主线程，再在应用上下文里激活窗口。
        if let instance::Claim::Primary(listener) = claim {
            let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
            let knock_loop = rgpui::tokio::Tokio::spawn(
                cx,
                instance::serve(listener, move || {
                    let _ = tx.send(());
                }),
            );
            let handle = window_handle;
            cx.spawn(async move |cx| {
                while rx.recv().await.is_some() {
                    handle.update(cx, |_, window, _| window.activate_window()).ok();
                }
                // 守活 knock_loop：Task 被丢弃会连带取消 tokio 侧的监听。
                drop(knock_loop.await);
            })
            .detach();
        }

        // 系统托盘：品牌图标 + 悬停提示 + 右键菜单（活动会话 / 显示主界面 / 退出），
        // 左键单击图标直接恢复主窗口（Windows 后端左键不弹菜单，菜单仅右键弹出）。
        // 默认关闭：只有设置里启用过才创建图标，否则连回调都收不到事件。
        if tray_enabled() {
            cx.set_tray_icon(Some(TRAY_ICON));
            // 菜单与提示由应用按会话列表同步（隐藏到托盘后也能直接跳回某个标签）。
            let _ = window_handle.update(cx, |app, _, cx| app.sync_tray(cx));
        }
        // 回调始终登记：运行中在设置里启用托盘时，图标与菜单立刻可用。
        cx.on_tray_icon_event(move |event, cx| {
            if matches!(event, TrayIconEvent::LeftClick) {
                window_handle
                    .update(cx, |_, window, _| window.activate_window())
                    .ok();
            }
        });
        cx.on_tray_menu_action(move |id: SharedString, cx: &mut App| {
            if let Some(rest) = id.strip_prefix("tab:") {
                if let Ok(tab_id) = rest.parse::<usize>() {
                    let _ = window_handle.update(cx, |app, window, cx| {
                        window.activate_window();
                        app.activate_tab(tab_id, window, cx);
                    });
                }
                return;
            }
            match id.as_ref() {
                "quit" => cx.quit(),
                "show" => {
                    // 从托盘恢复：把隐藏的主窗口重新带到前台。
                    window_handle
                        .update(cx, |_, window, _| window.activate_window())
                        .ok();
                }
                _ => {}
            }
        });
    });
}

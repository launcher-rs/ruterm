//! RuLink：基于 rgpui + rgpui-term 外接 IO 后端的 SSH 客户端。
//!
//! 首发纵向切片：命令行给出目标，窗口内是一个接在 russh 通道上的终端。

mod config;
mod session;
mod ssh;

use anyhow::Result;
use rgpui::{
    App, AppContext, Context, Entity, FocusHandle, Focusable, IntoElement, ParentElement, Render,
    SharedString, Styled, Task, Window, WindowOptions, div, px, rgb,
};
use rgpui_term::{TerminalBuilder, TerminalView};

use config::SshTarget;

/// RuLink 主视图：持有终端视图与会话任务。
struct RuLinkApp {
    terminal_view: Entity<TerminalView>,
    /// SSH 会话任务；drop 会取消任务，因此必须持有到应用退出。
    _session: Task<Result<()>>,
    focus_handle: FocusHandle,
}

impl RuLinkApp {
    /// 建立外接终端 + 启动 SSH 会话。
    fn new(target: SshTarget, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let mut builder = TerminalBuilder::new_external(None).expect("无法创建外接终端后端");
        let handles = builder
            .take_external_handles()
            .expect("new_external 之后必然存在外接句柄");
        let terminal = cx.new(|cx| builder.subscribe(cx));
        let terminal_view = cx.new(|cx| TerminalView::new(terminal, window, cx));
        let session = session::start(target, handles, cx);
        Self {
            terminal_view,
            _session: session,
            focus_handle: cx.focus_handle(),
        }
    }
}

impl Render for RuLinkApp {
    fn render(&mut self, _window: &mut Window, _cx: &mut Context<Self>) -> impl IntoElement {
        div()
            .size_full()
            .bg(rgb(0x1e1e1e))
            .px(px(4.0))
            .py(px(4.0))
            .child(self.terminal_view.clone())
    }
}

impl Focusable for RuLinkApp {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

fn main() {
    let target = match SshTarget::from_args() {
        Ok(target) => target,
        Err(error) => {
            eprintln!("{error}");
            std::process::exit(2);
        }
    };

    rgpui::init_logging();

    let title: SharedString = format!("RuLink – {}", target.addr()).into();
    let app = rgpui_platform::application();
    app.run(move |cx| {
        rgpui::tokio::init(cx);
        let opened = cx.open_window(
            WindowOptions {
                titlebar: Some(rgpui::TitlebarOptions {
                    title: Some(title.clone()),
                    ..Default::default()
                }),
                ..Default::default()
            },
            |window, cx| {
                let target = target.clone();
                cx.new(|cx| RuLinkApp::new(target, window, cx))
            },
        );
        if opened.is_err() {
            cx.quit();
        }
    });
}

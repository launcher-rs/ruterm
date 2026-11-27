//! 协议层：终端型协议的建连与字节编解码，按协议分目录放置。
//!
//! 这一层只负责"怎么把字节送到对端、怎么从对端收字节"，不碰终端 UI、标签和状态栏。
//! 目前 Telnet 已迁入；SSH 等公共会话管线抽出后再迁，避免一次性搬动两千多行。
//!
//! 所有终端协议共用的句柄是 [`SessionHandle`]：UI 侧据此统一处理
//! "建连成功 / 断线 / 主动关闭"，不因协议不同而分叉。

use std::sync::Arc;
use std::sync::atomic::AtomicBool;

use anyhow::Result;
use rgpui::Task;
use tokio::sync::oneshot;

use crate::sessions::ProtocolKind;

pub(crate) mod serial;
pub(crate) mod ssh;
pub(crate) mod tcp;
pub(crate) mod telnet;
pub(crate) mod ws;

/// 一种协议的静态元数据：展示名、默认端口、会话类别、列表副标题前缀。
///
/// 新增协议时只需在 [`spec`] 里加一条记录，主机表单的分段选项、默认端口、
/// 列表副标题等都从这里取，不必逐个 UI 文件补 `match`。图标因依赖 UI 类型
/// 且 SSH 按认证方式变化，仍由 `app::hosts::host_icon` 负责。
#[derive(Clone, Copy, Debug)]
pub struct ProtocolSpec {
    /// 展示名（表单分段、菜单、状态栏）。
    pub label: &'static str,
    /// 默认端口；0 表示该协议没有端口概念（如串口）。
    pub default_port: u16,
    /// 是否远程桌面协议（点开走桌面标签，不进终端）。
    pub desktop: bool,
    /// 列表副标题前缀；SSH 无前缀（直接显示 user@host:port）。
    pub subtitle_prefix: &'static str,
}

/// 各协议的元数据表。新增协议 = 这里加一条 + `ProtocolKind` 加一个变体。
const fn spec(kind: ProtocolKind) -> ProtocolSpec {
    match kind {
        ProtocolKind::Ssh => ProtocolSpec {
            label: "SSH",
            default_port: 22,
            desktop: false,
            subtitle_prefix: "",
        },
        ProtocolKind::Telnet => ProtocolSpec {
            label: "Telnet",
            default_port: 23,
            desktop: false,
            subtitle_prefix: "Telnet · ",
        },
        ProtocolKind::Tcp => ProtocolSpec {
            label: "TCP",
            // 裸 TCP 没有约定俗成的默认端口：留 0 让表单不预填，保存时校验必填。
            default_port: 0,
            desktop: false,
            subtitle_prefix: "TCP · ",
        },
        ProtocolKind::Ws => ProtocolSpec {
            label: "WS",
            default_port: 80,
            desktop: false,
            subtitle_prefix: "WS · ",
        },
        ProtocolKind::Serial => ProtocolSpec {
            label: "Serial",
            default_port: 0,
            desktop: false,
            subtitle_prefix: "Serial · ",
        },
        ProtocolKind::Rdp => ProtocolSpec {
            label: "RDP",
            default_port: 3389,
            desktop: true,
            subtitle_prefix: "RDP · ",
        },
        ProtocolKind::Vnc => ProtocolSpec {
            label: "VNC",
            default_port: 5900,
            desktop: true,
            subtitle_prefix: "VNC · ",
        },
    }
}

/// 供 `ProtocolKind` 的方法委托，避免元数据散落在多个 UI 文件里各写一遍。
pub(crate) const fn protocol_spec(kind: ProtocolKind) -> ProtocolSpec {
    spec(kind)
}

/// 终端型会话的公共句柄，字段语义对所有协议一致。
///
/// SSH 特有的带外能力（SFTP / 端口转发 / 远程信息）放在 `SshSession` 自己的字段里，
/// 不塞进这里；只依赖字节流的协议（Telnet 及后续串口 / WebSocket）可直接返回本结构。
pub struct SessionHandle {
    /// 会话任务，必须被持有（提前 drop 会取消连接）。
    pub join: Task<Result<()>>,
    /// 是否真正建立过连接（失败在建连阶段时标签按"连接失败"处理）。
    pub started: Arc<AtomicBool>,
    /// 连接成功时发送一次：UI 据此记录"连接真正可用"的时刻。
    pub ready: oneshot::Receiver<()>,
    /// 主动断开信号：标签被关闭时发送，会话任务随即释放连接。
    pub disconnect: oneshot::Sender<()>,
}

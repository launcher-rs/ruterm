//! RFB（VNC）协议客户端：只实现能稳定互通的子集。
//!
//! 握手支持 3.3 / 3.7 / 3.8，安全类型只收 `None` 与 `VNC Authentication`
//! （VeNCrypt / RA2 之类直接报「服务器要求的加密方式不支持」，不静默降级）。
//! 编码只通告 `Hextile` / `CopyRect` / `Raw` 与 `DesktopSize` 伪编码（按偏好排序，
//! 服务器取第一个支持的，压缩编码先行）：
//! 这三样是 TigerVNC、x11vnc、macOS 屏幕共享共同支持的最大公约数，
//! Tight / ZRLE 需要 zlib 流式解码，等真有服务器只肯给它们再加。
//!
//! 服务端给的所有尺寸都按上限校验后再分配：宽度、高度、矩形都必须落在
//! 帧缓冲内，否则整条连接报错断开，而不是带着越界读写继续跑。

use std::time::Duration;

use anyhow::{Context as _, Result, bail};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};

/// 单个方向的最大边长（服务器能报的最大值就 65535，这里留出常见 8K 屏）。
const MAX_DIMENSION: u16 = 8192;
/// 帧缓冲像素上限：16M 像素 ≈ 64 MiB BGRA，挡住「一台服务器要吃掉几个 G」。
const MAX_PIXELS: u32 = 16 * 1024 * 1024;
/// 服务器名的读取上限（协议里是 u32，不能照着分配）。
const MAX_NAME_BYTES: usize = 4096;
/// 服务器剪贴板的读取上限。
const MAX_CLIPBOARD_BYTES: usize = 4 * 1024 * 1024;
/// 握手每一步的等待上限：连上了却不说 RFB 的端口不该把连接挂死。
pub const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(15);
/// 帧更新的等待上限。静止桌面可能长时间没有像素变化，不能按 120 秒就断线；
/// 这里只兜底彻底失联的连接，正常退出走显式 Close。
pub const READ_TIMEOUT: Duration = Duration::from_secs(30 * 60);
/// 单条消息的写入上限：远端不读时不能让发送任务无限期卡住取消。
pub const WRITE_TIMEOUT: Duration = Duration::from_secs(30);

/// 我们通告给服务器的编码（客户端 → 服务器的优先级顺序）。
/// RFB 规定顺序即偏好：服务器会用列表里第一个自己支持的编码。Hextile 压缩后
/// 只有原始像素的几十分之一，WAN 下 Raw 先行等于强制全量未压缩传输，因此把
/// Raw 放在末位兜底；`DesktopSize` 是伪编码，顺序不影响。
pub const ENCODINGS: [i32; 4] = [5, 1, 0, -223];

/// RFB 的 `Rect`：以帧缓冲左上角为原点的像素矩形。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Rect {
    pub x: u16,
    pub y: u16,
    pub width: u16,
    pub height: u16,
}

/// 服务器声明的像素格式（ServerInit 里那 16 字节）。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PixelFormat {
    pub bits_per_pixel: u8,
    pub depth: u8,
    pub big_endian: bool,
    pub true_colour: bool,
    pub red_max: u16,
    pub green_max: u16,
    pub blue_max: u16,
    pub red_shift: u8,
    pub green_shift: u8,
    pub blue_shift: u8,
}

impl PixelFormat {
    /// Validate before any server-controlled pixel data is decoded.
    fn validate(&self) -> Result<()> {
        if !matches!(self.bits_per_pixel, 8 | 16 | 32)
            || self.depth == 0
            || self.depth > self.bits_per_pixel
            || !self.true_colour
        {
            bail!("服务器上报的像素格式无效（仅支持 8/16/32bpp 真彩色）");
        }
        let mut used = 0u32;
        let mut depth = 0;
        for (max, shift) in [
            (self.red_max, self.red_shift),
            (self.green_max, self.green_shift),
            (self.blue_max, self.blue_shift),
        ] {
            let max = u32::from(max);
            let bits = max.count_ones();
            if max == 0
                || max & (max + 1) != 0
                || u32::from(shift) + bits > u32::from(self.bits_per_pixel)
            {
                bail!("服务器上报的颜色最大值或位移无效");
            }
            let mask = max << shift;
            if used & mask != 0 {
                bail!("服务器上报的颜色通道重叠");
            }
            used |= mask;
            depth += bits;
        }
        // 通道位数之和与 depth 不必精确相等：不少服务器把保留 / alpha 位算进
        // depth=32（三通道和只有 24），解码只按 RGB 三通道取值，照常工作。
        // depth 比通道和还小＝报告自相矛盾，才拒绝。
        if u32::from(self.depth) < depth {
            bail!("服务器上报的颜色深度无效");
        }
        Ok(())
    }

    /// 每个像素占几个字节（格式已校验）。
    pub fn bytes_per_pixel(&self) -> usize {
        usize::from(self.bits_per_pixel.div_ceil(8)).max(1)
    }

    /// 把一个像素解成 BGRA（渲染端的 `RenderImage` 就直接吃这个顺序）。
    ///
    /// 不假设 32bpp：老服务器会报 16bpp 565 或 8bpp 332，按 shift / max 通用解，
    /// 通道最大值不是 255 时缩放到 0..=255。
    pub fn decode(&self, pixel: &[u8]) -> [u8; 4] {
        let mut value: u32 = 0;
        let bytes = self.bytes_per_pixel().min(pixel.len());
        if self.big_endian {
            for &byte in &pixel[..bytes] {
                value = (value << 8) | u32::from(byte);
            }
        } else {
            for (index, &byte) in pixel[..bytes].iter().enumerate() {
                value |= u32::from(byte) << (index * 8);
            }
        }
        let channel = |shift: u8, max: u16| -> u8 {
            if max == 0 {
                return 0;
            }
            let raw = (value >> shift) & u32::from(max);
            let scaled = raw * 255 / u32::from(max);
            scaled.min(255) as u8
        };
        [
            channel(self.blue_shift, self.blue_max),
            channel(self.green_shift, self.green_max),
            channel(self.red_shift, self.red_max),
            255,
        ]
    }
}

/// ServerInit 的内容（握手产物）。
#[derive(Clone, Debug)]
pub struct ServerInit {
    pub width: u16,
    pub height: u16,
    pub name: String,
    pub pixel_format: PixelFormat,
}

/// 客户端侧需要跨消息保留的状态。
pub struct ClientState {
    pub init: ServerInit,
    pub fb: Framebuffer,
}

impl ClientState {
    pub fn new(init: ServerInit) -> Result<Self> {
        init.pixel_format.validate()?;
        let fb = Framebuffer::new(init.width, init.height)?;
        Ok(Self { init, fb })
    }
}

/// 一条服务器消息。
pub enum Message {
    /// 一帧更新已写进帧缓冲；`resized` 表示本次更新改了桌面尺寸。
    Update { resized: bool },
    /// 服务器剪贴板（RFB 只传 Latin-1，按 UTF-8 尽力解）。
    Clipboard(String),
    /// 响铃 / 调色板等已就地消化的消息。
    Ignored,
}

/// 读到的服务器版本，只认 3.x。
fn parse_version(text: &[u8; 12]) -> Result<(u8, u8)> {
    let text = std::str::from_utf8(text).context("服务器版本号不是 ASCII")?;
    let rest = text
        .strip_prefix("RFB ")
        .context("服务器没有按 RFB 协议打招呼")?;
    let (major, minor) = rest.split_once('.').context("服务器版本号格式不对")?;
    let major: u8 = major.trim_end().parse().context("版本号主版本不是数字")?;
    let minor: u8 = minor.trim_end().parse().context("版本号次版本不是数字")?;
    if major != 3 {
        bail!("不支持的 RFB 主版本 {major}（只支持 3.x）");
    }
    Ok((major, minor))
}

/// 握手：版本协商 → 选安全类型 → 认证 → ClientInit → ServerInit。
///
/// `password` 只在这一步用于 VNC 认证的 DES 响应，不会留在任何状态里。
pub async fn handshake<S>(
    stream: &mut S,
    password: Option<&str>,
    shared: bool,
) -> Result<ServerInit>
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    let mut version_bytes = [0u8; 12];
    stream
        .read_exact(&mut version_bytes)
        .await
        .context("读取 RFB 版本失败（对端可能不是 VNC 服务）")?;
    let (_, minor) = parse_version(&version_bytes)?;
    // 3.3 之后的握手流程都按 3.8 走；3.7 只是 SecurityResult 细节不同。
    let ours = if minor >= 8 {
        b"RFB 003.008\n"
    } else if minor == 7 {
        b"RFB 003.007\n"
    } else {
        b"RFB 003.003\n"
    };
    stream.write_all(ours).await?;

    let security = negotiate_security(stream, minor, password.is_some()).await?;
    match security {
        SecurityType::None => {
            // 无认证 + 明文：VNC 协议本身如此，但会话可被同网段窃听，明确告警。
            log::warn!("VNC 服务器选择无认证（安全类型 None），会话未加密，输入与画面可被窃听");
            // 3.8 的安全结果对 None 也会发；3.7 / 3.3 不发，等 ClientInit 即可。
            if minor >= 8 {
                read_security_result(stream, minor).await?;
            }
        }
        SecurityType::VncAuth => {
            let Some(password) = password else {
                bail!("服务器要求 VNC 密码");
            };
            let mut challenge = [0u8; 16];
            stream
                .read_exact(&mut challenge)
                .await
                .context("读取 VNC 认证挑战失败")?;
            let response = vnc_auth_response(password, &challenge);
            stream.write_all(&response).await?;
            read_security_result(stream, minor).await?;
        }
    }

    stream.write_all(&[u8::from(shared)]).await?;

    let width = stream.read_u16().await.context("读取桌面宽度失败")?;
    let height = stream.read_u16().await.context("读取桌面高度失败")?;
    check_dimensions(width, height)?;
    let mut pf_bytes = [0u8; 16];
    stream.read_exact(&mut pf_bytes).await?;
    let pixel_format = PixelFormat {
        bits_per_pixel: pf_bytes[0],
        depth: pf_bytes[1],
        big_endian: pf_bytes[2] != 0,
        true_colour: pf_bytes[3] != 0,
        red_max: u16::from_be_bytes([pf_bytes[4], pf_bytes[5]]),
        green_max: u16::from_be_bytes([pf_bytes[6], pf_bytes[7]]),
        blue_max: u16::from_be_bytes([pf_bytes[8], pf_bytes[9]]),
        red_shift: pf_bytes[10],
        green_shift: pf_bytes[11],
        blue_shift: pf_bytes[12],
    };
    pixel_format.validate()?;
    let name_len = stream.read_u32().await.context("读取服务器名长度失败")? as usize;
    if name_len > MAX_NAME_BYTES {
        bail!("服务器名过长（{name_len} 字节）");
    }
    let mut name = vec![0u8; name_len];
    stream.read_exact(&mut name).await?;
    let name = String::from_utf8_lossy(&name).into_owned();

    Ok(ServerInit {
        width,
        height,
        name,
        pixel_format,
    })
}

fn check_dimensions(width: u16, height: u16) -> Result<()> {
    if width == 0 || height == 0 {
        bail!("服务器上报的桌面尺寸无效：{width}×{height}");
    }
    if width > MAX_DIMENSION || height > MAX_DIMENSION {
        bail!("服务器上报的桌面尺寸过大：{width}×{height}");
    }
    if u32::from(width) * u32::from(height) > MAX_PIXELS {
        bail!("服务器上报的桌面像素过多：{width}×{height}");
    }
    Ok(())
}

/// 安全类型：只认无认证与 VNC 认证。
enum SecurityType {
    None,
    VncAuth,
}

async fn negotiate_security<S>(
    stream: &mut S,
    minor: u8,
    has_password: bool,
) -> Result<SecurityType>
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    let security_type = if minor >= 7 {
        let count = stream.read_u8().await.context("读取安全类型数量失败")?;
        if count == 0 {
            let reason = read_reason(stream).await?;
            bail!("服务器拒绝了连接：{reason}");
        }
        let mut types = vec![0u8; usize::from(count)];
        stream.read_exact(&mut types).await?;
        let has_vnc = types.contains(&2);
        let has_none = types.contains(&1);
        // 有密码就优先走认证，免得把服务器配成"可写但无密码"时误用无认证；
        // 没密码时先用无认证，两者都没提供才落到 VncAuth 以便报出缺密码。
        let chosen = if has_vnc && has_password {
            2
        } else if has_none {
            1
        } else if has_vnc {
            2
        } else {
            0
        };
        if chosen == 0 {
            bail!(
                "服务器要求的安全类型不支持（{}）；只支持无认证与 VNC 认证",
                format_types(&types)
            );
        }
        stream.write_all(&[chosen]).await?;
        chosen
    } else {
        let chosen = stream.read_u32().await.context("读取安全类型失败")?;
        if chosen == 0 {
            let reason = read_reason(stream).await?;
            bail!("服务器拒绝了连接：{reason}");
        }
        if chosen > 255 || (chosen != 1 && chosen != 2) {
            bail!("服务器要求的安全类型不支持（{chosen}）");
        }
        chosen as u8
    };
    Ok(if security_type == 2 {
        SecurityType::VncAuth
    } else {
        SecurityType::None
    })
}

fn format_types(types: &[u8]) -> String {
    types
        .iter()
        .map(u8::to_string)
        .collect::<Vec<_>>()
        .join(", ")
}

/// 读失败原因串：3.8 起是 u32 长度 + 字节，之前只有 4 字节结果码。
async fn read_reason<S>(stream: &mut S) -> Result<String>
where
    S: AsyncRead + Unpin,
{
    let length = stream.read_u32().await.unwrap_or(0) as usize;
    if length == 0 || length > MAX_NAME_BYTES {
        return Ok("未提供原因".to_string());
    }
    let mut text = vec![0u8; length];
    stream.read_exact(&mut text).await.ok();
    Ok(String::from_utf8_lossy(&text).into_owned())
}

async fn read_security_result<S>(stream: &mut S, minor: u8) -> Result<()>
where
    S: AsyncRead + Unpin,
{
    let result = stream.read_u32().await.context("读取安全认证结果失败")?;
    if result == 0 {
        return Ok(());
    }
    if minor >= 8 {
        let reason = read_reason(stream).await?;
        bail!("认证失败：{reason}");
    }
    bail!("认证失败（服务器拒绝了密码）");
}

/// VNC 认证的响应：密码取前 8 字节补零，每字节按位颠倒后做 DES-ECB 加密挑战。
pub fn vnc_auth_response(password: &str, challenge: &[u8; 16]) -> [u8; 16] {
    use des::cipher::{Block, BlockCipherEncrypt, KeyInit};

    let mut key = [0u8; 8];
    for (slot, byte) in key.iter_mut().zip(password.bytes().take(8)) {
        *slot = byte.reverse_bits();
    }
    let cipher = des::Des::new_from_slice(&key).expect("DES 密钥固定 8 字节");
    let mut response = [0u8; 16];
    for half in 0..2 {
        let mut block = Block::<des::Des>::try_from(&challenge[half * 8..half * 8 + 8])
            .expect("DES 块长度固定 8 字节");
        cipher.encrypt_block(&mut block);
        response[half * 8..half * 8 + 8].copy_from_slice(block.as_slice());
    }
    response
}

/// 客户端 SetEncodings 消息（握手后发一次）。
pub fn set_encodings_message(encodings: &[i32]) -> Vec<u8> {
    let mut bytes = Vec::with_capacity(4 + encodings.len() * 4);
    bytes.push(2);
    bytes.push(0);
    bytes.extend_from_slice(&(encodings.len() as u16).to_be_bytes());
    for encoding in encodings {
        bytes.extend_from_slice(&encoding.to_be_bytes());
    }
    bytes
}

/// FramebufferUpdateRequest；`incremental` 为 false 时服务器会回一整帧。
pub fn update_request_message(incremental: bool, width: u16, height: u16) -> Vec<u8> {
    let mut bytes = Vec::with_capacity(10);
    bytes.push(3);
    bytes.push(u8::from(incremental));
    bytes.extend_from_slice(&0u16.to_be_bytes());
    bytes.extend_from_slice(&0u16.to_be_bytes());
    bytes.extend_from_slice(&width.to_be_bytes());
    bytes.extend_from_slice(&height.to_be_bytes());
    bytes
}

/// PointerEvent：按钮位图 + 在帧缓冲里的坐标。
pub fn pointer_message(x: u16, y: u16, buttons: u8) -> Vec<u8> {
    let mut bytes = Vec::with_capacity(6);
    bytes.push(5);
    bytes.push(buttons);
    bytes.extend_from_slice(&x.to_be_bytes());
    bytes.extend_from_slice(&y.to_be_bytes());
    bytes
}

/// KeyEvent：键码用的是 X11 keysym（见 [`keysym`]）。
pub fn key_message(keysym: u32, down: bool) -> Vec<u8> {
    let mut bytes = Vec::with_capacity(8);
    bytes.push(4);
    bytes.push(u8::from(down));
    bytes.extend_from_slice(&[0, 0]);
    bytes.extend_from_slice(&keysym.to_be_bytes());
    bytes
}

/// ClientCutText：RFB 只传 Latin-1，非 Latin-1 字符以 `?` 占位。
pub fn clipboard_message(text: &str) -> Vec<u8> {
    let latin1: Vec<u8> = text
        .chars()
        .map(|ch| u8::try_from(ch).unwrap_or(b'?'))
        .collect();
    let mut bytes = Vec::with_capacity(8 + latin1.len());
    bytes.push(6);
    bytes.extend_from_slice(&[0, 0, 0]);
    bytes.extend_from_slice(&(latin1.len() as u32).to_be_bytes());
    bytes.extend_from_slice(&latin1);
    bytes
}

/// 读一条服务器消息并落进帧缓冲。
pub async fn read_message<R>(reader: &mut R, state: &mut ClientState) -> Result<Message>
where
    R: AsyncRead + Unpin,
{
    let kind = reader.read_u8().await.context("读取服务器消息失败")?;
    match kind {
        0 => {
            let resized = read_framebuffer_update(reader, state).await?;
            Ok(Message::Update { resized })
        }
        1 => {
            // SetColourMapEntries：type(1) padding(1) first-colour(2) count(2) [6B]*n。
            // 真彩色像素格式下服务器不该发这条，读对字段只是保证不把流读歪。
            let _padding = reader.read_u8().await?;
            let _first = reader.read_u16().await?;
            let count = reader.read_u16().await?;
            for _ in 0..count {
                let mut entry = [0u8; 6];
                reader.read_exact(&mut entry).await?;
            }
            Ok(Message::Ignored)
        }
        2 => Ok(Message::Ignored),
        3 => {
            let mut pad = [0u8; 3];
            reader.read_exact(&mut pad).await?;
            let length = reader.read_u32().await? as usize;
            if length > MAX_CLIPBOARD_BYTES {
                bail!("服务器剪贴板过长（{length} 字节）");
            }
            let mut bytes = vec![0u8; length];
            reader.read_exact(&mut bytes).await?;
            // RFB 3.3 的 CutText 按规范是 Latin-1，但 Linux 上的服务器
            // （TigerVNC / x11vnc）转发的选择内容常是 UTF-8 编码的 UTF8_STRING：
            // 先按 UTF-8 解，解不通再退回 Latin-1，中文不再变成乱码。
            let text = match std::str::from_utf8(&bytes) {
                Ok(text) => text.to_string(),
                Err(_) => bytes.iter().map(|&byte| char::from(byte)).collect(),
            };
            Ok(Message::Clipboard(text))
        }
        other => bail!("未知的服务器消息类型 {other}"),
    }
}

async fn read_framebuffer_update<R>(reader: &mut R, state: &mut ClientState) -> Result<bool>
where
    R: AsyncRead + Unpin,
{
    let _padding = reader.read_u8().await?;
    let count = reader.read_u16().await?;
    let mut resized = false;
    for _ in 0..count {
        let x = reader.read_u16().await?;
        let y = reader.read_u16().await?;
        let width = reader.read_u16().await?;
        let height = reader.read_u16().await?;
        let encoding = reader.read_i32().await?;
        match encoding {
            0 => {
                apply_raw(
                    reader,
                    state,
                    Rect {
                        x,
                        y,
                        width,
                        height,
                    },
                )
                .await?;
            }
            1 => {
                let src_x = reader.read_u16().await?;
                let src_y = reader.read_u16().await?;
                state.fb.copy_rect(
                    Rect {
                        x: src_x,
                        y: src_y,
                        width,
                        height,
                    },
                    Rect {
                        x,
                        y,
                        width,
                        height,
                    },
                )?;
            }
            5 => {
                apply_hextile(
                    reader,
                    state,
                    Rect {
                        x,
                        y,
                        width,
                        height,
                    },
                )
                .await?;
            }
            -223 => {
                // DesktopSize：矩形宽高即新尺寸，没有像素负载。
                check_dimensions(width, height)?;
                state.fb.resize(width, height);
                state.init.width = width;
                state.init.height = height;
                resized = true;
            }
            other => bail!("服务器发来了未协商的编码 {other}"),
        }
    }
    Ok(resized)
}

/// 校验服务器给的矩形落在帧缓冲内（越界直接报错，不做"猜"）。
fn checked_rect(fb: &Framebuffer, rect: Rect) -> Result<Rect> {
    let right = u32::from(rect.x) + u32::from(rect.width);
    let bottom = u32::from(rect.y) + u32::from(rect.height);
    if rect.width == 0 || rect.height == 0 {
        bail!("服务器发来空矩形");
    }
    if right > u32::from(fb.width) || bottom > u32::from(fb.height) {
        bail!(
            "服务器发来的矩形越界：({},{}) {}×{} 超出 {}×{}",
            rect.x,
            rect.y,
            rect.width,
            rect.height,
            fb.width,
            fb.height
        );
    }
    Ok(rect)
}

async fn apply_raw<R>(reader: &mut R, state: &mut ClientState, rect: Rect) -> Result<()>
where
    R: AsyncRead + Unpin,
{
    let rect = checked_rect(&state.fb, rect)?;
    let bpp = state.init.pixel_format.bytes_per_pixel();
    let bytes = usize::from(rect.width) * usize::from(rect.height) * bpp;
    let mut payload = vec![0u8; bytes];
    reader.read_exact(&mut payload).await?;
    state.fb.blit(rect, &payload, &state.init.pixel_format);
    Ok(())
}

/// Hextile：16×16 小块，每块一个子编码字节。
async fn apply_hextile<R>(reader: &mut R, state: &mut ClientState, rect: Rect) -> Result<()>
where
    R: AsyncRead + Unpin,
{
    let rect = checked_rect(&state.fb, rect)?;
    let pf = state.init.pixel_format;
    let bpp = pf.bytes_per_pixel();
    let mut pixel = [0u8; 4];
    // 背景 / 前景色在整条 Hextile 消息里延续：协议规定未指定时沿用上一个块的颜色。
    let mut background = [0u8; 4];
    let mut foreground = [0u8; 4];
    let mut tiles_y = 0;
    while tiles_y < rect.height {
        let tile_h = (rect.height - tiles_y).min(16);
        let mut tiles_x = 0;
        while tiles_x < rect.width {
            let tile_w = (rect.width - tiles_x).min(16);
            let tile = Rect {
                x: rect.x + tiles_x,
                y: rect.y + tiles_y,
                width: tile_w,
                height: tile_h,
            };
            let subencoding = reader.read_u8().await?;
            if subencoding & 0b0000_0001 != 0 {
                // Raw：整块原样像素。
                let bytes = usize::from(tile_w) * usize::from(tile_h) * bpp;
                let mut payload = vec![0u8; bytes];
                reader.read_exact(&mut payload).await?;
                state.fb.blit(tile, &payload, &pf);
                tiles_x += tile_w;
                continue;
            }
            if subencoding & 0b0000_0010 != 0 {
                reader.read_exact(&mut pixel[..bpp]).await?;
                background = pf.decode(&pixel[..bpp]);
            }
            state.fb.fill(tile, background);
            if subencoding & 0b0000_0100 != 0 {
                reader.read_exact(&mut pixel[..bpp]).await?;
                foreground = pf.decode(&pixel[..bpp]);
            }
            if subencoding & 0b0000_1000 != 0 {
                let coloured = subencoding & 0b0001_0000 != 0;
                let count = reader.read_u8().await?;
                for _ in 0..count {
                    let color = if coloured {
                        reader.read_exact(&mut pixel[..bpp]).await?;
                        pf.decode(&pixel[..bpp])
                    } else {
                        foreground
                    };
                    let xy = reader.read_u8().await?;
                    let wh = reader.read_u8().await?;
                    let sub = Rect {
                        x: tile.x + u16::from(xy >> 4),
                        y: tile.y + u16::from(xy & 0x0f),
                        width: u16::from((wh >> 4) + 1),
                        height: u16::from((wh & 0x0f) + 1),
                    };
                    // 子矩形按协议可以越过块边界一点，按帧缓冲裁掉即可。
                    if let Some(clipped) = state.fb.clip(sub) {
                        state.fb.fill(clipped, color);
                    }
                }
            }
            tiles_x += tile_w;
        }
        tiles_y += tile_h;
    }
    Ok(())
}

/// 帧缓冲：内部固定 BGRA，与 `RenderImage` 的字节顺序一致。
pub struct Framebuffer {
    pub width: u16,
    pub height: u16,
    pub bgra: Vec<u8>,
}

impl Framebuffer {
    pub fn new(width: u16, height: u16) -> Result<Self> {
        check_dimensions(width, height)?;
        Ok(Self {
            width,
            height,
            bgra: vec![0u8; usize::from(width) * usize::from(height) * 4],
        })
    }

    /// 桌面尺寸变了就重建缓冲（旧内容不保留，随后会请求整帧）。
    pub fn resize(&mut self, width: u16, height: u16) {
        if self.width == width && self.height == height {
            return;
        }
        self.width = width;
        self.height = height;
        self.bgra = vec![0u8; usize::from(width) * usize::from(height) * 4];
    }

    fn stride(&self) -> usize {
        usize::from(self.width) * 4
    }

    /// 把一块原始像素解成 BGRA 并贴进缓冲。
    pub fn blit(&mut self, rect: Rect, payload: &[u8], pf: &PixelFormat) {
        let bpp = pf.bytes_per_pixel();
        let stride = self.stride();
        for row in 0..usize::from(rect.height) {
            let source = row * usize::from(rect.width) * bpp;
            let target = (usize::from(rect.y) + row) * stride + usize::from(rect.x) * 4;
            let dst = &mut self.bgra[target..target + usize::from(rect.width) * 4];
            for (column, chunk) in payload[source..source + usize::from(rect.width) * bpp]
                .chunks_exact(bpp)
                .enumerate()
            {
                dst[column * 4..column * 4 + 4].copy_from_slice(&pf.decode(chunk));
            }
        }
    }

    /// 用同一个颜色填一块。
    pub fn fill(&mut self, rect: Rect, color: [u8; 4]) {
        let stride = self.stride();
        for row in 0..usize::from(rect.height) {
            let target = (usize::from(rect.y) + row) * stride + usize::from(rect.x) * 4;
            let dst = &mut self.bgra[target..target + usize::from(rect.width) * 4];
            for chunk in dst.as_chunks_mut::<4>().0 {
                chunk.copy_from_slice(&color);
            }
        }
    }

    /// 复制矩形：源与目标可能重叠，逐行取到临时行再落。
    pub fn copy_rect(&mut self, source: Rect, target: Rect) -> Result<()> {
        checked_rect(self, source)?;
        checked_rect(self, target)?;
        let width = source.width.min(target.width);
        let height = source.height.min(target.height);
        let stride = self.stride();
        let mut row = vec![0u8; usize::from(width) * 4];
        for index in 0..usize::from(height) {
            let y = if target.y > source.y {
                usize::from(height) - 1 - index
            } else {
                index
            };
            let sy = usize::from(source.y) + y;
            let ty = usize::from(target.y) + y;
            let from = sy * stride + usize::from(source.x) * 4;
            let width_bytes = row.len();
            row.copy_from_slice(&self.bgra[from..from + width_bytes]);
            let to = ty * stride + usize::from(target.x) * 4;
            self.bgra[to..to + row.len()].copy_from_slice(&row);
        }
        Ok(())
    }

    /// 裁到帧缓冲内；完全在外就返回 None。
    pub fn clip(&self, rect: Rect) -> Option<Rect> {
        let x1 = rect.x.min(self.width);
        let y1 = rect.y.min(self.height);
        let x2 = (u32::from(rect.x) + u32::from(rect.width)).min(u32::from(self.width)) as u16;
        let y2 = (u32::from(rect.y) + u32::from(rect.height)).min(u32::from(self.height)) as u16;
        (x2 > x1 && y2 > y1).then_some(Rect {
            x: x1,
            y: y1,
            width: x2 - x1,
            height: y2 - y1,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn bgra_pf() -> PixelFormat {
        PixelFormat {
            bits_per_pixel: 32,
            depth: 24,
            big_endian: false,
            true_colour: true,
            red_max: 255,
            green_max: 255,
            blue_max: 255,
            red_shift: 16,
            green_shift: 8,
            blue_shift: 0,
        }
    }

    #[test]
    fn version_parsing_accepts_3x_and_rejects_others() {
        assert_eq!(parse_version(b"RFB 003.008\n").unwrap(), (3, 8));
        assert_eq!(parse_version(b"RFB 003.003\n").unwrap(), (3, 3));
        assert!(parse_version(b"RFB 004.000\n").is_err());
        assert!(parse_version(b"SSH-2.0-x\n\0\0").is_err());
    }

    #[test]
    fn pixel_decode_handles_32_and_16_bpp() {
        let pf = bgra_pf();
        // 内存顺序 B,G,R,A → 红 0x00112233 表示 r=0x00 g=0x11 b=0x22
        assert_eq!(
            pf.decode(&[0x33, 0x22, 0x11, 0x00]),
            [0x33, 0x22, 0x11, 255]
        );

        let rgb565 = PixelFormat {
            bits_per_pixel: 16,
            depth: 16,
            big_endian: false,
            true_colour: true,
            red_max: 31,
            green_max: 63,
            blue_max: 31,
            red_shift: 11,
            green_shift: 5,
            blue_shift: 0,
        };
        // 纯绿：g=63 → 255
        assert_eq!(rgb565.decode(&[0xe0, 0x07]), [0, 255, 0, 255]);

        let be = PixelFormat {
            big_endian: true,
            ..pf
        };
        assert_eq!(
            be.decode(&[0x00, 0x11, 0x22, 0xff]),
            [0xff, 0x22, 0x11, 255]
        );
    }

    #[test]
    fn framebuffer_fill_and_copy_rect() {
        let mut fb = Framebuffer::new(4, 4).unwrap();
        fb.fill(
            Rect {
                x: 0,
                y: 0,
                width: 2,
                height: 2,
            },
            [1, 2, 3, 255],
        );
        assert_eq!(&fb.bgra[0..4], &[1, 2, 3, 255]);
        fb.copy_rect(
            Rect {
                x: 0,
                y: 0,
                width: 2,
                height: 2,
            },
            Rect {
                x: 2,
                y: 2,
                width: 2,
                height: 2,
            },
        )
        .unwrap();
        let offset = (2 * 4 + 2) * 4;
        assert_eq!(&fb.bgra[offset..offset + 4], &[1, 2, 3, 255]);
    }

    #[tokio::test]
    async fn hextile_decodes_solid_and_subrects() {
        let init = ServerInit {
            width: 16,
            height: 16,
            name: "t".into(),
            pixel_format: bgra_pf(),
        };
        let mut state = ClientState::new(init).unwrap();
        // 一个块：bg 已指定 + fg 已指定 + 1 个非彩色子矩形（坐标 0,0 尺寸 2×2）
        let payload = vec![
            0b0000_1110, // bg + fg + any subrects
            0x10,
            0x20,
            0x30,
            0x00, // bg (BGR)
            0x01,
            0x02,
            0x03,
            0x00, // fg
            0x01, // 1 个子矩形
            0x00, // x=0 y=0
            0x11, // w=2 h=2
        ];
        let rect = Rect {
            x: 0,
            y: 0,
            width: 16,
            height: 16,
        };
        let mut reader: &[u8] = &payload;
        apply_hextile(&mut reader, &mut state, rect).await.unwrap();
        assert_eq!(&state.fb.bgra[0..4], &[0x01, 0x02, 0x03, 255]);
        assert_eq!(&state.fb.bgra[8..12], &[0x10, 0x20, 0x30, 255]);
    }

    #[tokio::test]
    async fn handshake_none_selects_supported_type_and_reads_init() {
        let (mut client, mut server) = tokio::io::duplex(4096);
        let task = tokio::spawn(async move {
            server.write_all(b"RFB 003.008\n").await.unwrap();
            let mut version = [0u8; 12];
            server.read_exact(&mut version).await.unwrap();
            assert_eq!(&version, b"RFB 003.008\n");
            server.write_all(&[2, 1, 2]).await.unwrap();
            let mut chosen = [0u8; 1];
            server.read_exact(&mut chosen).await.unwrap();
            // 没给密码时选无认证，即使服务器同时提供 VNC 认证。
            assert_eq!(chosen[0], 1);
            server.write_all(&0u32.to_be_bytes()).await.unwrap();
            let mut shared = [0u8; 1];
            server.read_exact(&mut shared).await.unwrap();
            assert_eq!(shared[0], 1);
            write_server_init(&mut server).await;
        });
        let init = handshake(&mut client, None, true).await.unwrap();
        task.await.unwrap();
        assert_eq!((init.width, init.height), (16, 8));
        assert_eq!(init.name, "test");
        assert_eq!(init.pixel_format.bytes_per_pixel(), 4);
    }

    #[tokio::test]
    async fn handshake_vnc_auth_prefers_challenge_when_password_given() {
        let (mut client, mut server) = tokio::io::duplex(4096);
        let task = tokio::spawn(async move {
            server.write_all(b"RFB 003.008\n").await.unwrap();
            let mut version = [0u8; 12];
            server.read_exact(&mut version).await.unwrap();
            server.write_all(&[2, 1, 2]).await.unwrap();
            let mut chosen = [0u8; 1];
            server.read_exact(&mut chosen).await.unwrap();
            assert_eq!(chosen[0], 2);
            server.write_all(&[7u8; 16]).await.unwrap();
            let mut response = [0u8; 16];
            server.read_exact(&mut response).await.unwrap();
            assert_eq!(response, vnc_auth_response("secret", &[7u8; 16]));
            server.write_all(&0u32.to_be_bytes()).await.unwrap();
            let mut shared = [0u8; 1];
            server.read_exact(&mut shared).await.unwrap();
            assert_eq!(shared[0], 0);
            write_server_init(&mut server).await;
        });
        let init = handshake(&mut client, Some("secret"), false).await.unwrap();
        task.await.unwrap();
        assert_eq!((init.width, init.height), (16, 8));
    }

    async fn write_server_init(server: &mut tokio::io::DuplexStream) {
        server.write_all(&16u16.to_be_bytes()).await.unwrap();
        server.write_all(&8u16.to_be_bytes()).await.unwrap();
        let mut pf = [0u8; 16];
        pf[0] = 32; // bpp
        pf[1] = 24; // depth
        pf[3] = 1; // true colour
        pf[4..6].copy_from_slice(&255u16.to_be_bytes());
        pf[6..8].copy_from_slice(&255u16.to_be_bytes());
        pf[8..10].copy_from_slice(&255u16.to_be_bytes());
        pf[10] = 16;
        pf[11] = 8;
        pf[12] = 0;
        server.write_all(&pf).await.unwrap();
        server.write_all(&4u32.to_be_bytes()).await.unwrap();
        server.write_all(b"test").await.unwrap();
    }

    #[test]
    fn auth_response_uses_bit_reversed_password_and_des() {
        let challenge = [0u8; 16];
        let empty = vnc_auth_response("", &challenge);
        let short = vnc_auth_response("12345678", &challenge);
        assert_ne!(empty, short);
        // 超过 8 字节的部分被忽略。
        assert_eq!(vnc_auth_response("12345678ignored", &challenge), short);
        // 全零密钥 + 全零明文就是 DES 的标准零向量结果。
        assert_eq!(
            empty,
            [
                0x8C, 0xA6, 0x4D, 0xE9, 0xC1, 0xB1, 0x23, 0xA7, 0x8C, 0xA6, 0x4D, 0xE9, 0xC1, 0xB1,
                0x23, 0xA7
            ]
        );
    }

    #[test]
    fn client_messages_have_expected_headers() {
        assert_eq!(
            key_message(0xff0d, true),
            vec![4, 1, 0, 0, 0, 0, 0xff, 0x0d]
        );
        assert_eq!(pointer_message(3, 4, 1), vec![5, 1, 0, 3, 0, 4]);
        let encodings = set_encodings_message(&ENCODINGS);
        assert_eq!(&encodings[..4], &[2, 0, 0, 4]);
        let request = update_request_message(true, 640, 480);
        assert_eq!(&request[..2], &[3, 1]);
        assert_eq!(&request[6..], &[0x02, 0x80, 0x01, 0xE0]);
        assert_eq!(
            clipboard_message("ab"),
            vec![6, 0, 0, 0, 0, 0, 0, 2, b'a', b'b']
        );
    }
}

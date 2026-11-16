//! 本机 shell 档案：探测、一键扫描与系统图标提取。
//!
//! 分两档是刻意的：启动只查文件存在（`detect`，微秒级），起子进程的 WSL 发行版
//! 枚举留到用户点「一键扫描」时再做（`scan`）——`wsl.exe -l` 即便在没装发行版的
//! 机器上也要几十到几百毫秒，不该挡在冷启动路径上。

use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

/// shell 类型：取不到系统图标时，据此挑一个内置字形兜底。
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub enum ShellKind {
    PowerShell,
    Cmd,
    Bash,
    Wsl,
    #[default]
    Other,
}

/// 一个可拉起的本机 shell。
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ShellProfile {
    /// 展示名，同时用作终端标签名（PowerShell 7 / Git Bash / WSL · Ubuntu）。
    pub name: String,
    /// 可执行文件路径。
    pub program: String,
    /// 启动参数（WSL 发行版靠 `-d <distro>` 区分，故单列一项而非拼进路径）。
    #[serde(default)]
    pub args: Vec<String>,
    #[serde(default)]
    pub kind: ShellKind,
}

impl ShellProfile {
    /// 稳定标识：路径 + 参数。设置里存的默认 shell 就是它。
    pub fn id(&self) -> String {
        if self.args.is_empty() {
            self.program.clone()
        } else {
            format!("{} {}", self.program, self.args.join(" "))
        }
    }

    /// 搜索框是否命中本条（按展示名或可执行文件名，大小写不敏感）。
    pub fn matches(&self, query: &str) -> bool {
        if query.is_empty() {
            return true;
        }
        let query = query.to_lowercase();
        self.name.to_lowercase().contains(&query)
            || Path::new(&self.program)
                .file_name()
                .map(|stem| stem.to_string_lossy().to_lowercase().contains(&query))
                .unwrap_or(false)
    }
}

/// 网络诊断工具：在本地 PTY 里跑系统命令，输出即终端内容。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum NetTool {
    Ping,
    TraceRoute,
}

impl NetTool {
    /// 标签名与菜单里用的工具名。
    pub fn label(self) -> &'static str {
        match self {
            Self::Ping => "Ping",
            Self::TraceRoute => "路由追踪",
        }
    }
}

/// 主机名只留安全字符：命令要拼进 `cmd /k`，空白 / 引号 / 管道都会变成注入面。
///
/// IPv6 字面量两侧的方括号顺手去掉——`ping ::1` 与 `ping [::1]` 都能跑，
/// 但前者不必依赖各版本工具对括号的处理。
pub fn sanitize_host(raw: &str) -> Option<String> {
    let host = raw
        .trim()
        .trim_start_matches('[')
        .trim_end_matches(']')
        .trim();
    if host.is_empty() || host.len() > 255 {
        return None;
    }
    host.chars()
        .all(|ch| ch.is_ascii_alphanumeric() || matches!(ch, '.' | '-' | ':' | '_'))
        .then(|| host.to_string())
}

/// 诊断命令的启动档案。
///
/// Windows 用 `cmd /k`：命令跑完窗口留在提示符上，追踪结果不会一闪而过就被
/// 「本地 shell 退出即收标签」的规则关掉；`ping -t` 持续跑，Ctrl+C 停下后同样回到提示符。
/// 非 Windows 的 PTY 后端只收单个可执行文件路径、传不了参数，故暂不提供。
#[cfg(windows)]
pub fn diagnostic_profile(tool: NetTool, host: &str) -> Option<ShellProfile> {
    let command = match tool {
        NetTool::Ping => format!("ping -t {host}"),
        NetTool::TraceRoute => format!("tracert {host}"),
    };
    Some(ShellProfile {
        name: format!("{} {host}", tool.label()),
        program: std::env::var("COMSPEC").unwrap_or_else(|_| "cmd.exe".to_string()),
        args: vec!["/k".to_string(), command],
        kind: ShellKind::Other,
    })
}

/// 非 Windows：见 [`diagnostic_profile`] 的说明。
#[cfg(not(windows))]
pub fn diagnostic_profile(_tool: NetTool, _host: &str) -> Option<ShellProfile> {
    None
}

/// 交给 PTY 后端的 shell 字符串。
///
/// Windows 后端把 `shell` 原样拼进 `CreateProcessW` 的命令行（`escape_args=false`），
/// 所以带启动参数（如 `wsl.exe -d Debian`）时必须自己拼一条完整命令行；
/// 没有参数时仍是纯路径，与旧行为一致。
///
/// 档案自带参数在前，用户自定义参数（`extra`）在后。
/// 非 Windows 后端不解析命令行，参数无法传递，只返回可执行文件路径。
pub fn command_line_with(profile: &ShellProfile, extra: &[String]) -> String {
    if profile.args.is_empty() && extra.is_empty() {
        return profile.program.clone();
    }
    #[cfg(windows)]
    {
        let mut line = quote_arg(&profile.program);
        for arg in profile.args.iter().chain(extra) {
            line.push(' ');
            line.push_str(&quote_arg(arg));
        }
        line
    }
    #[cfg(not(windows))]
    {
        let _ = extra;
        profile.program.clone()
    }
}

/// 启动参数文本 → 参数数组：按空白切分，双引号内保留空白，引号本身丢弃。
///
/// 未闭合的引号按"一直到结尾"处理，不报错——用户写到一半不该弹错误。
pub fn split_args(text: &str) -> Vec<String> {
    let mut args = Vec::new();
    let mut current = String::new();
    let mut quoted = false;
    // 空引号 `""` 是一个合法空参数，故额外记一个"本段有内容"标记。
    let mut started = false;
    for ch in text.chars() {
        match ch {
            '"' => {
                quoted = !quoted;
                started = true;
            }
            ch if ch.is_whitespace() && !quoted => {
                if started {
                    args.push(std::mem::take(&mut current));
                    started = false;
                }
            }
            ch => {
                current.push(ch);
                started = true;
            }
        }
    }
    if started {
        args.push(current);
    }
    args
}

/// 参数数组 → 文本（含空白或为空时加双引号），用于回填设置框与规范化存储。
pub fn join_args(args: &[String]) -> String {
    args.iter()
        .map(|arg| quote_arg(arg))
        .collect::<Vec<_>>()
        .join(" ")
}

/// 环境变量文本 → 表：每行 `KEY=VALUE`，空行与 `#` 开头的注释忽略。
///
/// 键名不合规（含空白或以 `=` 开头）时返回带行号的错误，让弹窗拒绝保存。
pub fn parse_env(text: &str) -> Result<std::collections::HashMap<String, String>, String> {
    let mut env = std::collections::HashMap::new();
    for (index, raw) in text.lines().enumerate() {
        let line = raw.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let Some((key, value)) = line.split_once('=') else {
            return Err(format!("第 {} 行缺少 =：{line}", index + 1));
        };
        let key = key.trim();
        if key.is_empty() || key.contains(char::is_whitespace) {
            return Err(format!("第 {} 行的变量名不合法：{line}", index + 1));
        }
        env.insert(key.to_string(), value.trim().to_string());
    }
    Ok(env)
}

/// 环境变量表 → 文本（按键排序，便于回填与比对）。
pub fn env_text(env: &std::collections::HashMap<String, String>) -> String {
    let mut entries: Vec<_> = env.iter().collect();
    entries.sort_by(|left, right| left.0.cmp(right.0));
    entries
        .into_iter()
        .map(|(key, value)| format!("{key}={value}"))
        .collect::<Vec<_>>()
        .join("\n")
}

/// 含空白或为空时加双引号（Windows 命令行分词规则）。
fn quote_arg(arg: &str) -> String {
    if arg.is_empty() || arg.contains([' ', '\t']) {
        format!("\"{arg}\"")
    } else {
        arg.to_string()
    }
}

/// 在 `PATH` 里查找可执行文件（Windows 下自动补 `.exe`）。
fn which(program: &str) -> Option<PathBuf> {
    let name = if cfg!(windows) {
        format!("{program}.exe")
    } else {
        program.to_owned()
    };
    let path = std::env::var_os("PATH")?;
    std::env::split_paths(&path)
        .map(|dir| dir.join(&name))
        .find(|candidate| candidate.is_file())
}

fn env_path(name: &str) -> Option<PathBuf> {
    std::env::var_os(name).map(PathBuf::from)
}

/// 由路径造档案；文件不存在就放弃。
fn profile(name: &str, path: PathBuf, kind: ShellKind) -> Option<ShellProfile> {
    path.is_file().then(|| ShellProfile {
        name: name.to_string(),
        program: path.display().to_string(),
        args: Vec::new(),
        kind,
    })
}

/// 只查文件系统，不起任何子进程。冷启动用它。
pub fn detect() -> Vec<ShellProfile> {
    let mut found = Vec::new();
    push(&mut found, powershell7());
    push(&mut found, windows_powershell());
    push(&mut found, git_bash());
    push(&mut found, command_prompt());
    push(&mut found, posix_shell());
    dedup(found)
}

/// 完整扫描：`detect()` 之外再枚举 WSL 发行版（起 `wsl.exe -l -q`）。
pub fn scan() -> Vec<ShellProfile> {
    let mut found = detect();
    found.extend(wsl_distros());
    dedup(found)
}

fn push(list: &mut Vec<ShellProfile>, item: Option<ShellProfile>) {
    if let Some(item) = item {
        list.push(item);
    }
}

/// 按 id 去重，保持先发现的顺序。
fn dedup(list: Vec<ShellProfile>) -> Vec<ShellProfile> {
    let mut seen: Vec<String> = Vec::new();
    list.into_iter()
        .filter(|profile| {
            let fresh = !seen.contains(&profile.id());
            if fresh {
                seen.push(profile.id());
            }
            fresh
        })
        .collect()
}

#[cfg(windows)]
fn powershell7() -> Option<ShellProfile> {
    profile("PowerShell 7", which("pwsh")?, ShellKind::PowerShell)
}

#[cfg(not(windows))]
fn powershell7() -> Option<ShellProfile> {
    None
}

/// Windows PowerShell：优先 PATH，再直接探 System32 里的确切路径。
#[cfg(windows)]
fn windows_powershell() -> Option<ShellProfile> {
    let path = which("powershell").or_else(|| {
        env_path("SystemRoot").map(|root| {
            root.join("System32")
                .join("WindowsPowerShell")
                .join("v1.0")
                .join("powershell.exe")
        })
    })?;
    profile("Windows PowerShell", path, ShellKind::PowerShell)
}

#[cfg(not(windows))]
fn windows_powershell() -> Option<ShellProfile> {
    None
}

#[cfg(windows)]
fn command_prompt() -> Option<ShellProfile> {
    let path = env_path("COMSPEC").or_else(|| which("cmd"))?;
    profile("命令提示符", path, ShellKind::Cmd)
}

#[cfg(not(windows))]
fn command_prompt() -> Option<ShellProfile> {
    None
}

/// Git Bash：`git.exe` 与 `bin\bash.exe` 同根，故由 git 的位置反推；
/// 探不到再按几个常见安装目录兜底（含用户级安装），最后退回 PATH 上的 bash。
#[cfg(windows)]
fn git_bash() -> Option<ShellProfile> {
    let from_git = which("git").and_then(|git| {
        git.parent()?
            .parent()
            .map(|root| root.join("bin").join("bash.exe"))
    });
    let candidates = from_git.into_iter().chain(
        ["ProgramFiles", "ProgramFiles(x86)", "LOCALAPPDATA"]
            .into_iter()
            .filter_map(env_path)
            .map(|dir| dir.join("Git").join("bin").join("bash.exe")),
    );
    let path = candidates
        .chain(which("bash"))
        .find(|path| path.is_file())?;
    profile("Git Bash", path, ShellKind::Bash)
}

#[cfg(not(windows))]
fn git_bash() -> Option<ShellProfile> {
    None
}

/// *nix 下按 `$SHELL` → 常见 shell 的顺序取第一个存在的。
#[cfg(unix)]
fn posix_shell() -> Option<ShellProfile> {
    let path = std::env::var("SHELL")
        .ok()
        .into_iter()
        .chain(["/bin/bash", "/bin/zsh", "/bin/fish", "/bin/sh"].map(str::to_string))
        .map(PathBuf::from)
        .find(|path| path.is_file())?;
    let name = path.file_name()?.to_string_lossy().into_owned();
    profile(&name, path, ShellKind::Bash)
}

#[cfg(not(unix))]
fn posix_shell() -> Option<ShellProfile> {
    None
}

/// WSL。`wsl.exe` 在部分 Windows 版本里不在 PATH 上，故也探 System32。
///
/// 每个发行版单独成条（`WSL · Debian`），启动参数 `-d <发行版>`；参数经
/// [`command_line_with`] 拼进整条命令行，绕开 `TerminalBuilder` 只收单一路径的限制。
/// 枚举失败（`wsl.exe` 存在但调用出错）时退回一条「默认发行版」，无发行版则不出现条目。
#[cfg(windows)]
fn wsl_distros() -> Vec<ShellProfile> {
    let Some(wsl) = wsl_launcher() else {
        return Vec::new();
    };
    wsl_profiles(&wsl.display().to_string(), wsl_distro_names())
}

/// 由 `wsl -l -q` 的结果造档案；`None` 表示枚举本身失败（不是"没装发行版"）。
#[cfg(windows)]
fn wsl_profiles(program: &str, names: Option<Vec<String>>) -> Vec<ShellProfile> {
    match names {
        Some(names) if names.is_empty() => Vec::new(),
        Some(names) => names
            .into_iter()
            .map(|name| ShellProfile {
                name: format!("WSL · {name}"),
                program: program.to_string(),
                args: vec!["-d".to_string(), name],
                kind: ShellKind::Wsl,
            })
            .collect(),
        None => vec![ShellProfile {
            name: "WSL（默认发行版）".to_string(),
            program: program.to_string(),
            args: Vec::new(),
            kind: ShellKind::Wsl,
        }],
    }
}

/// WSL 的可执行入口（不存在则 None）。
#[cfg(windows)]
fn wsl_launcher() -> Option<PathBuf> {
    which("wsl")
        .or_else(|| env_path("SystemRoot").map(|root| root.join("System32").join("wsl.exe")))
        .filter(|path| path.is_file())
}

/// 已安装的 WSL 发行版名（`wsl -l -q`）。`None`＝调用失败，`Some(空)`＝确实没装。
#[cfg(windows)]
fn wsl_distro_names() -> Option<Vec<String>> {
    let wsl = wsl_launcher()?;
    let output = std::process::Command::new(&wsl)
        .args(["-l", "-q"])
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null())
        .creation_flags(NO_WINDOW)
        .output()
        .ok()?;
    output
        .status
        .success()
        .then(|| decode_wsl_list(&output.stdout))
}

#[cfg(not(windows))]
fn wsl_distros() -> Vec<ShellProfile> {
    Vec::new()
}

/// `wsl -l -q` 老版本输出 UTF-16LE、新版本输出 UTF-8，两种都得吃；
/// 没装发行版时它输出的是一句提示语，按关键字滤掉。
#[cfg(windows)]
fn decode_wsl_list(bytes: &[u8]) -> Vec<String> {
    let text = if looks_utf16(bytes) {
        let units: Vec<u16> = bytes
            .chunks(2)
            .filter_map(|pair| Some(u16::from_le_bytes([pair[0], *pair.get(1)?])))
            .collect();
        String::from_utf16_lossy(&units)
    } else {
        String::from_utf8_lossy(bytes).into_owned()
    };
    text.lines()
        .map(str::trim)
        .filter(|line| !line.is_empty() && !is_wsl_hint(line))
        .map(str::to_string)
        .collect()
}

/// ASCII 文本的 UTF-16LE 形态：奇数位几乎全是 0。
#[cfg(windows)]
fn looks_utf16(bytes: &[u8]) -> bool {
    let odd: Vec<u8> = bytes.iter().copied().skip(1).step_by(2).collect();
    !odd.is_empty() && odd.iter().filter(|&&byte| byte == 0).count() * 2 >= odd.len()
}

/// 没装任何发行版时 `wsl -l -q` 输出的那句提示语（中英文系统各一版）。
#[cfg(windows)]
fn is_wsl_hint(line: &str) -> bool {
    let lower = line.to_ascii_lowercase();
    line.contains("未安装")
        || line.contains("尚未安装")
        || lower.contains("no installed distributions")
        || lower.contains("cannot install")
}

/// CREATE_NO_WINDOW：扫描时不许闪控制台窗口。
#[cfg(windows)]
const NO_WINDOW: u32 = 0x0800_0000;

#[cfg(windows)]
trait CreationFlags {
    fn creation_flags(&mut self, flags: u32) -> &mut Self;
}

#[cfg(windows)]
impl CreationFlags for std::process::Command {
    fn creation_flags(&mut self, flags: u32) -> &mut Self {
        std::os::windows::process::CommandExt::creation_flags(self, flags)
    }
}

/// 取可执行文件的系统默认图标，编码成 PNG 字节。
///
/// 走 `SHGetFileInfoW` 拿 HICON，再 `DrawIconEx` 画进 32 位 DIB Section 读回像素——
/// 这样 pwsh / Git Bash / 各 WSL 发行版显示的都是自己的图标，而不是一个通用字形。
/// 任何一步失败都返回 None，由调用方回退内置字形。
#[cfg(windows)]
pub fn icon_png(path: &str) -> Option<Vec<u8>> {
    use windows::Win32::UI::Shell::{SHFILEINFOW, SHGFI_ICON, SHGFI_LARGEICON, SHGetFileInfoW};
    use windows::core::PCWSTR;

    let wide: Vec<u16> = path.encode_utf16().chain(std::iter::once(0)).collect();
    let mut info = SHFILEINFOW::default();
    unsafe {
        SHGetFileInfoW(
            PCWSTR(wide.as_ptr()),
            Default::default(),
            Some(&mut info),
            std::mem::size_of::<SHFILEINFOW>() as u32,
            SHGFI_ICON | SHGFI_LARGEICON,
        )
    };
    let hicon = info.hIcon;
    if hicon.is_invalid() {
        return None;
    }
    let png = unsafe { icon_to_png(hicon) };
    unsafe {
        let _ = windows::Win32::UI::WindowsAndMessaging::DestroyIcon(hicon);
    }
    png
}

/// 把 HICON 画进 32 位 DIB Section 再编码；调用方负责 `DestroyIcon`。
#[cfg(windows)]
unsafe fn icon_to_png(hicon: windows::Win32::UI::WindowsAndMessaging::HICON) -> Option<Vec<u8>> {
    use std::ffi::c_void;
    use windows::Win32::Graphics::Gdi::{
        BI_RGB, BITMAP, BITMAPINFO, BITMAPINFOHEADER, CreateCompatibleDC, CreateDIBSection,
        DIB_RGB_COLORS, DeleteDC, DeleteObject, GetObjectW, HGDIOBJ, SelectObject,
    };
    use windows::Win32::UI::WindowsAndMessaging::{DI_NORMAL, DrawIconEx, GetIconInfo, ICONINFO};

    unsafe {
        let mut icon_info = ICONINFO::default();
        GetIconInfo(hicon, &mut icon_info).ok()?;
        let metrics_bitmap = if icon_info.hbmColor.is_invalid() {
            icon_info.hbmMask
        } else {
            icon_info.hbmColor
        };
        let mut metrics = BITMAP::default();
        if GetObjectW(
            HGDIOBJ(metrics_bitmap.0),
            std::mem::size_of::<BITMAP>() as i32,
            Some(&mut metrics as *mut BITMAP as *mut c_void),
        ) == 0
        {
            return None;
        }
        let (width, height) = (metrics.bmWidth.max(1), metrics.bmHeight.abs().max(1));
        let memory = CreateCompatibleDC(None);
        if memory.is_invalid() {
            return None;
        }
        let header = BITMAPINFOHEADER {
            biSize: std::mem::size_of::<BITMAPINFOHEADER>() as u32,
            biWidth: width,
            // 负高度 = 自上而下的行序，读像素时不必再翻一次。
            biHeight: -height,
            biPlanes: 1,
            biBitCount: 32,
            biCompression: BI_RGB.0,
            ..Default::default()
        };
        let mut bits: *mut c_void = std::ptr::null_mut();
        let png = match CreateDIBSection(
            Some(memory),
            &BITMAPINFO {
                bmiHeader: header,
                ..Default::default()
            },
            DIB_RGB_COLORS,
            &mut bits,
            None,
            0,
        ) {
            Ok(dib) if !bits.is_null() => {
                let previous = SelectObject(memory, HGDIOBJ(dib.0));
                let drawn = DrawIconEx(memory, 0, 0, hicon, width, height, 0, None, DI_NORMAL);
                let pixels = drawn.ok().map(|_| {
                    let source = std::slice::from_raw_parts(
                        bits as *const u8,
                        (width * height) as usize * 4,
                    );
                    encode_png(source, width, height)
                });
                SelectObject(memory, previous);
                let _ = DeleteObject(HGDIOBJ(dib.0));
                pixels
            }
            _ => None,
        };
        let _ = DeleteDC(memory);
        let _ = DeleteObject(HGDIOBJ(icon_info.hbmColor.0));
        let _ = DeleteObject(HGDIOBJ(icon_info.hbmMask.0));
        png.flatten()
    }
}

/// DIB 里的 BGRA → PNG 的 RGBA。全透明像素顺手清掉残留色值，避免描边发暗。
#[cfg(windows)]
fn encode_png(bgra: &[u8], width: i32, height: i32) -> Option<Vec<u8>> {
    let mut rgba = Vec::with_capacity(bgra.len());
    for chunk in bgra.as_chunks::<4>().0 {
        let (blue, green, red, alpha) = (chunk[0], chunk[1], chunk[2], chunk[3]);
        if alpha == 0 {
            rgba.extend_from_slice(&[0, 0, 0, 0]);
        } else {
            rgba.extend_from_slice(&[red, green, blue, alpha]);
        }
    }
    let mut out = Vec::new();
    {
        let mut encoder = png::Encoder::new(&mut out, width as u32, height as u32);
        encoder.set_color(png::ColorType::Rgba);
        encoder.set_depth(png::BitDepth::Eight);
        let mut writer = encoder.write_header().ok()?;
        writer.write_image_data(&rgba).ok()?;
    }
    Some(out)
}

#[cfg(not(windows))]
pub fn icon_png(_path: &str) -> Option<Vec<u8>> {
    None
}

/// 路径是否指向一个存在的文件（扫描结果的正确性判据）。
#[cfg(test)]
fn is_executable(path: &str) -> bool {
    Path::new(path).is_file()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn detect_only_lists_existing_executables() {
        let found = detect();
        assert!(
            found.iter().all(|profile| is_executable(&profile.program)),
            "扫描结果里不该有不存在的路径: {found:?}"
        );
    }

    #[test]
    fn scan_is_a_superset_of_detect() {
        let base = detect();
        let full = scan();
        assert!(full.len() >= base.len());
        for profile in &base {
            assert!(
                full.iter().any(|candidate| candidate.id() == profile.id()),
                "扫描丢掉了 {:?}",
                profile.id()
            );
        }
    }

    #[test]
    fn dedup_keeps_first_occurrence_and_order() {
        let manifest = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("Cargo.toml");
        let twice = vec![
            profile("先发现的", manifest.clone(), ShellKind::Other).unwrap(),
            profile("重复的", manifest, ShellKind::Other).unwrap(),
        ];
        let once = dedup(twice);
        assert_eq!(once.len(), 1);
        assert_eq!(once[0].name, "先发现的");
    }

    #[test]
    fn id_includes_args_so_wsl_distros_differ() {
        let wsl = ShellProfile {
            name: "WSL · Ubuntu".into(),
            program: "C:\\Windows\\System32\\wsl.exe".into(),
            args: vec!["-d".into(), "Ubuntu".into()],
            kind: ShellKind::Wsl,
        };
        assert!(wsl.id().ends_with("-d Ubuntu"));
    }

    #[cfg(windows)]
    #[test]
    fn wsl_list_decodes_both_utf16_and_utf8() {
        let utf16: Vec<u8> = "Ubuntu\r\nDebian\r\n"
            .encode_utf16()
            .flat_map(|unit| unit.to_le_bytes())
            .collect();
        assert_eq!(decode_wsl_list(&utf16), vec!["Ubuntu", "Debian"]);
        assert_eq!(
            decode_wsl_list(b"Ubuntu\r\nDebian\r\n"),
            vec!["Ubuntu", "Debian"]
        );
        assert!(decode_wsl_list("尚未安装任何 Linux 发行版。\r\n".as_bytes()).is_empty());
        assert!(decode_wsl_list(b"").is_empty());
    }

    #[cfg(windows)]
    #[test]
    fn wsl_profiles_split_per_distro_and_keep_args() {
        let profiles = wsl_profiles(
            r"C:\Windows\System32\wsl.exe",
            Some(vec!["Debian".into(), "docker-desktop".into()]),
        );
        assert_eq!(profiles.len(), 2);
        assert_eq!(profiles[0].name, "WSL · Debian");
        assert_eq!(profiles[0].args, vec!["-d".to_string(), "Debian".into()]);
        assert_eq!(profiles[1].name, "WSL · docker-desktop");
        // 两个发行版靠参数区分 id，去重不会把它们并成一条。
        assert_ne!(profiles[0].id(), profiles[1].id());
        // 确实没装发行版：不出现条目；枚举失败：退回默认发行版一条。
        assert!(wsl_profiles(r"C:\Windows\System32\wsl.exe", Some(Vec::new())).is_empty());
        let fallback = wsl_profiles(r"C:\Windows\System32\wsl.exe", None);
        assert_eq!(fallback.len(), 1);
        assert!(fallback[0].args.is_empty());
    }

    #[cfg(windows)]
    #[test]
    fn split_args_handles_quotes_and_empty_segments() {
        assert!(split_args("   ").is_empty());
        assert_eq!(
            split_args("  -NoLogo   -NoProfile "),
            ["-NoLogo", "-NoProfile"]
        );
        assert_eq!(split_args(r#"-Command "echo hi""#), ["-Command", "echo hi"]);
        assert_eq!(
            split_args(r#""C:\Program Files\Git\bin\bash.exe" -i -l"#),
            [r"C:\Program Files\Git\bin\bash.exe", "-i", "-l"]
        );
        // 空引号是一个合法空参数；未闭合的引号按到结尾处理。
        assert_eq!(split_args(r#""" tail"#), ["", "tail"]);
        assert_eq!(split_args(r#"head "open"#), ["head", "open"]);
    }

    #[test]
    fn join_args_round_trips_through_split() {
        let args = vec!["-Command".to_string(), "echo hi".to_string(), String::new()];
        assert_eq!(split_args(&join_args(&args)), args);
    }

    #[test]
    fn parse_env_reads_pairs_and_skips_comments() {
        let env = parse_env("FOO=1\n\n# 注释\nBAR = two words \nBAZ=\n").unwrap();
        assert_eq!(env.get("FOO").map(String::as_str), Some("1"));
        assert_eq!(env.get("BAR").map(String::as_str), Some("two words"));
        assert_eq!(env.get("BAZ").map(String::as_str), Some(""));
        assert!(parse_env("").unwrap().is_empty());
        // 值里允许再出现 =。
        assert_eq!(
            parse_env("URL=a=b").unwrap().get("URL").map(String::as_str),
            Some("a=b")
        );
    }

    #[test]
    fn parse_env_rejects_bad_lines() {
        assert!(parse_env("NOVALUE").unwrap_err().contains("第 1 行"));
        assert!(
            parse_env("OK=1\nBAD KEY=2")
                .unwrap_err()
                .contains("第 2 行")
        );
        assert!(parse_env("=1").is_err());
    }

    #[test]
    fn env_text_sorts_keys_and_round_trips() {
        let mut env = std::collections::HashMap::new();
        env.insert("B".to_string(), "2".to_string());
        env.insert("A".to_string(), "1".to_string());
        assert_eq!(env_text(&env), "A=1\nB=2");
        assert_eq!(parse_env(&env_text(&env)).unwrap(), env);
    }

    #[cfg(windows)]
    #[test]
    fn command_line_appends_and_quotes_args() {
        let mut profile = ShellProfile {
            name: "WSL · Debian".into(),
            program: r"C:\Windows\System32\wsl.exe".into(),
            args: vec![],
            kind: ShellKind::Wsl,
        };
        assert_eq!(
            command_line_with(&profile, &[]),
            r"C:\Windows\System32\wsl.exe"
        );
        profile.args = vec!["-d".into(), "Debian".into()];
        assert_eq!(
            command_line_with(&profile, &[]),
            r"C:\Windows\System32\wsl.exe -d Debian"
        );
        profile.program = r"C:\Program Files\Git\bin\bash.exe".into();
        profile.args = vec!["-i".into(), "-l".into()];
        assert_eq!(
            command_line_with(&profile, &[]),
            r#""C:\Program Files\Git\bin\bash.exe" -i -l"#
        );
        assert_eq!(
            command_line_with(&profile, &["-NoLogo".into(), "a b".into()]),
            r#""C:\Program Files\Git\bin\bash.exe" -i -l -NoLogo "a b""#
        );
    }

    #[cfg(windows)]
    #[test]
    fn icon_png_returns_png_bytes_for_a_real_executable() {
        let exe = env_path("SystemRoot")
            .expect("SystemRoot")
            .join("System32")
            .join("cmd.exe");
        let png = icon_png(&exe.display().to_string()).expect("cmd.exe 应有系统图标");
        assert_eq!(&png[1..4], b"PNG", "返回的应是 PNG 字节");
    }

    #[test]
    fn sanitize_host_keeps_hostnames_and_strips_ipv6_brackets() {
        assert_eq!(
            sanitize_host(" example.com ").as_deref(),
            Some("example.com")
        );
        assert_eq!(sanitize_host("127.0.0.1").as_deref(), Some("127.0.0.1"));
        assert_eq!(sanitize_host("[::1]").as_deref(), Some("::1"));
        assert_eq!(sanitize_host("fe80::1%eth0"), None, "% 不在白名单里");
        // 注入面：空白能把后面的参数拆成新命令，管道 / 引号同理。
        assert_eq!(sanitize_host("example.com & calc"), None);
        assert_eq!(sanitize_host("example.com|calc"), None);
        assert_eq!(sanitize_host("example.com\" x"), None);
        assert_eq!(sanitize_host(""), None);
        assert_eq!(sanitize_host("[]"), None);
        assert_eq!(sanitize_host(&"a".repeat(256)), None);
    }

    #[cfg(windows)]
    #[test]
    fn diagnostic_profile_runs_ping_and_tracert_under_cmd() {
        let ping = diagnostic_profile(NetTool::Ping, "example.com").unwrap();
        assert_eq!(ping.name, "Ping example.com");
        assert!(ping.program.to_lowercase().ends_with("cmd.exe"));
        // `cmd /k` 让命令跑完留在提示符上，标签不会被"shell 退出"规则收掉。
        assert_eq!(
            ping.args,
            vec!["/k".to_string(), "ping -t example.com".into()]
        );
        let trace = diagnostic_profile(NetTool::TraceRoute, "10.0.0.1").unwrap();
        assert_eq!(trace.name, "路由追踪 10.0.0.1");
        assert_eq!(
            trace.args,
            vec!["/k".to_string(), "tracert 10.0.0.1".into()]
        );
    }
}

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

/// 搜索框是否命中任一 shell。
pub fn matches(list: &[ShellProfile], query: &str) -> bool {
    list.iter().any(|profile| profile.matches(query))
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
/// 只给「默认发行版」一项：rgpui-term 的 `TerminalBuilder::new` 把 `Shell` 的参数写死成
/// 空 `Vec`，`wsl.exe -d <distro>` 递不进去，故按发行版开标签要等上游暴露启动参数
/// （路线图 §五.1 已记此限制）。发行版数量仍报给状态栏，扫描结果对用户不算黑盒。
#[cfg(windows)]
fn wsl_distros() -> Vec<ShellProfile> {
    let Some(wsl) = wsl_launcher() else {
        return Vec::new();
    };
    let count = wsl_distro_names().len();
    vec![ShellProfile {
        name: if count <= 1 {
            "WSL".to_string()
        } else {
            format!("WSL（{count} 个发行版）")
        },
        program: wsl.display().to_string(),
        args: Vec::new(),
        kind: ShellKind::Wsl,
    }]
}

/// WSL 的可执行入口（不存在则 None）。
#[cfg(windows)]
fn wsl_launcher() -> Option<PathBuf> {
    which("wsl")
        .or_else(|| env_path("SystemRoot").map(|root| root.join("System32").join("wsl.exe")))
        .filter(|path| path.is_file())
}

/// 已安装的 WSL 发行版名（`wsl -l -q`）；没装或调用失败时为空。
#[cfg(windows)]
fn wsl_distro_names() -> Vec<String> {
    let Some(wsl) = wsl_launcher() else {
        return Vec::new();
    };
    let Ok(output) = std::process::Command::new(&wsl)
        .args(["-l", "-q"])
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null())
        .creation_flags(NO_WINDOW)
        .output()
    else {
        return Vec::new();
    };
    decode_wsl_list(&output.stdout)
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
    for chunk in bgra.chunks_exact(4) {
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
    fn icon_png_returns_png_bytes_for_a_real_executable() {
        let exe = env_path("SystemRoot")
            .expect("SystemRoot")
            .join("System32")
            .join("cmd.exe");
        let png = icon_png(&exe.display().to_string()).expect("cmd.exe 应有系统图标");
        assert_eq!(&png[1..4], b"PNG", "返回的应是 PNG 字节");
    }
}

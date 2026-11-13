//! 会话平台：内置命令按平台筛选，避免在 Linux 上提示注册表命令。
//!
//! 识别来源都是现成的：SSH 连接后 exec 探测的 `uname -s`（见 `remote::PROBE_CMD`），
//! 以及本地 shell 的类型。`Cross` 不是会话平台，只用来标记与平台无关的命令（如 Docker）。

use crate::local_shell::ShellKind;

/// 命令行环境。`Unix` 覆盖 macOS / BSD / Git Bash 这类有 POSIX 工具但不是 Linux 的环境。
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Platform {
    Windows,
    Linux,
    Unix,
    /// 与平台无关（Docker 等）；不会作为会话平台出现。
    Cross,
}

impl Platform {
    /// 展示顺序：Windows 本地最常见，其后 Linux、其他 Unix，通用在最后。
    pub const ALL: [Platform; 4] = [Self::Windows, Self::Linux, Self::Unix, Self::Cross];

    pub fn label(self) -> &'static str {
        match self {
            Self::Windows => "Windows",
            Self::Linux => "Linux",
            Self::Unix => "Unix",
            Self::Cross => "通用",
        }
    }

    /// 由 `uname -s` 的输出（内核名 + 版本）判断平台。
    pub fn from_uname(sys: &str) -> Option<Self> {
        let name = sys.split_whitespace().next()?;
        Some(match name {
            "Linux" => Self::Linux,
            "Darwin" | "FreeBSD" | "OpenBSD" | "NetBSD" | "DragonFly" | "SunOS" | "AIX" => {
                Self::Unix
            }
            _ => return None,
        })
    }

    /// 本地 shell 的命令行环境。Git Bash 只承诺 POSIX 工具，不按 Windows 命令算。
    pub fn from_shell_kind(kind: ShellKind) -> Option<Self> {
        match kind {
            ShellKind::PowerShell | ShellKind::Cmd => Some(Self::Windows),
            ShellKind::Wsl => Some(Self::Linux),
            ShellKind::Bash => Some(Self::Unix),
            ShellKind::Other => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::Platform;

    #[test]
    fn uname_names_map_to_platforms() {
        assert_eq!(Platform::from_uname("Linux 5.15.0-119-generic"), Some(Platform::Linux));
        assert_eq!(Platform::from_uname("Darwin 23.6.0"), Some(Platform::Unix));
        assert_eq!(Platform::from_uname("FreeBSD 14.1-RELEASE"), Some(Platform::Unix));
        assert_eq!(Platform::from_uname("SunOS 5.11"), Some(Platform::Unix));
        assert_eq!(Platform::from_uname(""), None);
        assert_eq!(Platform::from_uname("Windows_NT"), None);
    }
}

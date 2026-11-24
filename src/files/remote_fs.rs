//! 远端文件浏览：目录列举、新建 / 重命名 / 删除与路径处理。
//!
//! 所有操作都走 SFTP API，远端路径只作为协议参数，绝不拼进 shell 命令。
//! 路径规则与 POSIX 一致（`/` 分隔、`~` 指向登录主目录）。

use anyhow::{Context as _, Result};
use russh_sftp::client::SftpSession;
use std::time::UNIX_EPOCH;

/// 目录里的一项（不含 `.` / `..`）。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Entry {
    pub name: String,
    pub is_dir: bool,
    pub is_symlink: bool,
    pub size: u64,
    /// 修改时间（Unix 秒）；服务器没给时为 None。
    pub modified: Option<u32>,
    /// 原始 st_mode（含权限位）；服务器没给时为 None。
    pub permissions: Option<u32>,
}

impl Entry {
    /// 权限串（不含文件类型位），如 `rwxr-xr-x`；服务器未提供时为 `—`。
    pub fn permissions_text(&self) -> String {
        match self.permissions {
            Some(mode) => permission_letters(mode),
            None => "—".to_string(),
        }
    }

    /// 类型列文案：目录 / 链接 / 文件。
    pub fn kind_text(&self) -> &'static str {
        if self.is_dir {
            "目录"
        } else if self.is_symlink {
            "链接"
        } else {
            "文件"
        }
    }
}

/// st_mode 低 9 位 → `rwxrwxrwx` 风格。
pub fn permission_letters(mode: u32) -> String {
    let bits = [
        (0o400, 'r'),
        (0o200, 'w'),
        (0o100, 'x'),
        (0o040, 'r'),
        (0o020, 'w'),
        (0o010, 'x'),
        (0o004, 'r'),
        (0o002, 'w'),
        (0o001, 'x'),
    ];
    bits.iter()
        .map(|(bit, letter)| if mode & bit != 0 { *letter } else { '-' })
        .collect()
}

/// 一次目录列举的结果。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Listing {
    /// SFTP 规范化后的绝对路径（`.` / `~` 已展开）。
    pub path: String,
    /// 上级目录；根目录为 None。
    pub parent: Option<String>,
    pub entries: Vec<Entry>,
}

/// 列出目录内容，目录在前、同类按名称排序。
pub async fn list(sftp: &SftpSession, path: Option<&str>) -> Result<Listing> {
    let raw = path
        .map(str::trim)
        .filter(|path| !path.is_empty())
        .unwrap_or(".");
    let resolved = crate::transfer::resolve_remote(sftp, raw).await?;
    let path = sftp
        .canonicalize(resolved.as_str())
        .await
        .with_context(|| format!("无法打开远端目录 {resolved}"))?;

    let mut entries: Vec<Entry> = sftp
        .read_dir(path.as_str())
        .await
        .with_context(|| format!("读取目录失败：{path}"))?
        .map(|entry| {
            let metadata = entry.metadata();
            Entry {
                name: entry.file_name(),
                is_dir: metadata.is_dir(),
                is_symlink: metadata.is_symlink(),
                size: metadata.len(),
                modified: metadata
                    .modified()
                    .ok()
                    .and_then(|time| time.duration_since(UNIX_EPOCH).ok())
                    .map(|duration| duration.as_secs().min(u64::from(u32::MAX)) as u32),
                permissions: metadata.permissions,
            }
        })
        .collect();
    // cached_key：小写形式每条只算一次。sort_by 现场比较的话，O(n log n) 次
    // 比较 × 2 次堆分配——万条目录一次列表就是二十多万次分配。
    entries
        .sort_by_cached_key(|entry| (!entry.is_dir, entry.name.to_lowercase(), entry.name.clone()));

    Ok(Listing {
        parent: parent_of(&path),
        path,
        entries,
    })
}

/// 新建目录，返回新目录的完整路径。
pub async fn mkdir(sftp: &SftpSession, dir: &str, name: &str) -> Result<String> {
    let name = checked_name(name)?;
    let path = child(dir, name);
    sftp.create_dir(path.as_str())
        .await
        .with_context(|| format!("无法创建目录 {path}（没有写权限？）"))?;
    Ok(path)
}

/// 重命名 / 移动同一目录下的条目，返回新路径。
pub async fn rename(sftp: &SftpSession, dir: &str, from: &str, to: &str) -> Result<String> {
    let to = checked_name(to)?;
    if to == from {
        return Ok(child(dir, from));
    }
    let old = child(dir, from);
    let new = child(dir, to);
    if sftp.metadata(new.as_str()).await.is_ok() {
        anyhow::bail!("{new} 已存在");
    }
    sftp.rename(old.as_str(), new.as_str())
        .await
        .with_context(|| format!("无法重命名 {old} → {new}"))?;
    Ok(new)
}

/// 删除文件或空目录（不递归，避免误删整个目录树）。
pub async fn delete(sftp: &SftpSession, path: &str) -> Result<()> {
    let metadata = sftp
        .metadata(path)
        .await
        .with_context(|| format!("远端路径不可访问：{path}"))?;
    if metadata.is_dir() {
        sftp.remove_dir(path)
            .await
            .with_context(|| format!("无法删除目录 {path}（目录非空或没有权限）"))
    } else {
        sftp.remove_file(path)
            .await
            .with_context(|| format!("无法删除文件 {path}"))
    }
}

/// 路径拼接：处理根目录与末尾斜杠，不重复加 `/`。
pub fn child(dir: &str, name: &str) -> String {
    let dir = dir.trim_end_matches('/');
    if dir.is_empty() {
        format!("/{name}")
    } else {
        format!("{dir}/{name}")
    }
}

/// 上级目录；根目录与相对路径没有上级。
pub fn parent_of(path: &str) -> Option<String> {
    let path = path.trim_end_matches('/');
    if path.is_empty() || !path.starts_with('/') {
        return None;
    }
    match path.rfind('/') {
        Some(0) => Some("/".to_string()),
        Some(index) => Some(path[..index].to_string()),
        None => None,
    }
}

/// 文件 / 目录名只允许单段：不含分隔符，也不是 `.` / `..`。
fn checked_name(name: &str) -> Result<&str> {
    let name = name.trim();
    if name.is_empty()
        || name == "."
        || name == ".."
        || name.contains('/')
        || name.contains('\\')
        || name.contains('\0')
    {
        anyhow::bail!("名称不能为空、不能包含路径分隔符，也不能是 . 或 ..");
    }
    Ok(name)
}

#[cfg(test)]
mod tests {
    use super::{child, parent_of, permission_letters};

    #[test]
    fn permission_letters_render_mode_bits() {
        assert_eq!(permission_letters(0o755), "rwxr-xr-x");
        assert_eq!(permission_letters(0o644), "rw-r--r--");
        assert_eq!(permission_letters(0o000), "---------");
    }

    #[test]
    fn child_joins_posix_paths() {
        assert_eq!(child("/home/tester", "a.txt"), "/home/tester/a.txt");
        assert_eq!(child("/home/tester/", "docs"), "/home/tester/docs");
        assert_eq!(child("/", "etc"), "/etc");
    }

    #[test]
    fn parent_of_walks_up_to_root() {
        assert_eq!(parent_of("/home/tester"), Some("/home".into()));
        assert_eq!(parent_of("/home"), Some("/".into()));
        assert_eq!(parent_of("/home/"), Some("/".into()));
        assert_eq!(parent_of("/"), None);
        assert_eq!(parent_of("relative"), None);
    }
}

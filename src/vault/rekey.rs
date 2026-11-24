//! 数据库文件换名与主密码重设的事务恢复。
//!
//! Windows 上 turso 连接释放有延迟，换名必须重试；重设主密码期间用事务日志
//! 记录进度，崩溃后启动时据此决定前滚或回滚，绝不留下半新半旧的库文件。

use std::path::{Path, PathBuf};

use anyhow::{Context as _, Result};

use crate::paths::StorageMode;

use super::{UnlockBackend, write_new_synced};

/// 文件换名（带重试，每 100ms 一次）：turso/toasty 的连接由 tokio 运行时异步回收，
/// Windows 上句柄可能滞留片刻，立刻改名会撞上"文件被占用"；休眠必须在运行时内进行。
pub(crate) async fn rename_with_retry(
    rt: &tokio::runtime::Handle,
    from: PathBuf,
    to: PathBuf,
    attempts: u32,
) -> Result<(), String> {
    let (from_desc, to_desc) = (from.display().to_string(), to.display().to_string());
    let moved = rt
        .spawn(async move {
            let mut last: Option<std::io::Error> = None;
            for _ in 0..attempts {
                match std::fs::rename(&from, &to) {
                    Ok(()) => return Ok(()),
                    Err(error) => {
                        last = Some(error);
                        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
                    }
                }
            }
            Err(last.unwrap_or_else(|| std::io::Error::other("文件仍被占用")))
        })
        .await
        .map_err(|error| format!("换名任务意外中止: {error}"))?;
    moved.map_err(|error| format!("换名 {from_desc} → {to_desc} 失败: {error}"))
}

/// 连同 SQLite 的 `-wal` / `-shm` 边文件一起换名：边文件按主文件名配对，
/// 只挪走主文件会让旧库的 WAL 挂到新库上（密钥不同），必须整套搬走。
pub(crate) async fn rename_db_bundle(
    rt: &tokio::runtime::Handle,
    base: PathBuf,
    target: PathBuf,
    attempts: u32,
) -> Result<(), String> {
    // 先搬边文件再搬主文件：只要 WAL 还留在原处，新库就位后就会把旧库的 WAL
    // 当成自己的挂上去（页号对不上直接毁数据），所以边文件搬不动就整体作罢。
    let sides = ["-wal", "-shm"]
        .iter()
        .filter_map(|suffix| {
            let name = base.file_name()?.to_str()?;
            let side = base.with_file_name(format!("{name}{suffix}"));
            side.exists().then(|| {
                let target_name = target.file_name().and_then(|n| n.to_str()).unwrap_or("db");
                (
                    side,
                    target.with_file_name(format!("{target_name}{suffix}")),
                )
            })
        })
        .collect::<Vec<(PathBuf, PathBuf)>>();
    for (side, side_target) in sides {
        rename_with_retry(rt, side, side_target, attempts)
            .await
            .map_err(|error| format!("{error}（边文件未让位，主文件保持不动）"))?;
    }
    rename_with_retry(rt, base, target, attempts).await
}

/// 删除整套库文件（主文件 + WAL/SHM 边文件）。
///
/// 只用于已确认无用的残留：暂存库的上一轮草稿，或换密钥成功后的旧库备份。
pub(crate) fn remove_db_bundle(base: &Path) {
    let Some(name) = base.file_name().and_then(|n| n.to_str()) else {
        return;
    };
    for suffix in ["", "-wal", "-shm"] {
        let path = if suffix.is_empty() {
            base.to_path_buf()
        } else {
            base.with_file_name(format!("{name}{suffix}"))
        };
        if path.exists()
            && let Err(error) = std::fs::remove_file(&path)
        {
            log::warn!("删除 {} 失败: {error}", path.display());
        }
    }
}

/// 换主密码的事务日志路径（公开数据，明文：两把盐本就明文落盘）。
fn rekey_journal_path(mode: &StorageMode) -> std::path::PathBuf {
    mode.db_path().with_file_name("ruterm.db.rekey-pending")
}

/// 换主密码的事务日志：动库之前落盘，崩溃后 `recover_rekey` 据此判断
/// 主库当前是新是旧，并把盐 / 解锁标记配平。
#[derive(Clone, Debug)]
struct RekeyJournal {
    new_salt: Option<Vec<u8>>,
    old_salt: Option<Vec<u8>>,
    backend: UnlockBackend,
}

/// 在动库之前写入事务日志（原子写）。
pub(crate) fn write_rekey_journal(
    mode: &StorageMode,
    new_salt: Option<&[u8; 16]>,
    old_salt: Option<&[u8]>,
    backend: UnlockBackend,
) -> Result<()> {
    // [0] 新盐有无；[1..17) 新盐；[17] 旧盐有无；[18..34) 旧盐；[34] 后端。
    let mut bytes = Vec::with_capacity(35);
    match new_salt {
        Some(salt) => {
            bytes.push(1);
            bytes.extend_from_slice(salt);
        }
        None => {
            bytes.push(0);
            bytes.extend_from_slice(&[0u8; 16]);
        }
    }
    match old_salt {
        Some(salt) => {
            bytes.push(1);
            bytes.extend_from_slice(salt);
        }
        None => {
            bytes.push(0);
            bytes.extend_from_slice(&[0u8; 16]);
        }
    }
    bytes.push(backend.as_str().as_bytes()[0]);
    write_new_synced(&rekey_journal_path(mode), &bytes).context("写入换密钥事务日志失败")
}

/// 事务干净结束（成功或已回滚）后清掉日志。
pub(crate) fn clear_rekey_journal(mode: &StorageMode) {
    let _ = std::fs::remove_file(rekey_journal_path(mode));
}

fn read_rekey_journal(mode: &StorageMode) -> Result<Option<RekeyJournal>> {
    let path = rekey_journal_path(mode);
    let bytes = match std::fs::read(&path) {
        Ok(bytes) => bytes,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => {
            return Err(error).with_context(|| format!("读取 {} 失败", path.display()));
        }
    };
    // 日志读不出就拒绝自动处理：配平错盐比停机等人工更糟。
    if bytes.len() != 35 {
        anyhow::bail!(
            "换密钥事务日志 {} 长度异常（{} 字节），已拒绝自动恢复（请保留现场手工处理）",
            path.display(),
            bytes.len()
        );
    }
    let backend = match bytes[34] {
        b'k' => UnlockBackend::Keyring,
        b'p' => UnlockBackend::Passphrase,
        _ => anyhow::bail!("换密钥事务日志 {} 内容异常，已拒绝自动恢复", path.display()),
    };
    Ok(Some(RekeyJournal {
        new_salt: (bytes[0] == 1).then(|| bytes[1..17].to_vec()),
        old_salt: (bytes[17] == 1).then(|| bytes[18..34].to_vec()),
        backend,
    }))
}

/// 收集旧库备份候选：`keyring-bak` 最优先，其余带时间戳的按修改时间取最新。
fn collect_backups(db: &Path) -> Vec<std::path::PathBuf> {
    let mut backups = Vec::new();
    let primary = db.with_file_name("ruterm.db.keyring-bak");
    if primary.exists() {
        backups.push(primary);
    }
    if let Some(dir) = db.parent()
        && let Ok(entries) = std::fs::read_dir(dir)
    {
        let mut stamped: Vec<(std::path::PathBuf, std::time::SystemTime)> = entries
            .flatten()
            .filter(|entry| {
                entry
                    .file_name()
                    .to_string_lossy()
                    .starts_with("ruterm.db.bak-")
            })
            .filter_map(|entry| {
                let modified = entry.metadata().and_then(|m| m.modified()).ok()?;
                Some((entry.path(), modified))
            })
            .collect();
        // read_dir 顺序取决于文件系统：按修改时间降序，回滚才总是取到最新的旧库。
        stamped.sort_by_key(|entry| std::cmp::Reverse(entry.1));
        backups.extend(stamped.into_iter().map(|(path, _)| path));
    }
    backups
}

/// 启动时修复上次中断的换主密码事务。
///
/// 换密钥按“落事务日志 → 旧库改名成备份 → 新库就位 → 换盐 → 换标记 → 清日志”的
/// 顺序进行。启动时结合事务日志判断：
/// - 主库不在、有备份：换名完成前崩溃，把备份搬回原处，删草稿与日志；
/// - 主库在、日志在：换名之后崩溃。当前盐已等于日志里的新盐（或本轮不换盐）
///   则补写解锁标记、把事务补完；否则新库退位，旧库备份回原处、旧盐还原；
/// - 主库在、日志不在：没有未完成的事务，残留的 `ruterm.db.new` 是上一轮草稿，删掉；
/// - 主库与备份都不在却仍有草稿：无法判断草稿对应哪份口令，拒绝自动处理并保留现场。
pub fn recover_rekey(mode: &StorageMode) -> Result<()> {
    let db = mode.db_path();
    let staging = db.with_file_name("ruterm.db.new");
    let journal = read_rekey_journal(mode)?;
    if db.exists() {
        if let Some(journal) = journal {
            return recover_rekey_in_place(mode, &staging, journal);
        }
        remove_db_bundle(&staging);
        return Ok(());
    }
    let backups = collect_backups(&db);
    if let Some(backup) = backups.into_iter().next() {
        rename_bundle_sync(&backup, &db)
            .with_context(|| format!("恢复旧库 {} 失败", backup.display()))?;
        remove_db_bundle(&staging);
        clear_rekey_journal(mode);
        log::warn!(
            "检测到中断的主密码更换，已将旧库备份 {} 恢复为 {}",
            backup.display(),
            db.display()
        );
        return Ok(());
    }
    if staging.exists() {
        anyhow::bail!(
            "检测到中断的主密码更换：{} 不存在且找不到旧库备份，已拒绝自动处理（请保留现有文件并手工恢复）",
            db.display()
        );
    }
    Ok(())
}

/// 主库已就位但事务日志仍在：崩溃点在换名之后、清理之前。
fn recover_rekey_in_place(mode: &StorageMode, staging: &Path, journal: RekeyJournal) -> Result<()> {
    let db = mode.db_path();
    let salt_path = mode.salt_path();
    let current_salt = std::fs::read(&salt_path).ok();
    let salt_is_new = match &journal.new_salt {
        // 本轮换了盐：只有当前盐文件确实等于新盐，才认定事务已推进过换盐这一步。
        Some(new_salt) => current_salt.as_deref() == Some(new_salt.as_slice()),
        // 本轮不换盐（换回 keyring）：盐本就不该动，视为已配平。
        None => true,
    };
    if salt_is_new {
        // 新库 + 新盐已成对，只差解锁标记：补写标记即事务补完。旧库备份保守
        // 起见保留在原地（数据完整性在上轮运行里没能最终确认），可手工清理。
        write_new_synced(
            &mode.unlock_marker_path(),
            journal.backend.as_str().as_bytes(),
        )
        .with_context(|| format!("补写 {}", mode.unlock_marker_path().display()))?;
        remove_db_bundle(staging);
        clear_rekey_journal(mode);
        log::warn!(
            "检测到中断的主密码更换：新库与新盐已配平，已补写解锁标记为 {}，旧库备份保留待人工清理",
            journal.backend.as_str()
        );
        return Ok(());
    }
    // 盐还是旧的：换盐没发生或没生效，新库退位、旧库备份回原处、旧盐还原。
    // 解锁标记此刻必然还是旧值（换标记排在换盐之后），无需处理。
    let backups = collect_backups(&db);
    let Some(backup) = backups.into_iter().next() else {
        anyhow::bail!(
            "检测到中断的主密码更换：主库与旧盐不匹配但找不到旧库备份，已拒绝自动处理（请保留现有文件并手工恢复）"
        );
    };
    rename_bundle_sync(&backup, &db)
        .with_context(|| format!("恢复旧库 {} 失败", backup.display()))?;
    match journal.old_salt {
        Some(old_salt) => {
            write_new_synced(&salt_path, &old_salt).with_context(|| "还原旧派生盐失败")?;
        }
        None => {
            let _ = std::fs::remove_file(&salt_path);
        }
    }
    remove_db_bundle(staging);
    clear_rekey_journal(mode);
    log::warn!(
        "检测到中断的主密码更换，已回滚旧库（备份 {} 恢复为 {}）",
        backup.display(),
        db.display()
    );
    Ok(())
}

/// 同步版整套换名：边文件先走，主文件最后，理由同 [`rename_db_bundle`]。
fn rename_bundle_sync(from: &Path, to: &Path) -> Result<()> {
    let name = from
        .file_name()
        .and_then(|n| n.to_str())
        .context("库文件名无效")?;
    let target_name = to
        .file_name()
        .and_then(|n| n.to_str())
        .context("目标库文件名无效")?;
    for suffix in ["-wal", "-shm"] {
        let side = from.with_file_name(format!("{name}{suffix}"));
        if side.exists() {
            let target = to.with_file_name(format!("{target_name}{suffix}"));
            std::fs::rename(&side, &target)
                .with_context(|| format!("恢复 {} 失败", side.display()))?;
        }
    }
    std::fs::rename(from, to)
        .with_context(|| format!("恢复 {} → {} 失败", from.display(), to.display()))
}

/// 改名保留（同步版：用于从未被打开过的普通文本/JSON 文件）。
pub(crate) fn rename_preserving(path: &Path) {
    let target = path.with_file_name(format!(
        "{}.migrated",
        path.file_name().and_then(|n| n.to_str()).unwrap_or("data")
    ));
    if target.exists() {
        log::warn!(
            "{} 已存在，保留 {} 不重复改名",
            target.display(),
            path.display()
        );
        return;
    }
    if let Err(error) = std::fs::rename(path, &target) {
        log::warn!("迁移后改名 {} 失败: {error}", path.display());
    }
}

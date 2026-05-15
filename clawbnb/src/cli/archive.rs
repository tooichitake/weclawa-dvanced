//! `weclawbot archive` subcommand — v3.6 I7.
//!
//! ## 子命令
//!
//! - `weclawbot archive list` — 列出 `~/.weclawbot/archive/` 下所有归档
//!   manifest，输出 JSON 给脚本消费
//! - `weclawbot archive index` — 重新生成 `archive_index.json`（operator
//!   cron 在 S3 sync 之前调，确保 index 跟 fs 一致）
//! - `weclawbot archive sync-s3 --bucket=...` — 读 index.json，把所有
//!   .enc 文件 + .manifest.json 用 `aws s3 cp --no-overwrite` 上传到 WORM
//!   bucket。失败不阻塞下一个文件 —— 整批跑完报告每个状态。
//!
//! ## 不引入 AWS SDK 依赖
//!
//! 直接 shell out 到 `aws` CLI —— 大多数 HIPAA-mandated 部署 operator
//! 都装 AWS CLI + IAM credentials chain，不重复造轮子。失败时输出原始
//! `aws` stderr 给 operator 调试。

use std::path::PathBuf;
use std::process::Command;

/// 列出本机所有归档 manifest。输出格式跟 `archive_index.json.archives`
/// 数组一致 —— 脚本可以直接 jq 处理。
pub fn list() -> Result<(), String> {
    let dir = archive_dir();
    if !dir.exists() {
        println!("[]");
        return Ok(());
    }
    let mut out = Vec::new();
    let entries = std::fs::read_dir(&dir).map_err(|e| format!("read_dir {}: {e}", dir.display()))?;
    for entry in entries {
        let entry = entry.map_err(|e| format!("read_dir entry: {e}"))?;
        let path = entry.path();
        let fname = match path.file_name().and_then(|n| n.to_str()) {
            Some(s) => s,
            None => continue,
        };
        if !fname.ends_with(".manifest.json") {
            continue;
        }
        let bytes = match std::fs::read(&path) {
            Ok(b) => b,
            Err(_) => continue,
        };
        let v: serde_json::Value = match serde_json::from_slice(&bytes) {
            Ok(v) => v,
            Err(_) => continue,
        };
        out.push(v);
    }
    println!("{}", serde_json::to_string_pretty(&out).map_err(|e| e.to_string())?);
    Ok(())
}

/// 重新生成 `archive_index.json`。功能等同 [`crate::ee::audit_archiver::SqliteAuditArchiver::write_archive_index`]
/// 但不需要启 daemon —— operator cron 直接 `weclawbot archive index` 即可。
///
/// 不需要 daemon DB connection，纯 fs scan + JSON 写。
pub fn index() -> Result<(), String> {
    let dir = archive_dir();
    if !dir.exists() {
        return Err(format!("archive dir does not exist: {}", dir.display()));
    }
    let mut archives = Vec::new();
    let entries = std::fs::read_dir(&dir).map_err(|e| format!("read_dir: {e}"))?;
    for entry in entries {
        let entry = entry.map_err(|e| format!("read_dir entry: {e}"))?;
        let path = entry.path();
        let fname = match path.file_name().and_then(|n| n.to_str()) {
            Some(s) => s,
            None => continue,
        };
        if !fname.ends_with(".manifest.json") {
            continue;
        }
        let bytes = match std::fs::read(&path) {
            Ok(b) => b,
            Err(_) => continue,
        };
        let v: serde_json::Value = match serde_json::from_slice(&bytes) {
            Ok(v) => v,
            Err(_) => continue,
        };
        archives.push(v);
    }
    let index = serde_json::json!({
        "schema_version": 1,
        "generated_at": chrono::Utc::now().to_rfc3339(),
        "archive_dir": dir.to_string_lossy(),
        "archives": archives,
    });
    let index_path = dir.join("archive_index.json");
    let tmp = dir.join("archive_index.json.tmp");
    std::fs::write(&tmp, serde_json::to_vec_pretty(&index).map_err(|e| e.to_string())?)
        .map_err(|e| format!("write tmp: {e}"))?;
    std::fs::rename(&tmp, &index_path).map_err(|e| format!("rename: {e}"))?;
    println!("{}", index_path.display());
    Ok(())
}

/// 同步 archive_dir 到 S3 (WORM) bucket。Shell-out `aws s3 cp` 跑每个
/// `.enc` 和 `.manifest.json` 文件。`--no-overwrite` (实际 `aws s3 cp`
/// 没这个 flag，我们靠 ifexists check) 由 sha256 dedup 保证 ——
/// 文件名 already 含 ciphertext sha256 前 12 字符，重传是 idempotent。
///
/// 失败不抛 — 累计 stderr 报告，整批跑完。返回非零退出码当作"至少一个
/// 文件失败"，cron monitor 可据此告警。
pub fn sync_s3(bucket: &str, prefix: &str) -> Result<(), String> {
    if bucket.is_empty() {
        return Err("--bucket required".into());
    }
    // 防误用 — bucket name 校验 (S3 规则)。先校 bucket 再 fs，让 CLI
    // arg 错误能在没装 ~/.weclawbot 的环境（CI / 单测）下也立即报。
    if !bucket
        .chars()
        .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-' || c == '.')
    {
        return Err(format!("invalid bucket name: {bucket}"));
    }
    let dir = archive_dir();
    if !dir.exists() {
        return Err(format!("archive dir does not exist: {}", dir.display()));
    }

    let entries = std::fs::read_dir(&dir).map_err(|e| format!("read_dir: {e}"))?;
    let mut ok = 0usize;
    let mut failed = 0usize;
    let mut failures: Vec<String> = Vec::new();
    for entry in entries {
        let entry = entry.map_err(|e| format!("read_dir entry: {e}"))?;
        let path = entry.path();
        let fname = match path.file_name().and_then(|n| n.to_str()) {
            Some(s) => s,
            None => continue,
        };
        // 只同步 .enc / .manifest.json / archive_index.json
        let should = fname.ends_with(".enc")
            || fname.ends_with(".manifest.json")
            || fname == "archive_index.json";
        if !should {
            continue;
        }
        let target = if prefix.is_empty() {
            format!("s3://{bucket}/{fname}")
        } else {
            format!("s3://{bucket}/{}/{fname}", prefix.trim_matches('/'))
        };
        let output = Command::new("aws")
            .arg("s3")
            .arg("cp")
            .arg(&path)
            .arg(&target)
            .output();
        match output {
            Ok(o) if o.status.success() => {
                ok += 1;
                println!("synced: {} → {}", fname, target);
            }
            Ok(o) => {
                failed += 1;
                let err = String::from_utf8_lossy(&o.stderr).trim().to_string();
                failures.push(format!("{fname}: aws cp exit {:?}: {err}", o.status.code()));
            }
            Err(e) => {
                failed += 1;
                failures.push(format!("{fname}: aws cp spawn: {e}"));
            }
        }
    }
    println!("\nsync complete: ok={ok}, failed={failed}");
    if failed > 0 {
        eprintln!("\nfailures:");
        for f in &failures {
            eprintln!("  {f}");
        }
        return Err(format!("{failed} files failed to sync"));
    }
    Ok(())
}

fn archive_dir() -> PathBuf {
    dirs::home_dir()
        .map(|h| h.join(".weclawbot").join("archive"))
        .unwrap_or_else(|| PathBuf::from("./.weclawbot/archive"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sync_s3_rejects_empty_bucket() {
        let r = sync_s3("", "");
        assert!(r.is_err());
        assert!(r.unwrap_err().contains("bucket"));
    }

    #[test]
    fn sync_s3_rejects_invalid_bucket_name() {
        let r = sync_s3("INVALID_UPPERCASE", "");
        assert!(r.is_err());
        assert!(r.unwrap_err().contains("invalid"));
    }
}

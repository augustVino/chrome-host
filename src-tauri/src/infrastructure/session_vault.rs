//! 会话保险库：环境身份层的 Cookie 部分。
//!
//! - 当前版本 `session-vault.json`（0600——明文凭证，含 HttpOnly cookie）；
//! - 历史代 `session-vault-history/gen-<n>.json`，保留最近 [`MAX_GENERATIONS`] 代供回滚；
//! - 实例私有导出 `<ins>/chrome-host-cookies.json`（随实例目录生命周期自动清理）。
//!
//! 与文件层快照（login-profile 目录，ProfileService 负责）互补：
//! 文件层覆盖 IndexedDB/localStorage 等磁盘态，本模块覆盖含 session cookie
//! 在内的运行时 Cookie——Chrome 从不把 session cookie 落盘，文件层原理上拿不到。

use std::io::Write;
use std::path::Path;

use serde::{Deserialize, Serialize};

use crate::error::AppError;

/// 保险库历史保留代数
pub const MAX_GENERATIONS: usize = 5;

/// CDP Cookie 记录（Storage.getCookies 返回项的最小子集；
/// 字段与 Storage.setCookies 入参同构，M3 注入时原样回传）
#[derive(Debug, Clone, Serialize, Deserialize, Default, PartialEq)]
#[serde(rename_all = "camelCase", default)]
pub struct CookieRecord {
    pub name: String,
    pub value: String,
    pub domain: String,
    pub path: String,
    /// CDP 语义：unix 秒；-1 = 会话 Cookie（仅存在于内存，正是文件层拿不到的部分）
    pub expires: f64,
    pub http_only: bool,
    pub secure: bool,
    pub session: bool,
    /// "Strict" | "Lax" | "None"
    pub same_site: Option<String>,
}

/// 保险库内容（当前版本与历史代同构）
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct SessionVault {
    pub generation: i64,
    pub exported_at: i64,
    pub source_instance_id: Option<String>,
    pub cookies: Vec<CookieRecord>,
}

/// 实例私有 Cookie 导出（generation 无意义，仅复用解析/写盘逻辑）
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct InstanceCookieExport {
    pub exported_at: i64,
    pub instance_id: String,
    pub cookies: Vec<CookieRecord>,
}

/// 从 CDP Storage.getCookies 的 result 对象解析 Cookie 列表（纯函数）。
/// 单条解析失败跳过该条，不让个别异形字段毁掉整次导出。
pub fn parse_cdp_cookies(result: &serde_json::Value) -> Vec<CookieRecord> {
    result
        .get("cookies")
        .and_then(|c| c.as_array())
        .map(|arr| {
            arr.iter()
                .filter_map(|c| serde_json::from_value::<CookieRecord>(c.clone()).ok())
                .collect()
        })
        .unwrap_or_default()
}

/// CookieRecord → Storage.setCookies 的 CookieParam（纯函数）。
/// 两个协议差异（实测踩坑）：
/// - `session` 是 getCookies 的输出字段，CookieParam 不接受，必须剔除；
/// - 会话 cookie（expires ≤ 0）应**省略 expires 字段**，传 -1 会被拒 -32602；
/// - sameSite "Unspecified" 不在 CookieParam 枚举内，剔除。
pub fn to_set_cookie_params(cookies: &[CookieRecord]) -> Vec<serde_json::Value> {
    cookies
        .iter()
        .map(|c| {
            let mut v = serde_json::json!({
                "name": c.name,
                "value": c.value,
                "domain": c.domain,
                "path": c.path,
                "secure": c.secure,
                "httpOnly": c.http_only,
            });
            if c.expires > 0.0 {
                v["expires"] = serde_json::json!(c.expires);
            }
            if let Some(s) = &c.same_site {
                if s != "Unspecified" && !s.is_empty() {
                    v["sameSite"] = serde_json::json!(s);
                }
            }
            v
        })
        .collect()
}

fn write_0600(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        let mut f = std::fs::OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .mode(0o600)
            .open(path)?;
        f.write_all(bytes)
    }
    #[cfg(not(unix))]
    {
        let mut f = std::fs::File::create(path)?;
        f.write_all(bytes)
    }
}

/// 读当前保险库；不存在或损坏返回 None（调用方按"身份层为空"处理）
pub fn read_current(env_dir: &Path) -> Option<SessionVault> {
    let data = std::fs::read_to_string(env_dir.join("session-vault.json")).ok()?;
    serde_json::from_str(&data).ok()
}

/// 写入新当前版本：旧当前转入 history，修剪至 MAX_GENERATIONS 代。
/// generation 单调 +1。返回写入的新 vault。
/// 原子性（P1）：当前文件经 tmp+rename 落盘——写入中断不会留下半写坏的
/// session-vault.json（读侧对损坏文件按空身份处理，半写意味着静默丢登录态）。
/// history 代文件是新名写入无覆盖冲突，维持直写。跨路径互斥由 SessionService
/// 的进程内锁保证，本函数不重复加锁。
pub fn write_new(
    env_dir: &Path,
    cookies: Vec<CookieRecord>,
    source_instance_id: Option<String>,
) -> Result<SessionVault, AppError> {
    let current = read_current(env_dir);
    let generation = current.as_ref().map(|v| v.generation).unwrap_or(0) + 1;

    if let Some(old) = current {
        let hist = env_dir.join("session-vault-history");
        std::fs::create_dir_all(&hist)?;
        let bytes = serde_json::to_vec(&old)?;
        write_0600(&hist.join(format!("gen-{}.json", old.generation)), &bytes)?;
        prune(&hist)?;
    }

    let vault = SessionVault {
        generation,
        exported_at: chrono::Utc::now().timestamp_millis(),
        source_instance_id,
        cookies,
    };
    let bytes = serde_json::to_vec_pretty(&vault)?;
    let target = env_dir.join("session-vault.json");
    let tmp = env_dir.join(".session-vault.json.tmp");
    write_0600(&tmp, &bytes)?;
    std::fs::rename(&tmp, &target)?;
    Ok(vault)
}

/// 历史代列表（新→旧）
pub fn list_history(env_dir: &Path) -> Vec<SessionVault> {
    let hist = env_dir.join("session-vault-history");
    let mut out: Vec<SessionVault> = std::fs::read_dir(&hist)
        .into_iter()
        .flatten()
        .flatten()
        .filter_map(|e| {
            let name = e.file_name().to_string_lossy().to_string();
            if !name.starts_with("gen-") || !name.ends_with(".json") {
                return None;
            }
            std::fs::read(e.path()).ok().and_then(|b| serde_json::from_slice(&b).ok())
        })
        .collect();
    out.sort_by(|a, b| b.generation.cmp(&a.generation));
    out
}

/// 回滚到指定历史代：该代内容成为新的当前版本（generation 前进为 max+1，
/// 保持单调）；旧当前照常转入 history。404 语义：代文件不存在。
pub fn restore(env_dir: &Path, generation: i64) -> Result<SessionVault, AppError> {
    let path = env_dir
        .join("session-vault-history")
        .join(format!("gen-{generation}.json"));
    let data = std::fs::read(&path).map_err(|_| {
        AppError::not_found(
            "SESSION_GENERATION_NOT_FOUND",
            format!("第 {generation} 代保险库历史不存在"),
        )
    })?;
    let restored: SessionVault =
        serde_json::from_slice(&data).map_err(|e| AppError::internal(format!("历史代损坏: {e}")))?;
    write_new(env_dir, restored.cookies, restored.source_instance_id)
}

/// 实例私有导出读写
pub fn write_instance_export(path: &Path, instance_id: &str, cookies: &[CookieRecord]) -> Result<(), AppError> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?; // 实例目录正常已存在，防御性创建
    }
    let export = InstanceCookieExport {
        exported_at: chrono::Utc::now().timestamp_millis(),
        instance_id: instance_id.to_string(),
        cookies: cookies.to_vec(),
    };
    let bytes = serde_json::to_vec(&export)?;
    write_0600(path, &bytes)?;
    Ok(())
}

pub fn read_instance_export(path: &Path) -> Option<InstanceCookieExport> {
    let data = std::fs::read_to_string(path).ok()?;
    serde_json::from_str(&data).ok()
}

/// 清空保险库（当前 + 全部历史）。best-effort：不存在视为成功。
pub fn clear(env_dir: &Path) -> Result<(), AppError> {
    let _ = std::fs::remove_file(env_dir.join("session-vault.json"));
    let _ = std::fs::remove_file(env_dir.join(".session-vault.json.tmp"));
    let _ = std::fs::remove_dir_all(env_dir.join("session-vault-history"));
    Ok(())
}

fn prune(hist: &Path) -> Result<(), AppError> {
    let mut gens: Vec<(i64, std::path::PathBuf)> = std::fs::read_dir(hist)?
        .flatten()
        .filter_map(|e| {
            let name = e.file_name().to_string_lossy().to_string();
            let n = name
                .strip_prefix("gen-")?
                .strip_suffix(".json")?
                .parse::<i64>()
                .ok()?;
            Some((n, e.path()))
        })
        .collect();
    gens.sort_by(|a, b| b.0.cmp(&a.0)); // 新 → 旧
    for (_, path) in gens.into_iter().skip(MAX_GENERATIONS) {
        let _ = std::fs::remove_file(path); // best-effort：修剪失败不影响主流程
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp_dir() -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!("vault-test-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn cookie(name: &str) -> CookieRecord {
        CookieRecord { name: name.into(), value: "v".into(), domain: "a.com".into(), ..Default::default() }
    }

    #[test]
    fn parse_tolerates_malformed_entries() {
        let result = serde_json::json!({ "cookies": [
            { "name": "a", "value": "1", "domain": "a.com", "path": "/",
              "expires": 123.0, "httpOnly": true, "secure": false, "session": false,
              "sameSite": "Lax", "extraUnknownField": 1 },
            { "name": 123 }, // 异形：跳过
        ]});
        let out = parse_cdp_cookies(&result);
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].name, "a");
        assert!(out[0].http_only);
        assert_eq!(out[0].same_site.as_deref(), Some("Lax"));
    }

    /// 原子落盘（P1）：写后 tmp 不残留、当前文件完整可读
    #[test]
    fn write_new_leaves_no_tmp_and_reads_back() {
        let dir = tmp_dir();
        let v = write_new(&dir, vec![cookie("a")], Some("ins_1".into())).unwrap();
        assert!(!dir.join(".session-vault.json.tmp").exists(), "tmp 已 rename 消费");
        let back = read_current(&dir).unwrap();
        assert_eq!(back.generation, v.generation);
        assert_eq!(back.cookies.len(), 1);
    }

    #[test]
    fn write_new_generations_monotonic_and_archives_old() {
        let dir = tmp_dir();
        let v1 = write_new(&dir, vec![cookie("a")], Some("ins_1".into())).unwrap();
        assert_eq!(v1.generation, 1);
        let v2 = write_new(&dir, vec![cookie("b")], Some("ins_2".into())).unwrap();
        assert_eq!(v2.generation, 2);

        let current = read_current(&dir).unwrap();
        assert_eq!(current.generation, 2);
        assert_eq!(current.cookies[0].name, "b");

        let hist = list_history(&dir);
        assert_eq!(hist.len(), 1);
        assert_eq!(hist[0].generation, 1);
        assert_eq!(hist[0].cookies[0].name, "a");
    }

    #[test]
    fn prune_keeps_last_five_generations() {
        let dir = tmp_dir();
        for i in 0..8 {
            write_new(&dir, vec![cookie(&format!("c{i}"))], None).unwrap();
        }
        // 当前是第 8 代；history 应只剩 3/4/5/6/7 代
        let hist = list_history(&dir);
        assert_eq!(hist.len(), MAX_GENERATIONS);
        assert_eq!(hist[0].generation, 7);
        assert_eq!(hist[hist.len() - 1].generation, 3);
    }

    #[test]
    fn restore_makes_old_generation_current() {
        let dir = tmp_dir();
        write_new(&dir, vec![cookie("old")], None).unwrap(); // gen 1
        write_new(&dir, vec![cookie("new")], None).unwrap(); // gen 2

        let restored = restore(&dir, 1).unwrap();
        assert_eq!(restored.cookies[0].name, "old");
        // generation 前进为 3（当前曾是 2）
        assert_eq!(restored.generation, 3);
        assert_eq!(read_current(&dir).unwrap().cookies[0].name, "old");

        // 404：不存在的代
        let err = restore(&dir, 99).unwrap_err();
        assert_eq!(err.code(), "SESSION_GENERATION_NOT_FOUND");
    }

    #[test]
    fn credential_files_get_0600_mode() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tmp_dir();
        write_new(&dir, vec![cookie("a")], None).unwrap();
        let mode = std::fs::metadata(dir.join("session-vault.json"))
            .unwrap()
            .permissions()
            .mode();
        assert_eq!(mode & 0o777, 0o600);
    }

    /// CookieParam 转换协议契约：剔除 session 字段、会话 cookie 省略 expires、
    /// Unspecified/空 sameSite 剔除（实测 -32602 根因）
    #[test]
    fn to_set_cookie_params_matches_cdp_contract() {
        let mut session_cookie = cookie("sess");
        session_cookie.session = true; // expires=-1 默认
        let mut persistent = cookie("perm");
        persistent.expires = 1893456000.0;
        persistent.same_site = Some("Lax".into());
        let mut unspecified = cookie("uns");
        unspecified.same_site = Some("Unspecified".into());
        persistent.http_only = true;

        let params = to_set_cookie_params(&[session_cookie, persistent, unspecified]);
        // 会话 cookie：无 expires、无 session 字段
        assert!(params[0].get("expires").is_none());
        assert!(params[0].get("session").is_none());
        // 持久 cookie：expires 保留、sameSite 保留
        assert_eq!(params[1]["expires"], 1893456000.0);
        assert_eq!(params[1]["sameSite"], "Lax");
        assert!(params[1]["httpOnly"].as_bool().unwrap());
        // Unspecified sameSite 剔除
        assert!(params[2].get("sameSite").is_none());
        // 全部条目都无 session 字段
        for p in &params {
            assert!(p.get("session").is_none());
        }
    }

    #[test]
    fn instance_export_roundtrip() {
        let dir = tmp_dir();
        let path = dir.join("chrome-host-cookies.json");
        write_instance_export(&path, "ins_1", &[cookie("a")]).unwrap();
        let export = read_instance_export(&path).unwrap();
        assert_eq!(export.instance_id, "ins_1");
        assert_eq!(export.cookies.len(), 1);
        assert!(read_instance_export(&dir.join("missing.json")).is_none());
    }
}

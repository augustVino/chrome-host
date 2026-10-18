use std::path::{Path, PathBuf};

use walkdir::WalkDir;

use crate::error::AppError;

/// 选择性复制引擎。
/// 规则即常量：实测迭代只改这里的数组，不动逻辑。

/// 白名单——文件（相对母本目录的精确路径）
const WHITELIST_FILES: &[&str] = &[
    "Local State",
    "Default/Preferences",
    "Default/Cookies",
    "Default/Network/Cookies",
    "Default/Login Data",
];

/// 白名单——目录（整目录递归，内含文件仍逐个过黑名单）
const WHITELIST_DIRS: &[&str] = &[
    "Default/Local Storage",
    "Default/Session Storage",
    "Default/IndexedDB",
];

/// 黑名单——按路径组件名匹配（落在白名单父目录下也跳过）
const BLACKLIST_NAMES: &[&str] = &[
    "Cache",
    "Code Cache",
    "GPUCache",
    "GrShaderCache",
    "ShaderCache",
    "Service Worker",
    "Crashpad",
    "component_crx_cache",
];

/// 黑名单——按文件名前缀/后缀匹配（Chrome 运行锁与残留锁）
const BLACKLIST_PREFIXES: &[&str] = &["Singleton"];
const BLACKLIST_SUFFIXES: &[&str] = &[".lock"];

/// 遍历母本目录，产出白名单内的文件清单（相对路径，仅普通文件）。
/// 返回相对路径而非绝对路径：execute 需要相对结构来还原目标目录布局。
pub fn build(src: &Path) -> Vec<PathBuf> {
    let mut plan: Vec<PathBuf> = Vec::new();

    let push_file = |plan: &mut Vec<PathBuf>, rel: PathBuf| {
        let abs = src.join(&rel);
        if abs.is_file() && !is_blacklisted(&rel) {
            plan.push(rel);
        }
    };

    for entry in WHITELIST_FILES {
        push_file(&mut plan, PathBuf::from(entry));
    }
    for dir in WHITELIST_DIRS {
        let dir_abs = src.join(dir);
        if !dir_abs.is_dir() {
            continue;
        }
        for entry in WalkDir::new(&dir_abs) {
            let Ok(entry) = entry else { continue };
            if !entry.file_type().is_file() {
                continue;
            }
            let Ok(rel) = entry.path().strip_prefix(src) else { continue };
            push_file(&mut plan, rel.to_path_buf());
        }
    }
    plan
}

/// 执行复制：按清单从 src 复制到 dest（自动建父目录），返回复制总字节数（体积观测）。
pub fn execute(plan: &[PathBuf], src: &Path, dest: &Path) -> Result<u64, AppError> {
    let mut total: u64 = 0;
    for rel in plan {
        let from = src.join(rel);
        let to = dest.join(rel);
        if let Some(parent) = to.parent() {
            std::fs::create_dir_all(parent)?;
        }
        total += std::fs::copy(&from, &to)?;
    }
    Ok(total)
}

/// 黑名单判定：相对路径的任一组件命中名称黑名单，或文件名命中前缀/后缀黑名单。
fn is_blacklisted(rel: &Path) -> bool {
    for component in rel.components() {
        let name = component.as_os_str().to_string_lossy();
        if BLACKLIST_NAMES.iter().any(|b| name.eq_ignore_ascii_case(b))
            || BLACKLIST_PREFIXES.iter().any(|p| name.starts_with(p))
            || BLACKLIST_SUFFIXES.iter().any(|s| name.ends_with(s))
        {
            return true;
        }
    }
    false
}

/// consolidate 原子换名区间（rename dir→old / rename tmp→dir / remove old）的
/// 崩溃残留处置（P1）：
/// · old 存在且 dir 缺失 = 崩在两次 rename 之间——old 是唯一完整母本，恢复回位；
/// · old 存在且 dir 存在 = 固化已成功只是清理未完——old 为残留，删除；
/// · tmp 残留两形态下都无价值，顺手清除。
/// 返回是否执行了恢复（true = 母本从 old 回位）
pub fn resolve_stale_swap(target: &Path) -> std::io::Result<bool> {
    let name = target.file_name().expect("target 有文件名").to_string_lossy().to_string();
    let Some(parent) = target.parent() else { return Ok(false) };
    let old = parent.join(format!(".{name}.old"));
    let tmp = parent.join(format!(".{name}.capturing"));
    let _ = std::fs::remove_dir_all(&tmp);
    if !old.exists() {
        return Ok(false);
    }
    if target.exists() {
        std::fs::remove_dir_all(&old)?;
        Ok(false)
    } else {
        std::fs::rename(&old, target)?;
        Ok(true)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scratch() -> PathBuf {
        let dir = std::env::temp_dir().join(format!("cem-copy-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn put(root: &Path, rel: &str, content: &[u8]) {
        let path = root.join(rel);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, content).unwrap();
    }

    /// 白名单命中 + 黑名单剔除
    #[test]
    fn whitelist_hits_and_blacklist_skips() {
        let src = scratch();
        put(&src, "Local State", b"ls");
        put(&src, "Default/Preferences", b"pref");
        put(&src, "Default/Cookies", b"cookies");
        put(&src, "Default/Network/Cookies", b"network-cookies");
        put(&src, "Default/Login Data", b"logindata");
        put(&src, "Default/Local Storage/leveldb/000003.log", b"lsdb");
        put(&src, "Default/IndexedDB/http_a.indexeddb.leveldb/000001.log", b"idb");
        // 黑名单：白名单父目录内的 Cache 组件
        put(&src, "Default/Local Storage/Cache/junk", b"junk");
        // 黑名单：非白名单区域的可再生内容（本就不该进清单）
        put(&src, "Default/Cache/big", b"bigcache");
        put(&src, "Default/Code Cache/x", b"x");
        put(&src, "Crashpad/metadata", b"m");
        // 锁文件：根部的 Singleton（白名单外，验证不误入）；白名单内的 Singleton* 与 *.lock（黑名单剔除）
        put(&src, "SingletonLock", b"lock");
        put(&src, "Default/Local Storage/SingletonCookie", b"lock");
        put(&src, "Default/Local Storage/leveldb/LOCK", b""); // leveldb 自建锁，无 .lock 后缀，允许复制（无害）
        put(&src, "Default/Session Storage/session.lock", b"lock");
        // 非白名单文件
        put(&src, "Default/History", b"history");

        let plan = build(&src);
        let rels: Vec<String> = plan.iter().map(|p| p.to_string_lossy().to_string()).collect();
        assert!(rels.contains(&"Local State".to_string()));
        assert!(rels.contains(&"Default/Preferences".to_string()));
        assert!(rels.contains(&"Default/Cookies".to_string()));
        assert!(rels.contains(&"Default/Network/Cookies".to_string()));
        assert!(rels.contains(&"Default/Login Data".to_string()));
        assert!(rels.iter().any(|r| r.starts_with("Default/Local Storage/leveldb/")));
        assert!(rels.iter().any(|r| r.starts_with("Default/IndexedDB/")));

        assert!(!rels.iter().any(|r| r.contains("Cache/")), "Cache 组件必须剔除: {rels:?}");
        assert!(!rels.iter().any(|r| r.contains("Code Cache")));
        assert!(!rels.iter().any(|r| r.contains("Crashpad")));
        assert!(!rels.iter().any(|r| r.contains("SingletonCookie")), "白名单内的 Singleton* 也必须剔除");
        assert!(!rels.iter().any(|r| r.ends_with(".lock")), "*.lock 必须剔除");
        assert!(rels.iter().any(|r| r.ends_with("leveldb/LOCK")), "leveldb LOCK 非黑名单语义，应保留");
        assert!(!rels.iter().any(|r| r.contains("History")), "非白名单不进清单");

        // execute：布局还原 + 字节数汇总
        let dest = scratch();
        let bytes = execute(&plan, &src, &dest).unwrap();
        let expect: u64 = [2u64, 4, 7, 15, 9, 4, 3].iter().sum();
        assert_eq!(bytes, expect);
        assert_eq!(std::fs::read(dest.join("Default/Network/Cookies")).unwrap(), b"network-cookies");
        assert!(!dest.join("Default/History").exists());
        assert!(!dest.join("SingletonLock").exists());
        assert!(!dest.join("Default/Session Storage/session.lock").exists());
    }

    /// 空目录：空清单 + execute 为 0 字节无副作用
    #[test]
    fn empty_dir_yields_empty_plan() {
        let src = scratch();
        let plan = build(&src);
        assert!(plan.is_empty());
        let dest = scratch();
        assert_eq!(execute(&plan, &src, &dest).unwrap(), 0);
    }

    /// 大文件完整性：复制字节数 = 源文件长度（体积统计的准确性）
    #[test]
    fn large_file_copies_fully() {
        let src = scratch();
        let payload: Vec<u8> = (0..3_000_000u32).map(|i| (i % 251) as u8).collect();
        put(&src, "Default/IndexedDB/http_big.indexeddb.leveldb/blob", &payload);

        let plan = build(&src);
        assert_eq!(plan.len(), 1);
        let dest = scratch();
        let bytes = execute(&plan, &src, &dest).unwrap();
        assert_eq!(bytes, payload.len() as u64);
        assert_eq!(std::fs::read(dest.join(&plan[0])).unwrap(), payload);
    }

    /// 换名区间崩溃残留：dir 缺失时 old（唯一完整母本）恢复回位；
    /// dir 存在时 old 是固化成功后的残留，清除；tmp 恒清除
    #[test]
    fn resolve_stale_swap_recovers_or_cleans() {
        let dir = std::env::temp_dir().join(format!("cem-copier-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let target = dir.join("profile");
        let old = dir.join(".profile.old");
        let tmp = dir.join(".profile.capturing");

        // 形态 1：崩在两次 rename 之间（dir 缺失，old 完整，tmp 残留）
        std::fs::create_dir_all(&old).unwrap();
        std::fs::write(old.join("data"), b"vault").unwrap();
        std::fs::create_dir_all(&tmp).unwrap();
        assert!(resolve_stale_swap(&target).unwrap(), "唯一母本从 old 恢复");
        assert_eq!(std::fs::read(target.join("data")).unwrap(), b"vault", "内容随恢复完好");
        assert!(!old.exists() && !tmp.exists());

        // 形态 2：固化已成功（dir 存在），old 只是清理未完的残留
        std::fs::create_dir_all(&old).unwrap();
        assert!(!resolve_stale_swap(&target).unwrap(), "有 target 时 old 仅清除");
        assert!(target.exists() && !old.exists());

        // 形态 3：无残留 → no-op
        assert!(!resolve_stale_swap(&target).unwrap());
    }
}

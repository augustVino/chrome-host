/// 运行时路径唯一定义处。
/// 禁止在业务代码中散落拼接路径，一律经由本结构。
#[derive(Debug, Clone)]
pub struct AppPaths {
    /// Tauri app_data_dir
    pub root: std::path::PathBuf,
}

impl AppPaths {
    pub fn new(root: std::path::PathBuf) -> Self {
        AppPaths { root }
    }

    pub fn db_file(&self) -> std::path::PathBuf {
        self.root.join("manager.db")
    }

    /// CfT 内核根目录：<root>/kernel/chrome-for-testing
    pub fn kernel_root(&self) -> std::path::PathBuf {
        self.root.join("kernel").join("chrome-for-testing")
    }

    pub fn environments_root(&self) -> std::path::PathBuf {
        self.root.join("environments")
    }

    pub fn environment_dir(&self, env_id: &str) -> std::path::PathBuf {
        self.environments_root().join(env_id)
    }

    pub fn instances_root(&self, env_id: &str) -> std::path::PathBuf {
        self.environment_dir(env_id).join("instances")
    }

    /// 实例独立 profile：<root>/environments/<env_id>/instances/<ins_id>
    pub fn instance_profile(&self, env_id: &str, ins_id: &str) -> std::path::PathBuf {
        self.instances_root(env_id).join(ins_id)
    }

    /// 登录态母本：<root>/environments/<env_id>/login-profile
    pub fn login_profile_dir(&self, env_id: &str) -> std::path::PathBuf {
        self.environment_dir(env_id).join("login-profile")
    }

    /// 删除实例的物化扩展目录（best-effort，NotFound 容忍）。
    /// 实例删除编排（instance/environment service 两处）共用，与写入点
    /// （extension_service 物化管道）同源拼接，防漂移
    pub fn remove_instance_generated_extensions(&self, ins_id: &str) {
        let dir = generated_extensions_dir(&self.root, ins_id);
        if let Err(e) = std::fs::remove_dir_all(&dir) {
            if e.kind() != std::io::ErrorKind::NotFound {
                tracing::warn!("清理实例物化扩展目录失败（继续）: {} - {e}", dir.display());
            }
        }
    }

    /// 孤儿对账兑底（启动编排调用）：删 generated-extensions/ 下不在存活实例集
    /// 的顶层目录，返回删除数。历史残留（删除编排缺满前遗留）一次清
    pub fn prune_orphan_generated_extensions(&self, live_ins_ids: &[String]) -> usize {
        let root = self.root.join("generated-extensions");
        let Ok(entries) = std::fs::read_dir(&root) else {
            return 0; // 目录不存在 = 无孤儿
        };
        let live: std::collections::HashSet<&str> =
            live_ins_ids.iter().map(String::as_str).collect();
        let mut removed = 0;
        for entry in entries.flatten() {
            let name = entry.file_name().to_string_lossy().to_string();
            if live.contains(name.as_str()) {
                continue;
            }
            match std::fs::remove_dir_all(entry.path()) {
                Ok(()) => {
                    tracing::info!("[extensions] 清理孤儿物化目录: {name}");
                    removed += 1;
                }
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                Err(e) => tracing::warn!("清理孤儿物化目录失败（继续）: {name} - {e}"),
            }
        }
        removed
    }

    /// 实例私有 Cookie 导出（关闭时尽力导出，随实例目录生命周期自动清理）。
    /// 保险库文件布局（session-vault.json / history）定义于 session_vault.rs，不经由本结构
    pub fn instance_cookies(&self, env_id: &str, ins_id: &str) -> std::path::PathBuf {
        self.instance_profile(env_id, ins_id).join("chrome-host-cookies.json")
    }
}

/// 实例物化扩展目录：<data_root>/generated-extensions/<ins_id>。
/// 自由函数（extension_service 持 data_root 而非 AppPaths）；唯一拼接源，
/// AppPaths 的清理/对账方法与物化写入点都经由这里
pub fn generated_extensions_dir(data_root: &std::path::Path, ins_id: &str) -> std::path::PathBuf {
    data_root.join("generated-extensions").join(ins_id)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ins_dir(root: &std::path::Path, ins_id: &str) -> std::path::PathBuf {
        let dir = generated_extensions_dir(root, ins_id);
        std::fs::create_dir_all(dir.join("environment-label")).unwrap();
        std::fs::write(dir.join("environment-label").join("manifest.json"), "{}").unwrap();
        dir
    }

    /// 删除编排：实例物化目录整体移除；目录不存在视为已清理（NotFound 容忍，
    /// 重复删除幂等）
    #[test]
    fn remove_instance_generated_extensions_is_idempotent() {
        let root = std::env::temp_dir().join(format!("cem-paths-{}", uuid::Uuid::new_v4()));
        let paths = AppPaths::new(root.clone());
        let dir = ins_dir(&root, "ins_a");
        assert!(dir.is_dir());
        paths.remove_instance_generated_extensions("ins_a");
        assert!(!dir.exists());
        paths.remove_instance_generated_extensions("ins_a"); // 幂等：不 panic 不告警路径
    }

    /// 启动对账：孤儿目录删除、存活实例目录保留、根目录不存在静默 0
    #[test]
    fn prune_removes_orphans_keeps_live() {
        let root = std::env::temp_dir().join(format!("cem-paths-{}", uuid::Uuid::new_v4()));
        let paths = AppPaths::new(root.clone());
        let live = ins_dir(&root, "ins_live");
        let orphan = ins_dir(&root, "ins_gone");

        let removed = paths.prune_orphan_generated_extensions(&["ins_live".to_string()]);
        assert_eq!(removed, 1);
        assert!(live.exists(), "存活实例目录不误删");
        assert!(!orphan.exists(), "孤儿目录被清");

        let none = std::env::temp_dir().join(format!("cem-paths-{}", uuid::Uuid::new_v4()));
        assert_eq!(AppPaths::new(none).prune_orphan_generated_extensions(&[]), 0, "根目录不存在 → 0");
    }
}

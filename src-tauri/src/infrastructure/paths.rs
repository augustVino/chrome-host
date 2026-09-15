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
}

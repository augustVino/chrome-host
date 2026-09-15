//! 应用设置：轻量 JSON 文件，缺失/损坏时回退默认值。
//! MVP 仅 minimize_to_tray；后续设置项按需追加（不加 UI，可手改文件）。

use serde::Deserialize;

#[derive(Debug, Clone, Deserialize)]
pub struct AppSettings {
    /// 主窗口关闭 = 隐藏到托盘（默认开）。false 时关闭即退出应用。
    #[serde(default = "default_true")]
    pub minimize_to_tray: bool,
}

fn default_true() -> bool {
    true
}

impl Default for AppSettings {
    fn default() -> Self {
        AppSettings { minimize_to_tray: true }
    }
}

/// 从 <app_data_dir>/config.json 读取；文件缺失或字段缺失均回退默认（启动路径，禁 panic）。
pub fn load(root: &std::path::Path) -> AppSettings {
    let default = AppSettings::default();
    let Ok(text) = std::fs::read_to_string(root.join("config.json")) else {
        return default;
    };
    match serde_json::from_str::<AppSettings>(&text) {
        Ok(s) => s,
        Err(e) => {
            tracing::warn!("config.json 解析失败，使用默认设置: {e}");
            default
        }
    }
}

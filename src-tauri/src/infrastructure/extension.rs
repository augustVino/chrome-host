//! 内置扩展的底层机制：资源目录自动发现 + 元数据读取 + 模板物化。
//!
//! 约定（目录是唯一真相源，新增内置扩展无需改代码）：
//! - `resources/extensions/<name>/manifest.json` 存在即注册为 system 扩展（ExtensionService 同步）
//! - `chrome-host.json`（可选）声明加载方式：缺省 static 直引源目录；
//!   materialize 物化烘焙（需实例运行时上下文，由 ExtensionService 按目录名分派物化器）
//!
//! 机制：Chromium `--load-extension=<dir>`（未打包 MV3）。CfT 为非品牌构建，
//! 未受品牌版禁用该 flag 的影响；pinned 版本锁定行为。

use std::path::{Path, PathBuf};

use serde::Deserialize;

use crate::error::AppError;

/// 内置扩展在 resources 下的根目录（tauri.conf bundle.resources 声明整个目录，递归拷贝）
pub const BUNDLED_EXTENSIONS_DIR: &str = "extensions";

/// 自描述元数据文件名（与 manifest.json 同目录；Chrome 忽略未引用文件）
pub const BUNDLE_META_FILE: &str = "chrome-host.json";

const TEMPLATE_FILES: &[&str] = &["manifest.json", "content.js"];

/// 内置扩展的加载方式
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ExtensionLoadMode {
    /// 源目录直接引用（Direct Mode：改代码，新实例即加载最新）
    #[default]
    Static,
    /// 物化烘焙：按实例生成专属副本并注入运行时上下文（如环境标识）
    Materialize,
}

/// chrome-host.json 的解析形态
#[derive(Debug, Default, Deserialize)]
pub struct ExtensionBundleMeta {
    #[serde(default)]
    pub mode: ExtensionLoadMode,
}

/// 读取扩展目录的自描述元数据。文件缺失 = Static；
/// 文件存在但非法 → Err（显式暴露，不静默降级）。
pub fn read_bundle_meta(dir: &Path) -> Result<ExtensionBundleMeta, AppError> {
    let path = dir.join(BUNDLE_META_FILE);
    if !path.is_file() {
        return Ok(ExtensionBundleMeta::default());
    }
    let text = std::fs::read_to_string(&path)
        .map_err(|e| AppError::internal(format!("读取 {} 失败: {e}", path.display())))?;
    serde_json::from_str(&text)
        .map_err(|e| AppError::invalid_request(format!("{} 解析失败: {e}", path.display())))
}

/// 生成实例专属扩展副本：模板内容中的占位符替换为实际标签文本。
/// out_dir = `<app_data>/generated-extensions/<ins_id>/environment-label`
pub fn materialize_environment_label(
    templates_dir: &Path,
    out_dir: &Path,
    label: &str,
    position: &str,
    color: &str,
) -> Result<PathBuf, AppError> {
    std::fs::create_dir_all(out_dir)?;
    for file in TEMPLATE_FILES {
        let src = templates_dir.join(file);
        let content = std::fs::read_to_string(&src).map_err(|e| {
            AppError::internal(format!("读取扩展模板失败 {}: {e}", src.display()))
        })?;
        let content = content
            .replace("ENVIRONMENT_NAME_PLACEHOLDER", label)
            .replace("ENVIRONMENT_LABEL_POSITION_PLACEHOLDER", position)
            .replace("ENVIRONMENT_LABEL_STYLE_PLACEHOLDER", color);
        std::fs::write(out_dir.join(file), content)?;
    }
    Ok(out_dir.to_path_buf())
}

/// manifest.json 最小解析：只取展示元数据；name 缺失视为无效（无有效标识）。
#[derive(Debug, Deserialize)]
pub struct ExtensionManifest {
    pub name: String,
    pub description: Option<String>,
    pub version: Option<String>,
    pub manifest_version: Option<i64>,
}

/// 读取扩展目录的 manifest.json。目录不存在 / 文件不可读 / JSON 解析失败 → Err。
pub fn read_manifest(dir: &Path) -> Result<ExtensionManifest, AppError> {
    if !dir.is_dir() {
        return Err(AppError::invalid_request(format!(
            "扩展目录不存在: {}",
            dir.display()
        )));
    }
    let text = std::fs::read_to_string(dir.join("manifest.json")).map_err(|e| {
        AppError::invalid_request(format!(
            "manifest.json 不可读 ({}): {e}",
            dir.join("manifest.json").display()
        ))
    })?;
    serde_json::from_str(&text)
        .map_err(|e| AppError::invalid_request(format!("manifest.json 解析失败: {e}")))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write_template(dir: &Path) {
        std::fs::create_dir_all(dir).unwrap();
        std::fs::write(
            dir.join("manifest.json"),
            r#"{"name":"Environment Label","manifest_version":3}"#,
        )
        .unwrap();
        std::fs::write(
            dir.join("content.js"),
            "const n = 'ENVIRONMENT_NAME_PLACEHOLDER'; const p = 'ENVIRONMENT_LABEL_POSITION_PLACEHOLDER'; const s = 'ENVIRONMENT_LABEL_STYLE_PLACEHOLDER';",
        )
        .unwrap();
    }

    #[test]
    fn materialize_replaces_placeholders_and_writes_files() {
        let tmp = std::env::temp_dir().join(format!("cem-ext-{}", uuid::Uuid::new_v4()));
        write_template(&tmp);
        let out = tmp.join("out").join("environment-label");

        let dir = materialize_environment_label(&tmp, &out, "qapub · Chrome #1", "top-right", "purple").unwrap();

        assert_eq!(dir, out);
        let js = std::fs::read_to_string(out.join("content.js")).unwrap();
        assert!(js.contains("'qapub · Chrome #1'"), "环境名已注入: {js}");
        assert!(js.contains("'top-right'"), "位置占位已替换: {js}");
        assert!(js.contains("'purple'"), "样式占位已替换: {js}");
        assert!(!js.contains("PLACEHOLDER"), "无残留占位: {js}");
        assert!(out.join("manifest.json").exists());
    }

    /// 元数据读取：缺文件 = Static；合法解析；非法 JSON 显式报错
    #[test]
    fn bundle_meta_defaults_and_parse() {
        let tmp = std::env::temp_dir().join(format!("cem-ext-meta-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&tmp).unwrap();

        assert_eq!(
            read_bundle_meta(&tmp).unwrap().mode,
            ExtensionLoadMode::Static,
            "无 chrome-host.json → Static"
        );

        std::fs::write(tmp.join(BUNDLE_META_FILE), r#"{"mode":"materialize"}"#).unwrap();
        assert_eq!(
            read_bundle_meta(&tmp).unwrap().mode,
            ExtensionLoadMode::Materialize
        );

        std::fs::write(tmp.join(BUNDLE_META_FILE), "{bad json").unwrap();
        assert!(read_bundle_meta(&tmp).is_err(), "非法元数据显式报错");
    }
}

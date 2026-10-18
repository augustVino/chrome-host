//! 内置扩展的底层机制：资源目录自动发现 + 元数据读取 + 统一物化管道。
//!
//! 约定（目录是唯一真相源，新增内置扩展无需改代码）：
//! - `resources/extensions/<name>/manifest.json` 存在即注册为 system 扩展（ExtensionService 同步）
//! - `chrome-host.json`（可选）声明加载方式：缺省 static 直引源目录；
//!   materialize 物化：模板目录原样递归拷贝 + 写入单个 config 文件（哑管道，
//!   配置内容由 ExtensionService 按 consumer 组装）；声明了 configFile 才可经 config API 写入
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

/// 物化时写入运行时配置的缺省文件名（未声明 configFile 的物化扩展——
/// 如 environment-label——配置由服务层组装，不开放 API 写入）
pub const DEFAULT_CONFIG_FILE: &str = "config.js";

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

/// chrome-host.json 的解析形态。configFile 声明了才可通过 config API 写入
/// （未声明 = 配置由服务层内置组装，如 environment-label 读全局设置）
#[derive(Debug, Default, Deserialize)]
pub struct ExtensionBundleMeta {
    #[serde(default)]
    pub mode: ExtensionLoadMode,
    #[serde(default, rename = "configFile")]
    pub config_file: Option<String>,
    /// 配置语法参考链接（可多条）；声明后 GUI 配置对话框/CLI 输出展示外链
    #[serde(default, rename = "configDocsUrl")]
    pub config_docs_url: Option<Vec<String>>,
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

/// 统一物化管道：模板目录原样递归拷贝（含 chrome-host.json，Chrome 忽略未引用
/// 文件）+ 写入单个配置文件。`.js` 写 `self.__CHROME_HOST_CONFIG__ = {...};` 赋值
/// （content script 与 service worker 均可同步读全局变量，无 fetch 异步闪烁；
/// 不进 web_accessible_resources——net-rules 配置含敏感头），其他扩展名写原始 JSON。
/// out_dir = `<app_data>/generated-extensions/<ins_id>/<目录名>`，每次启动覆盖重写。
pub fn materialize_with_config(
    templates_dir: &Path,
    out_dir: &Path,
    config_file: &str,
    config: &serde_json::Value,
) -> Result<PathBuf, AppError> {
    std::fs::create_dir_all(out_dir)?;
    copy_dir_recursive(templates_dir, out_dir)?;
    let payload = if config_file.ends_with(".js") {
        format!("self.__CHROME_HOST_CONFIG__ = {};", config)
    } else {
        config.to_string()
    };
    std::fs::write(out_dir.join(config_file), payload)?;
    Ok(out_dir.to_path_buf())
}

fn copy_dir_recursive(src: &Path, dst: &Path) -> Result<(), AppError> {
    for entry in std::fs::read_dir(src)? {
        let entry = entry?;
        let file_type = entry.file_type()?;
        let target = dst.join(entry.file_name());
        if file_type.is_dir() {
            std::fs::create_dir_all(&target)?;
            copy_dir_recursive(&entry.path(), &target)?;
        } else {
            std::fs::copy(entry.path(), &target)?;
        }
    }
    Ok(())
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
        std::fs::create_dir_all(dir.join("icons")).unwrap();
        std::fs::write(
            dir.join("manifest.json"),
            r#"{"name":"Environment Label","manifest_version":3}"#,
        )
        .unwrap();
        std::fs::write(dir.join("content.js"), "// reads self.__CHROME_HOST_CONFIG__").unwrap();
        std::fs::write(dir.join("chrome-host.json"), r#"{"mode":"materialize"}"#).unwrap();
        std::fs::write(dir.join("icons").join("icon.png"), b"png").unwrap();
    }

    /// 统一管道：模板树原样拷贝 + config.js 为同步赋值语句（同步全局变量，无 fetch）
    #[test]
    fn materialize_copies_tree_and_writes_config() {
        let tmp = std::env::temp_dir().join(format!("cem-ext-{}", uuid::Uuid::new_v4()));
        write_template(&tmp);
        // out 与模板目录不相交（与生产布局一致：app_data vs resources；
        // 若嵌套会递归拷贝自身）
        let out = std::env::temp_dir().join(format!("cem-ext-out-{}", uuid::Uuid::new_v4()));

        let config = serde_json::json!({"text": "qapub · Chrome #1", "color": "purple"});
        let dir = materialize_with_config(&tmp, &out, "config.js", &config).unwrap();

        assert_eq!(dir, out);
        let js = std::fs::read_to_string(out.join("config.js")).unwrap();
        assert!(
            js.starts_with("self.__CHROME_HOST_CONFIG__ = {"),
            "同步赋值语句: {js}"
        );
        assert!(js.contains("qapub · Chrome #1"), "配置内容烘入: {js}");
        assert!(out.join("manifest.json").exists(), "模板文件原样拷贝");
        assert!(out.join("chrome-host.json").exists(), "元数据随目录拷贝（Chrome 忽略）");
        assert!(out.join("icons").join("icon.png").exists(), "子目录递归拷贝");
    }

    /// 非 .js 配置文件写原始 JSON（不包 JS 赋值壳）
    #[test]
    fn non_js_config_file_gets_raw_json() {
        let tmp = std::env::temp_dir().join(format!("cem-ext-raw-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&tmp).unwrap();
        std::fs::write(tmp.join("manifest.json"), r#"{"name":"X","manifest_version":3}"#).unwrap();
        let out = std::env::temp_dir().join(format!("cem-ext-rawout-{}", uuid::Uuid::new_v4()));
        materialize_with_config(&tmp, &out, "rules.json", &serde_json::json!([{"id": 1}])).unwrap();
        let text = std::fs::read_to_string(out.join("rules.json")).unwrap();
        assert!(!text.contains("__CHROME_HOST_CONFIG__"), "原始 JSON: {text}");
        assert_eq!(text, r#"[{"id":1}]"#);
    }

    /// 元数据读取：缺文件 = Static；合法解析（含 configFile）；非法 JSON 显式报错
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
        let meta = read_bundle_meta(&tmp).unwrap();
        assert_eq!(meta.mode, ExtensionLoadMode::Materialize);
        assert!(meta.config_file.is_none(), "configFile 可缺省");

        std::fs::write(
            tmp.join(BUNDLE_META_FILE),
            r#"{"mode":"materialize","configFile":"config.js"}"#,
        )
        .unwrap();
        assert_eq!(read_bundle_meta(&tmp).unwrap().config_file.as_deref(), Some("config.js"));

        std::fs::write(tmp.join(BUNDLE_META_FILE), "{bad json").unwrap();
        assert!(read_bundle_meta(&tmp).is_err(), "非法元数据显式报错");
    }
}

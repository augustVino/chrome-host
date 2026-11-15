fn main() {
    // 编译期内置扩展源清单：枚举源码树 extensions/ 直接子目录（排序稳定），
    // 生成 BUILTIN_EXTENSION_DIRS 常量（infrastructure/extension.rs include）。
    // 用途：sync_system_extensions 只认本清单——tauri 把 resources 拷贝到
    // workspace target 时只增不删，源码树已删除的内置扩展会在 target 残留成
    // 幽灵（devtools-unlock 事件），清单过滤后 target 残留被天然无视。
    // 打包产物不跑 build.rs，但清单已编入二进制，且 bundle 同样从源码树复制，
    // 两者天然一致；新增/删除内置扩展目录需重新编译（rerun-if-changed 已声明）。
    let manifest_dir = std::env::var("CARGO_MANIFEST_DIR").expect("CARGO_MANIFEST_DIR 缺失");
    let ext_root = std::path::Path::new(&manifest_dir).join("extensions");
    let mut dirs: Vec<String> = std::fs::read_dir(&ext_root)
        .expect("读取源码树 extensions/ 失败（打包破损？）")
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.is_dir())
        .filter_map(|p| p.file_name().map(|n| n.to_string_lossy().to_string()))
        .collect();
    dirs.sort();

    let out_dir = std::env::var("OUT_DIR").expect("OUT_DIR 缺失");
    let list = dirs
        .iter()
        .map(|d| format!("{d:?}"))
        .collect::<Vec<_>>()
        .join(", ");
    std::fs::write(
        std::path::Path::new(&out_dir).join("builtin_extensions.rs"),
        format!(
            "/// 编译期生成（build.rs）：内置扩展目录源清单，与 src-tauri/extensions/ 强一致。\n\
             /// 新增/删除内置扩展目录需重新编译。\n\
             pub const BUILTIN_EXTENSION_DIRS: &[&str] = &[{list}];\n"
        ),
    )
    .expect("写出 builtin_extensions.rs 失败");
    println!("cargo:rerun-if-changed=extensions");

    // tauri-build 的 copy_binaries 对已存在的 sidecar 目标会先 remove_file().unwrap()——
    // 部分环境（GitHub macOS runner 实测）对已存在产物执行删除时报 EPERM（Operation
    // not permitted），unwrap 直接 panic 掉整个构建。此处先尽力删除可能的旧目标
    // （忽略一切错误），让 tauri-build 恒走纯创建路径，绕开环境相关删除失败。
    let target_dir = std::path::Path::new(&out_dir)
        .parent()
        .and_then(|p| p.parent())
        .and_then(|p| p.parent())
        .and_then(|p| p.parent())
        .expect("OUT_DIR 层级异常")
        .to_path_buf()
        .to_string_lossy()
        .to_string();
    let triple = std::env::var("TARGET").expect("TARGET 缺失");
    let exe = if triple.contains("windows") { "chrome-host-cli.exe" } else { "chrome-host-cli" };
    for layout in [
        format!("{target_dir}/{triple}/release/{exe}"),
        format!("{target_dir}/{triple}/debug/{exe}"),
        format!("{target_dir}/release/{exe}"),
        format!("{target_dir}/debug/{exe}"),
    ] {
        let _ = std::fs::remove_file(&layout);
    }

    tauri_build::build()
}

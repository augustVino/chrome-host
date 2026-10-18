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

    tauri_build::build()
}

use super::{cdp_send, require_conn};
use crate::application::page_service::frames::iframe_sessions_view;
use crate::application::page_service::refs::{is_ref_target, require_ref};
use crate::application::page_service::state::Watched;
use crate::domain::page_control::{normalize_ref, PageControlError};
use crate::error::AppError;
use serde_json::{json, Value};
/// upload 单文件大小上限（100MB，读取前显式拒绝）。
pub(crate) const MAX_UPLOAD_BYTES: u64 = 100 * 1024 * 1024;

/// 主机文件校验（安全边界，锁外快速失败）：
/// 1. 必须绝对路径（防相对路径歧义）；
/// 2. 拒符号链接：逐组件 symlink_metadata 检查（canonicalize 对比会把
///    macOS /tmp→/private/tmp 这类系统别名也判死，逐组件只拒真软链——
///    意图是拒逃逸，不拒系统默认别名）；
/// 3. 必须存在且是普通文件（目录/不存在分列 422/404）；
/// 4. ≤ 100MB（超出显式报错，不读入内存）。
pub(crate) fn validate_upload_file(
    raw: &str,
) -> Result<(std::path::PathBuf, String, u64), AppError> {
    use std::path::{Component, Path};
    let path_invalid = |msg: String| AppError::business(422, "PAGE_UPLOAD_PATH_INVALID", msg);
    if raw.trim().is_empty() {
        return Err(AppError::invalid_request(
            "filePath required（主机绝对路径）",
        ));
    }
    let p = Path::new(raw);
    if !p.is_absolute() {
        return Err(path_invalid(format!(
            "filePath 必须是主机绝对路径（got \"{raw}\"；相对路径有歧义，拒绝）"
        )));
    }
    // 逐组件拒真软链（存在性在下一步整体检查：不存在 → 404）。错误信息附
    // canonicalize 物理路径——macOS 的 /tmp、/var 本身就是软链（→ /private/...），
    // 调用方按提示改传物理路径即可通过。
    let mut cur = std::path::PathBuf::new();
    for comp in p.components() {
        cur.push(comp.as_os_str());
        if let Component::Normal(_) = comp {
            if let Ok(md) = std::fs::symlink_metadata(&cur) {
                if md.file_type().is_symlink() {
                    let resolved = std::fs::canonicalize(&cur)
                        .map(|c| c.display().to_string())
                        .unwrap_or_else(|_| "?".into());
                    return Err(path_invalid(format!(
                        "filePath 经符号链接（\"{}\" → {resolved}），拒绝（防路径逃逸）；请改传物理路径",
                        cur.display()
                    )));
                }
            }
        }
    }
    let md = std::fs::metadata(p).map_err(|_| {
        AppError::not_found(
            "PAGE_UPLOAD_FILE_NOT_FOUND",
            format!("主机文件不存在：\"{raw}\""),
        )
    })?;
    if !md.is_file() {
        return Err(path_invalid(format!(
            "filePath 指向的不是普通文件（目录/设备节点等）：\"{raw}\""
        )));
    }
    if md.len() > MAX_UPLOAD_BYTES {
        return Err(AppError::business(
            422,
            "PAGE_UPLOAD_FILE_TOO_LARGE",
            format!(
                "文件 {:.1}MB 超出 100.0MB 上限（\"{raw}\"）",
                md.len() as f64 / 1024.0 / 1024.0
            ),
        ));
    }
    let name = p
        .file_name()
        .and_then(|s| s.to_str())
        .unwrap_or("file")
        .to_string();
    Ok((p.to_path_buf(), name, md.len()))
}

/// describeNode 响应 → 是否 input[type=file]（不是则 422）。attributes
/// 是扁平数组
/// ["type","file","name","f",...]；input 无 type 属性 = 默认 text，拒绝。
fn ensure_file_input(desc_resp: &Value, target_desc: &str) -> Result<(), AppError> {
    let err = |detail: String| {
        AppError::business(
            422,
            "PAGE_TARGET_NOT_FILE_INPUT",
            format!("upload 目标 {target_desc} 不是 file input：{detail}"),
        )
    };
    let node = desc_resp
        .get("node")
        .ok_or_else(|| err("describeNode 无 node".into()))?;
    let node_name = node.get("nodeName").and_then(Value::as_str).unwrap_or("");
    if !node_name.eq_ignore_ascii_case("input") {
        return Err(err(format!("是 <{node_name}> 元素")));
    }
    let mut ty: Option<String> = None;
    if let Some(attrs) = node.get("attributes").and_then(Value::as_array) {
        for pair in attrs.chunks(2) {
            if pair.len() == 2
                && pair[0]
                    .as_str()
                    .is_some_and(|k| k.eq_ignore_ascii_case("type"))
            {
                ty = pair[1].as_str().map(str::to_string);
            }
        }
    }
    match ty.as_deref() {
        Some(t) if t.eq_ignore_ascii_case("file") => Ok(()),
        Some(t) => Err(err(format!("是 input[type=\"{t}\"]，需要 type=\"file\""))),
        None => Err(err(
            "input 无 type 属性（默认 text），需要 type=\"file\"".into()
        )),
    }
}

/// upload 工具编排：定位（ref → backendNodeId + 帧会话路由；selector →
/// nodeId，同 fill 的定位链）→ describeNode 校验 file input →
/// DOM.setFileInputFiles（不拷贝文件，CDP 直接引用主机路径）。
/// setFileInputFiles 会触发页面原生 change 事件（React 受控表单同常收到
/// ——真机验证）。
pub(crate) async fn upload_tool(
    watched: &Watched,
    tid: &str,
    sid: &str,
    target: &str,
    path: &std::path::Path,
    file_name: &str,
    size: u64,
) -> Result<Value, AppError> {
    let conn = require_conn(watched)?;
    // 定位：(参数键, 节点 id, 发送会话, 描述)。ref 路径 = RefMap 直查
    //（无 CDP 往返，miss → 404 PAGE_REF_NOT_FOUND）；帧内 ref 经
    // iframe_sessions 路由（无登记 = 同源帧/会话已死 → 主会话，同 kit 语义）。
    // selector 路径：nodeId，同 fill 的定位链。
    let (key, node_id, esid, desc) = if is_ref_target(target) {
        let entry = require_ref(watched, tid, target)?;
        let Some(bni) = entry.backend_node_id else {
            return Err(AppError::business(
                409,
                "PAGE_TARGET_MOVED",
                format!(
                    "ref no longer resolves to a live node (@{} has no backend node) — run \"snap <target>\" again and use fresh @eN refs",
                    normalize_ref(target)
                ),
            ));
        };
        let esid = entry
            .frame_id
            .as_deref()
            .and_then(|fid| iframe_sessions_view(watched, tid).remove(fid))
            .unwrap_or_else(|| sid.to_string());
        (
            "backendNodeId",
            bni,
            esid,
            format!("@{}", normalize_ref(target)),
        )
    } else {
        let doc = cdp_send(&conn, "DOM.getDocument", json!({}), Some(sid)).await?;
        let root = doc
            .pointer("/root/nodeId")
            .and_then(Value::as_i64)
            .unwrap_or(0);
        let q = cdp_send(
            &conn,
            "DOM.querySelector",
            json!({ "nodeId": root, "selector": target }),
            Some(sid),
        )
        .await?;
        let node_id = q.get("nodeId").and_then(Value::as_i64).unwrap_or(0);
        if node_id == 0 {
            return Err(PageControlError::element_not_found(target).into());
        }
        ("nodeId", node_id, sid.to_string(), target.to_string())
    };
    // 校验 file input（不是 → 422，先于任何写入）
    let described = cdp_send(
        &conn,
        "DOM.describeNode",
        json!({ key: node_id }),
        Some(&esid),
    )
    .await?;
    ensure_file_input(&described, &desc)?;
    cdp_send(
        &conn,
        "DOM.setFileInputFiles",
        json!({ "files": [path.display().to_string()], key: node_id }),
        Some(&esid),
    )
    .await?;
    Ok(json!({
        "result": format!("Set file \"{file_name}\" ({size} bytes) on {desc}"),
        "fileName": file_name,
        "bytes": size,
    }))
}

//! page 控制端点（P2-C + W-cap）：`/api/v1/instances/{id}/page/*`。
//!
//! handler 只做参数解包与透传——tab 路由（targetId / 唯一前缀）、错误分类、
//! 对话框游标、超时预算全部在 [`crate::application::page_service::PageService`]
//! （P2-B）。响应体为 service 返回原样（与 console/network/screenshot 等
//! 既有端点一致，无额外包裹层）。
//!
//! `tab` 缺省语义（service 侧 `resolve_target("")`，P2-F F1）：唯一 page
//! target 时自动选中；多 target → 409 `PAGE_TAB_AMBIGUOUS`，message 列出全部
//! targetId 供显式指定（不猜活跃 tab，避免取错页面）。

use axum::extract::{Path, Query, State};
use axum::Json;
use serde::Deserialize;
use serde_json::Value;

use crate::error::AppError;

use super::server::ApiState;

/// POST /api/v1/instances/{id}/page/snap —— AX 树快照 + @eN ref 整表替换。
#[derive(Deserialize)]
pub struct SnapBody {
    /// 目标 tab（targetId 或唯一前缀；缺省 = 唯一页面标签自动选中，多标签 409 列出全部）
    pub tab: Option<String>,
}

/// POST /api/v1/instances/{id}/page/click —— 原生输入点击。
#[derive(Deserialize)]
pub struct ClickBody {
    /// 点击目标：@eN ref（上次 snap 产生）或 CSS 选择器
    pub target: String,
    pub tab: Option<String>,
}

/// POST /api/v1/instances/{id}/page/fill —— 聚焦 + 清空 + 输入 + 回读校验。
#[derive(Deserialize)]
pub struct FillBody {
    /// 填写目标：@eN ref 或 CSS 选择器
    pub target: String,
    /// 填入文本；空串 = 仅清空
    pub value: String,
    pub tab: Option<String>,
}

/// POST /api/v1/instances/{id}/page/press —— 键盘三段序列（rawKeyDown/char/keyUp）。
#[derive(Deserialize)]
pub struct PressBody {
    /// 键名：Enter/Tab/单字符/组合键（Control+A 等）；未知 → 400 PAGE_KEY_UNKNOWN
    pub key: String,
    /// 可选聚焦目标：@eN ref 或 CSS 选择器（先 focus 再按键）
    pub target: Option<String>,
    pub tab: Option<String>,
}

/// POST /api/v1/instances/{id}/page/wait —— 轮询等待元素状态。
#[derive(Deserialize)]
pub struct WaitBody {
    pub selector: String,
    /// visible（缺省）/ hidden / attached / detached
    pub state: Option<String>,
    /// 时长字符串："10s" / "250ms" / "500"（裸数字 = ms）；缺省 10s
    pub timeout: Option<String>,
    pub tab: Option<String>,
}

/// GET /api/v1/instances/{id}/page/dialogs —— 对话框缓冲查询（已被自动 accept）。
#[derive(Deserialize)]
pub struct DialogsQuery {
    pub tab: Option<String>,
    /// 游标：只返回 sequence 严格大于 N 的条目；缺省 0 = 全部
    pub since: Option<u64>,
}

/// POST /api/v1/instances/{id}/page/eval —— 任意 JS 求值（P2-G）。
/// ⚠ 高权限：可执行任意 JS（与退役前 cdp.mjs 同权限），仅在 page 原语
/// 不够用时使用（契约文档已披露）。
#[derive(Deserialize)]
pub struct EvalBody {
    pub expression: String,
    pub tab: Option<String>,
}

/// POST /api/v1/instances/{id}/page/html —— 读 outerHTML（P2-G）。
#[derive(Deserialize)]
pub struct HtmlBody {
    /// CSS 选择器；缺省 = 整文档 documentElement.outerHTML。未命中 → 404
    pub selector: Option<String>,
    pub tab: Option<String>,
}

/// POST /api/v1/instances/{id}/page/clickxy —— 视口坐标点击（P2-G）。
#[derive(Deserialize)]
pub struct ClickxyBody {
    /// 视口 CSS 像素横/纵坐标（无 locate/hit-test，坐标即意图）
    pub x: f64,
    pub y: f64,
    pub tab: Option<String>,
}

/// POST /api/v1/instances/{id}/page/loadall —— 循环点击直到选择器消失（P2-G）。
#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LoadallBody {
    pub selector: String,
    /// 轮询间隔时长字符串："1500ms" / "1.5s" / 裸数字 = ms；缺省 1500ms
    pub interval: Option<String>,
    /// 点击上限；缺省 200（预算 = interval × maxClicks + 30s）
    pub max_clicks: Option<u32>,
    pub tab: Option<String>,
}

/// POST /api/v1/instances/{id}/page/type —— 向当前焦点元素输入文本（P2-G）。
/// 跨域 iframe 输入的唯一路径（eval/fill 的 selector 均不可达跨域 frame）。
#[derive(Deserialize)]
pub struct TypeBody {
    /// 非空文本（空串 → 400）
    pub text: String,
    pub tab: Option<String>,
}

/// POST /api/v1/instances/{id}/page/navigate —— 页内导航（保留历史）。
/// 与实例级 navigate（new+close 组合，丢历史）的差异见 AGENT-API。
#[derive(Deserialize)]
pub struct NavigateBody {
    /// 目标 URL（http/https 白名单，与实例级 tabs/navigate 同源；非法 → 400）
    pub url: String,
    pub tab: Option<String>,
}

/// POST /api/v1/instances/{id}/page/back|forward —— 导航历史回退/前进。
#[derive(Deserialize)]
pub struct NavHistoryBody {
    pub tab: Option<String>,
}

/// POST /api/v1/instances/{id}/page/reload —— 重载当前页。
#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ReloadBody {
    /// true = 跳过缓存（Page.reload ignoreCache）；缺省 false
    pub ignore_cache: Option<bool>,
    /// 目标标签页（targetId 或唯一前缀；缺省 = 唯一页面标签自动选中，多标签 409 列出全部）
    pub tab: Option<String>,
}

/// POST /api/v1/instances/{id}/page/screenshot —— 元素/视口 png 截图（W-cap）。
#[derive(Deserialize)]
pub struct ScreenshotBody {
    /// 截图目标：@eN ref（上次 snap 产生）或 CSS 选择器；缺省 = 当前视口整幅。
    /// 元素路径与 click 同定位链（scrollIntoView + quads），clip 取联合包围盒
    pub target: Option<String>,
    pub tab: Option<String>,
}

/// POST /api/v1/instances/{id}/page/pdf —— 整页 PDF（W-cap）。
/// 蛇形纸张参数（paperWidth/paperHeight 英寸），缺省 Letter 8.5×11。
#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PdfBody {
    pub tab: Option<String>,
    /// 横向打印；缺省 false
    pub landscape: Option<bool>,
    /// 打印背景；缺省 true
    pub print_background: Option<bool>,
    /// 纸宽英寸；缺省 8.5
    pub paper_width: Option<f64>,
    /// 纸高英寸；缺省 11
    pub paper_height: Option<f64>,
}

/// POST /api/v1/instances/{id}/page/emulate 的 viewport 子对象。
#[derive(Deserialize)]
pub struct ViewportBody {
    /// 视口宽（CSS 像素，正整数）
    pub width: i64,
    /// 视口高（CSS 像素，正整数）
    pub height: i64,
}

/// POST /api/v1/instances/{id}/page/emulate —— 设备仿真（W-cap）。
/// 三形态互斥：preset（预设名，未知 → 422 PAGE_EMULATE_UNKNOWN_PRESET）/
/// viewport+可选 userAgent/mobile/touch / off=true 清除。仿真只作用于
/// 指定 tab 的 page session（与实例级无关）；重复 emulate = 覆盖。
#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct EmulateBody {
    pub preset: Option<String>,
    pub viewport: Option<ViewportBody>,
    pub user_agent: Option<String>,
    /// 自定义视口缺省 false（预设形态自带表值）
    pub mobile: Option<bool>,
    pub touch: Option<bool>,
    /// true = 清除全部仿真（viewport / UA / touch）
    #[serde(default)]
    pub off: bool,
    pub tab: Option<String>,
}

pub async fn snap(
    State(state): State<ApiState>,
    Path(id): Path<String>,
    Json(body): Json<SnapBody>,
) -> Result<Json<Value>, AppError> {
    let resp = state
        .page
        .snap(&id, body.tab.as_deref().unwrap_or(""))
        .await?;
    Ok(Json(resp))
}

pub async fn click(
    State(state): State<ApiState>,
    Path(id): Path<String>,
    Json(body): Json<ClickBody>,
) -> Result<Json<Value>, AppError> {
    let resp = state
        .page
        .click(&id, body.tab.as_deref().unwrap_or(""), &body.target)
        .await?;
    Ok(Json(resp))
}

pub async fn fill(
    State(state): State<ApiState>,
    Path(id): Path<String>,
    Json(body): Json<FillBody>,
) -> Result<Json<Value>, AppError> {
    let resp = state
        .page
        .fill(
            &id,
            body.tab.as_deref().unwrap_or(""),
            &body.target,
            &body.value,
        )
        .await?;
    Ok(Json(resp))
}

pub async fn press(
    State(state): State<ApiState>,
    Path(id): Path<String>,
    Json(body): Json<PressBody>,
) -> Result<Json<Value>, AppError> {
    let resp = state
        .page
        .press(
            &id,
            body.tab.as_deref().unwrap_or(""),
            &body.key,
            body.target.as_deref(),
        )
        .await?;
    Ok(Json(resp))
}

pub async fn wait(
    State(state): State<ApiState>,
    Path(id): Path<String>,
    Json(body): Json<WaitBody>,
) -> Result<Json<Value>, AppError> {
    let resp = state
        .page
        .wait(
            &id,
            body.tab.as_deref().unwrap_or(""),
            &body.selector,
            body.state.as_deref(),
            body.timeout.as_deref(),
        )
        .await?;
    Ok(Json(resp))
}

pub async fn dialogs(
    State(state): State<ApiState>,
    Path(id): Path<String>,
    Query(q): Query<DialogsQuery>,
) -> Result<Json<Value>, AppError> {
    let resp = state
        .page
        .dialogs(&id, q.tab.as_deref().unwrap_or(""), q.since)
        .await?;
    Ok(Json(resp))
}

pub async fn eval(
    State(state): State<ApiState>,
    Path(id): Path<String>,
    Json(body): Json<EvalBody>,
) -> Result<Json<Value>, AppError> {
    let resp = state
        .page
        .eval(&id, body.tab.as_deref().unwrap_or(""), &body.expression)
        .await?;
    Ok(Json(resp))
}

/// POST /api/v1/instances/{id}/page/raw —— 任意 CDP 命令透传（D5 逃生舱）。
/// ⚠ 最高权限：可发任意 CDP 命令（含 Input/Emulation 等域），仅排查未覆盖
/// 场景时使用；与退役前 cdp.mjs evalraw 同权限。
#[derive(Deserialize)]
pub struct RawBody {
    /// CDP 方法名（如 "Emulation.setDeviceMetricsOverride"）
    pub method: String,
    /// 命令参数；缺省 = 空对象
    pub params: Option<Value>,
    pub tab: Option<String>,
    /// true = 发 browser 级（Target.getTargets 等）；缺省 false = tab 的 page session
    #[serde(default)]
    pub browser: bool,
}

pub async fn raw(
    State(state): State<ApiState>,
    Path(id): Path<String>,
    Json(body): Json<RawBody>,
) -> Result<Json<Value>, AppError> {
    let resp = state
        .page
        .raw(
            &id,
            body.tab.as_deref().unwrap_or(""),
            &body.method,
            body.params.unwrap_or(serde_json::json!({})),
            body.browser,
        )
        .await?;
    Ok(Json(resp))
}

pub async fn html(
    State(state): State<ApiState>,
    Path(id): Path<String>,
    Json(body): Json<HtmlBody>,
) -> Result<Json<Value>, AppError> {
    let resp = state
        .page
        .html(
            &id,
            body.tab.as_deref().unwrap_or(""),
            body.selector.as_deref(),
        )
        .await?;
    Ok(Json(resp))
}

pub async fn clickxy(
    State(state): State<ApiState>,
    Path(id): Path<String>,
    Json(body): Json<ClickxyBody>,
) -> Result<Json<Value>, AppError> {
    let resp = state
        .page
        .clickxy(&id, body.tab.as_deref().unwrap_or(""), body.x, body.y)
        .await?;
    Ok(Json(resp))
}

pub async fn loadall(
    State(state): State<ApiState>,
    Path(id): Path<String>,
    Json(body): Json<LoadallBody>,
) -> Result<Json<Value>, AppError> {
    let resp = state
        .page
        .loadall(
            &id,
            body.tab.as_deref().unwrap_or(""),
            &body.selector,
            body.interval.as_deref(),
            body.max_clicks,
        )
        .await?;
    Ok(Json(resp))
}

pub async fn r#type(
    State(state): State<ApiState>,
    Path(id): Path<String>,
    Json(body): Json<TypeBody>,
) -> Result<Json<Value>, AppError> {
    let resp = state
        .page
        .r#type(&id, body.tab.as_deref().unwrap_or(""), &body.text)
        .await?;
    Ok(Json(resp))
}

/// POST /api/v1/instances/{id}/page/navigate —— 页内导航（page session 上
/// Page.navigate，保留历史；等待 load 事件，窗口内未见 → loaded:false）。
pub async fn navigate(
    State(state): State<ApiState>,
    Path(id): Path<String>,
    Json(body): Json<NavigateBody>,
) -> Result<Json<Value>, AppError> {
    let resp = state
        .page
        .navigate(&id, body.tab.as_deref().unwrap_or(""), &body.url)
        .await?;
    Ok(Json(resp))
}

pub async fn screenshot(
    State(state): State<ApiState>,
    Path(id): Path<String>,
    Json(body): Json<ScreenshotBody>,
) -> Result<Json<Value>, AppError> {
    let resp = state
        .page
        .screenshot(&id, body.tab.as_deref().unwrap_or(""), body.target.as_deref())
        .await?;
    Ok(Json(resp))
}

/// POST /api/v1/instances/{id}/page/back —— 历史后退（无更早条目 → 409
/// PAGE_NAV_NO_HISTORY）。
pub async fn back(
    State(state): State<ApiState>,
    Path(id): Path<String>,
    Json(body): Json<NavHistoryBody>,
) -> Result<Json<Value>, AppError> {
    let resp = state
        .page
        .back(&id, body.tab.as_deref().unwrap_or(""))
        .await?;
    Ok(Json(resp))
}

/// POST /api/v1/instances/{id}/page/forward —— 历史前进（无更晚条目 → 409
/// PAGE_NAV_NO_HISTORY）。
pub async fn forward(
    State(state): State<ApiState>,
    Path(id): Path<String>,
    Json(body): Json<NavHistoryBody>,
) -> Result<Json<Value>, AppError> {
    let resp = state
        .page
        .forward(&id, body.tab.as_deref().unwrap_or(""))
        .await?;
    Ok(Json(resp))
}

pub async fn pdf(
    State(state): State<ApiState>,
    Path(id): Path<String>,
    Json(body): Json<PdfBody>,
) -> Result<Json<Value>, AppError> {
    let resp = state
        .page
        .pdf(
            &id,
            body.tab.as_deref().unwrap_or(""),
            body.landscape,
            body.print_background,
            body.paper_width,
            body.paper_height,
        )
        .await?;
    Ok(Json(resp))
}

/// POST /api/v1/instances/{id}/page/reload —— 重载（等待语义同 navigate）。
pub async fn reload(
    State(state): State<ApiState>,
    Path(id): Path<String>,
    Json(body): Json<ReloadBody>,
) -> Result<Json<Value>, AppError> {
    let resp = state
        .page
        .reload(
            &id,
            body.tab.as_deref().unwrap_or(""),
            body.ignore_cache.unwrap_or(false),
        )
        .await?;
    Ok(Json(resp))
}

pub async fn emulate(
    State(state): State<ApiState>,
    Path(id): Path<String>,
    Json(body): Json<EmulateBody>,
) -> Result<Json<Value>, AppError> {
    let req = crate::domain::emulate::EmulateRequest {
        preset: body.preset,
        viewport: body
            .viewport
            .map(|v| crate::domain::emulate::Viewport {
                width: v.width,
                height: v.height,
            }),
        user_agent: body.user_agent,
        mobile: body.mobile,
        touch: body.touch,
        off: body.off,
    };
    let resp = state
        .page
        .emulate(&id, body.tab.as_deref().unwrap_or(""), req)
        .await?;
    Ok(Json(resp))
}

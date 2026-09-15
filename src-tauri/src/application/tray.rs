//! 托盘：菜单栏常驻入口 + macOS Popover 宿主。
//!
//! 纪律：托盘不直接操作进程，所有动作映射回服务层；
//! 菜单刷新由服务层事件驱动（instance-*/environment-*/login-profile-changed），500ms 防抖整体重建。

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use tauri::menu::{MenuBuilder, SubmenuBuilder};
use tauri::tray::{MouseButton, MouseButtonState, TrayIconBuilder, TrayIconEvent};
use tauri::{AppHandle, LogicalSize, Manager, PhysicalPosition};
use tauri::Listener;

use crate::application::environment_service::EnvironmentService;
use crate::application::instance_service::InstanceService;
use crate::error::AppError;

pub const TRAY_ID: &str = "main";
pub const POPOVER_LABEL: &str = "popover";

const REBUILD_DEBOUNCE_MS: u64 = 500;

/// 托盘动作所需的只读服务集合（managed state）
pub struct TrayState {
    pub env_service: Arc<EnvironmentService>,
    pub instance_service: Arc<InstanceService>,
}

static REBUILD_SCHEDULED: AtomicBool = AtomicBool::new(false);

/// Popover 上次上报的内容高度（显示前预设窗口尺寸，减少弹出后的跳变）
static LAST_POPOVER_HEIGHT: std::sync::Mutex<f64> = std::sync::Mutex::new(540.0);

/// 启动时构建托盘 + 注册事件桥。幂等保护：仅允许调用一次（main.rs setup 顺序保证）。
pub fn init(app: &AppHandle) -> Result<(), AppError> {
    // 数据就绪后首建菜单（init 在 reconcile_all 之后调用）
    schedule_rebuild(app);

    let tray = TrayIconBuilder::with_id(TRAY_ID)
        .icon(app.default_window_icon().expect("bundle 图标存在").clone())
        .tooltip("Chrome Host")
        .show_menu_on_left_click(false) // macOS 左键弹 Popover，右键出菜单
        .on_menu_event(|app, event| {
            let id = event.id().as_ref().to_string();
            dispatch_menu_action(app, &id);
        })
        .on_tray_icon_event(|tray, event| {
            if let TrayIconEvent::Click {
                button: MouseButton::Left,
                button_state: MouseButtonState::Up,
                ..
            } = event
            {
                if let Err(e) = toggle_popover(tray.app_handle()) {
                    tracing::warn!("托盘点击处理失败: {e}");
                }
            }
        })
        .build(app)
        .map_err(|e| AppError::internal(format!("托盘构建失败: {e}")))?;

    // 事件桥：服务层 emit 的业务事件 → 防抖重建菜单
    let handle = app.clone();
    app.listen_any("instance-created", move |_| schedule_rebuild(&handle));
    let handle = app.clone();
    app.listen_any("instance-started", move |_| schedule_rebuild(&handle));
    let handle = app.clone();
    app.listen_any("instance-stopped", move |_| schedule_rebuild(&handle));
    let handle = app.clone();
    app.listen_any("instance-error", move |_| schedule_rebuild(&handle));
    let handle = app.clone();
    app.listen_any("instance-deleted", move |_| schedule_rebuild(&handle));
    let handle = app.clone();
    app.listen_any("environment-created", move |_| schedule_rebuild(&handle));
    let handle = app.clone();
    app.listen_any("environment-updated", move |_| schedule_rebuild(&handle));
    let handle = app.clone();
    app.listen_any("environment-deleted", move |_| schedule_rebuild(&handle));

    // Popover 高度自适应：前端实测内容高度后上报，Rust 侧执行 set_size
    // （尺寸调整收敛在后端，避免前端权限/时序问题；宽限范围与前端常量一致）
    let handle = app.clone();
    app.listen_any("popover-height", move |event| {
        tracing::debug!("popover-height 事件: {}", event.payload());
        let Ok(payload) = serde_json::from_str::<serde_json::Value>(event.payload()) else {
            return;
        };
        let Some(height) = payload["height"].as_f64() else {
            return;
        };
        let clamped = height.clamp(280.0, 540.0);
        if let Ok(mut last) = LAST_POPOVER_HEIGHT.lock() {
            *last = clamped;
        }
        let Some(pop) = handle.get_webview_window(POPOVER_LABEL) else {
            return;
        };
        let scale = pop.scale_factor().unwrap_or(1.0) as f64;
        let current = pop.outer_size().unwrap_or_default().to_logical::<f64>(scale);
        if (current.height - clamped).abs() < 1.0 {
            return; // 高度未变，跳过重复 set_size
        }
        match pop.set_size(LogicalSize::new(380.0, clamped)) {
            Ok(()) => tracing::info!("popover 高度自适应: {:.0} → {:.0}", current.height, clamped),
            Err(e) => tracing::warn!("popover 高度调整失败: {e}"),
        }
    });

    tracing::info!("托盘已就绪");
    let _ = tray; // TrayIcon 由 app 持有（tray_by_id 可取回）
    Ok(())
}

/// 菜单动作分发（同步上下文）：异步动作 spawn 到 runtime，失败仅留痕不阻塞 UI。
fn dispatch_menu_action(app: &AppHandle, id: &str) {
    let (action, target) = id.split_once(':').unwrap_or((id, ""));
    let st = app.state::<TrayState>();
    match action {
        "new" => {
            let svc = st.instance_service.clone();
            let env_id = target.to_string();
            tauri::async_runtime::spawn(async move {
                match svc.create(&env_id).await {
                    Ok(_) => tracing::info!("托盘新建实例成功: env={env_id}"),
                    Err(e) => tracing::warn!("托盘新建实例失败: {e}"),
                }
            });
        }
        "focus" => {
            let svc = st.instance_service.clone();
            let ins_id = target.to_string();
            tauri::async_runtime::spawn(async move {
                if let Err(e) = svc.focus(&ins_id).await {
                    tracing::warn!("托盘唤出实例失败: {e}");
                }
            });
        }
        "stop" => {
            let svc = st.instance_service.clone();
            let ins_id = target.to_string();
            tauri::async_runtime::spawn(async move {
                if let Err(e) = svc.stop(&ins_id).await {
                    tracing::warn!("托盘停止实例失败: {e}");
                }
            });
        }
        "open" => show_main(app),
        "quit" => {
            tracing::info!("托盘 Quit：应用退出");
            app.exit(0);
        }
        _ => {}
    }
}

/// 主窗口唤出（托盘菜单 Open Manager / Popover 底部按钮的兜底路径）
pub fn show_main(app: &AppHandle) {
    if let Some(w) = app.get_webview_window("main") {
        let _ = w.show();
        let _ = w.unminimize();
        let _ = w.set_focus();
    }
}

/// macOS Popover 开关：定位到托盘图标下方并显示；已可见则隐藏。
fn toggle_popover(app: &AppHandle) -> Result<(), AppError> {
    let Some(pop) = app.get_webview_window(POPOVER_LABEL) else {
        return Err(AppError::internal("popover 窗口不存在"));
    };
    if pop.is_visible().unwrap_or(false) {
        let _ = pop.hide();
        return Ok(());
    }
    // 显示前按上次高度预设，避免"先 540 再收缩"的跳变（内容高度由前端实测上报）
    let last = LAST_POPOVER_HEIGHT.lock().map(|l| *l).unwrap_or(540.0);
    if (last - 540.0).abs() >= 1.0 {
        let _ = pop.set_size(LogicalSize::new(380.0, last));
    }
    // 定位：水平居中于托盘图标、顶部贴图标下缘，左右 clamp 留 8px 边距
    if let Some(tray) = app.tray_by_id(TRAY_ID) {
        if let Ok(Some(rect)) = tray.rect() {
            let size = pop.outer_size().unwrap_or(tauri::PhysicalSize::new(380, 540));
            // tray rect 可能是 Physical 或 Logical，统一换算成物理坐标
            let scale = pop.scale_factor().unwrap_or(1.0) as f64;
            let (rx, ry, rw, rh) = match (rect.position, rect.size) {
                (tauri::Position::Physical(p), tauri::Size::Physical(s)) => {
                    (p.x as f64, p.y as f64, s.width as f64, s.height as f64)
                }
                (tauri::Position::Logical(p), tauri::Size::Logical(s)) => {
                    (p.x * scale, p.y * scale, s.width * scale, s.height * scale)
                }
                _ => return Ok(()), // 混合坐标系不出现在实际平台，保守不定位
            };
            let mut x = rx + (rw - size.width as f64) / 2.0;
            let y = ry + rh + 6.0;
            if let Ok(Some(monitor)) = pop.current_monitor() {
                let mw = monitor.size().width as f64;
                x = x.clamp(8.0, (mw - size.width as f64 - 8.0).max(8.0));
            }
            let _ = pop.set_position(PhysicalPosition::new(x as i32, y as i32));
        }
    }
    let _ = pop.show();
    let _ = pop.set_focus();
    Ok(())
}

/// 防抖调度：事件风暴下 500ms 内只重建一次；重建总是读最新数据，风暴期间的事件不丢失
fn schedule_rebuild(app: &AppHandle) {
    if REBUILD_SCHEDULED.swap(true, Ordering::SeqCst) {
        return;
    }
    let a = app.clone();
    tauri::async_runtime::spawn(async move {
        tokio::time::sleep(Duration::from_millis(REBUILD_DEBOUNCE_MS)).await;
        REBUILD_SCHEDULED.store(false, Ordering::SeqCst);
        if let Err(e) = rebuild_menu(&a).await {
            tracing::warn!("托盘菜单刷新失败: {e}");
        }
    });
}

/// 整体重建菜单（实例量级小，重建成本可忽略，计划 ）。
/// 数据走服务层对账路径（list_summary / list_by_env），与 UI 同源。
pub async fn rebuild_menu(app: &AppHandle) -> Result<(), AppError> {
    let st = app.state::<TrayState>();
    let envs = st.env_service.list_summary().await?;

    let mut menu = MenuBuilder::new(app);
    for env in &envs {
        let instances = st.instance_service.list_by_env(&env.environment.id).await?;
        let submenu = SubmenuBuilder::new(app, &env.environment.name)
            .text(format!("new:{}", env.environment.id), "New Instance")
            .build()
            .map_err(|e| AppError::internal(format!("托盘子菜单构建失败: {e}")))?;
        // 运行中实例：Focus / Stop（显示名沿用 Chrome #N 约定）
        let running: Vec<_> = instances.iter().filter(|i| i.status.is_alive_status()).collect();
        if !running.is_empty() {
            let _ = submenu
                    .append(&tauri::menu::PredefinedMenuItem::separator(app).map_err(|e| AppError::internal(e.to_string()))?);
            for ins in running {
                let n = instances
                    .iter()
                    .position(|i| i.id == ins.id)
                    .map(|p| p + 1)
                    .unwrap_or(0);
                let _ = submenu.append(
                    &tauri::menu::MenuItemBuilder::with_id(
                        format!("focus:{}", ins.id),
                        format!("● Chrome #{n}  ·  Focus"),
                    )
                    .build(app)
                    .map_err(|e| AppError::internal(e.to_string()))?,
                );
                let _ = submenu.append(
                    &tauri::menu::MenuItemBuilder::with_id(
                        format!("stop:{}", ins.id),
                        format!("● Chrome #{n}  ·  Stop"),
                    )
                    .build(app)
                    .map_err(|e| AppError::internal(e.to_string()))?,
                );
            }
        }
        menu = menu.item(&submenu);
    }
    if !envs.is_empty() {
        menu = menu.item(
            &tauri::menu::PredefinedMenuItem::separator(app).map_err(|e| AppError::internal(e.to_string()))?,
        );
    }
    let menu = menu
        .text("open", "Open Manager")
        .text("quit", "Quit")
        .build()
        .map_err(|e| AppError::internal(format!("托盘菜单构建失败: {e}")))?;

    if let Some(tray) = app.tray_by_id(TRAY_ID) {
        tray.set_menu(Some(menu))
            .map_err(|e| AppError::internal(format!("托盘菜单替换失败: {e}")))?;
    }
    Ok(())
}

//! RebelShell 管理端：单端口同时提供 Web 界面与客户端反向连接。

mod api;
mod assets;
mod auth;
mod config;
mod hub;
mod listener;
mod protocol;
mod state;
mod terminal;
mod ws;

use axum::routing::{get, post};
use axum::Router;
use tokio::net::TcpListener;
use tower_http::trace::TraceLayer;
use tracing::{error, info, warn};
use tracing_subscriber::EnvFilter;

use config::Config;
use listener::SniffingListener;
use state::{AppState, Event};

#[tokio::main]
async fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();

    let config = match Config::load(&args) {
        Ok(config) => config,
        Err(err) => {
            eprintln!("[rebelshell] 配置错误: {err}");
            std::process::exit(2);
        }
    };
    init_tracing();

    run(config).await;
}

fn init_tracing() {
    // 默认信息级；用 RUST_LOG 可细调，例如 RUST_LOG=rebelshell_server=debug。
    let filter = EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| EnvFilter::new("info,tower_http=warn"));
    tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_target(false)
        .init();
}

async fn run(config: Config) {
    if config.uses_default_token() {
        warn!(
            "正在使用默认管理令牌 {DEFAULT}，任何人都能登录。请通过 ADMIN_TOKEN 环境变量或 --token 覆盖。",
            DEFAULT = config::DEFAULT_ADMIN_TOKEN
        );
    }

    let state = AppState::new(config);
    info!(listen = %state.config.listen_addr, "RebelShell 管理端启动中");

    // 设备列表有变化（上线/下线/心跳）时统一在这里广播，避免每个连接各自比较。
    spawn_device_watcher(state.clone());

    // 单端口：同一个 listener 既接 Web 请求，也接客户端的反向连接，靠首帧嗅探分流。
    let listener = match TcpListener::bind(state.config.listen_addr).await {
        Ok(listener) => listener,
        Err(err) => {
            error!("无法监听 {}: {err}", state.config.listen_addr);
            return;
        }
    };
    info!(
        "服务已就绪：http://{}（同一端口也接受虚拟机客户端的反向连接）",
        state.config.listen_addr
    );

    let app = build_router(state.clone());
    let sniffing = SniffingListener::new(listener, state.hub.clone());

    tokio::select! {
        result = axum::serve(sniffing, app) => {
            if let Err(err) = result {
                error!("服务异常退出: {err}");
            }
        }
        _ = tokio::signal::ctrl_c() => {
            info!("收到中断信号，正在退出");
        }
    }
}

/// 组装 REST + WebSocket + 内嵌前端资源的完整路由。
fn build_router(state: AppState) -> Router {
    let api = Router::new()
        .route("/login", post(auth::login))
        .route("/health", get(api::health))
        .route("/devices", get(api::devices))
        .route("/command", post(api::command))
        .route("/files/push", post(api::push_file))
        .route("/files/pull", get(api::pull_file))
        .route("/fs/list", get(api::list_dir))
        .route("/fs/mkdir", post(api::make_dir))
        .route("/fs/rename", post(api::rename))
        .route("/fs/delete", post(api::delete))
        .route("/fs/read", get(api::read_text))
        .route("/fs/write", post(api::write_text))
        .with_state(state.clone());

    Router::new()
        .route("/ws", get(ws::ws_handler))
        .route("/ws/terminal", get(terminal::ws_terminal))
        .nest("/api", api)
        .fallback(assets::static_handler)
        .layer(TraceLayer::new_for_http())
        .with_state(state)
}

/// 周期性比较设备快照的「指纹」，变化时广播 [`Event::Devices`]。
///
/// 指纹包含在线状态与最后心跳时间，因此设备列表里「最后心跳」的秒数也会实时刷新。
fn spawn_device_watcher(state: AppState) {
    tokio::spawn(async move {
        let mut interval = tokio::time::interval(state.config.push_interval);
        interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        let mut last_fingerprint: Option<String> = None;

        loop {
            interval.tick().await;
            let devices = state.hub.devices();
            let fingerprint = fingerprint(&devices);
            if last_fingerprint.as_deref() != Some(fingerprint.as_str()) {
                last_fingerprint = Some(fingerprint);
                state.publish(Event::Devices);
            }
        }
    });
}

/// 设备快照指纹：任何对前端可见的字段变化都会体现在这里。
fn fingerprint(devices: &[hub::DeviceInfo]) -> String {
    let mut out = String::with_capacity(devices.len() * 48);
    for device in devices {
        out.push_str(&format!(
            "{}|{}|{}|{}|{}|{};",
            device.uuid,
            device.hostname,
            device.ip,
            device.os,
            device.online,
            // 心跳时间按秒取整，避免毫秒抖动导致每轮都判定为「有变化」。
            device.last_seen_ms / 1000
        ));
    }
    out
}

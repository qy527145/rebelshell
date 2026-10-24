//! RebelShell 管理端：反向 TCP 控制服务 + Web 管理界面。

mod api;
mod assets;
mod auth;
mod config;
mod hub;
mod protocol;
mod state;
mod ws;

use std::time::Duration;

use axum::routing::{get, post};
use axum::Router;
use tokio::net::TcpListener;
use tower_http::trace::TraceLayer;
use tracing::{error, info, warn};
use tracing_subscriber::EnvFilter;

use config::Config;
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
    info!(
        tcp = %state.config.tcp_addr,
        http = %state.config.http_addr,
        "RebelShell 管理端启动中"
    );

    // 设备列表有变化（上线/下线/心跳）时统一在这里广播，避免每个连接各自比较。
    spawn_device_watcher(state.clone());

    let tcp_task = tokio::spawn(accept_clients(state.clone()));
    let http_task = tokio::spawn(serve_http(state.clone()));

    tokio::select! {
        result = tcp_task => {
            if let Err(err) = result {
                error!("控制端口任务异常退出: {err}");
            }
        }
        result = http_task => {
            if let Err(err) = result {
                error!("Web 服务任务异常退出: {err}");
            }
        }
        _ = tokio::signal::ctrl_c() => {
            info!("收到中断信号，正在退出");
        }
    }
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

/// 控制端口：接受客户端主动反向连接。
async fn accept_clients(state: AppState) {
    let listener = match TcpListener::bind(state.config.tcp_addr).await {
        Ok(listener) => listener,
        Err(err) => {
            error!("无法监听控制端口 {}: {err}", state.config.tcp_addr);
            return;
        }
    };
    info!(
        "控制端口已就绪：{}（等待虚拟机主动连入）",
        state.config.tcp_addr
    );

    loop {
        match listener.accept().await {
            Ok((stream, peer)) => {
                // 关闭 Nagle：命令与心跳都是小帧，攒包只会增加延迟。
                let _ = stream.set_nodelay(true);
                let hub = state.hub.clone();
                tokio::spawn(async move {
                    hub::serve_connection(hub, stream, peer).await;
                });
            }
            Err(err) => {
                // 单个 accept 失败（例如瞬时 fd 耗尽）不该终止整个监听循环。
                warn!("accept 失败: {err}");
                tokio::time::sleep(Duration::from_millis(200)).await;
            }
        }
    }
}

/// Web 服务：REST API + WebSocket + 内嵌的前端资源。
async fn serve_http(state: AppState) {
    let api = Router::new()
        .route("/login", post(auth::login))
        .route("/health", get(api::health))
        .route("/devices", get(api::devices))
        .route("/command", post(api::command))
        .route("/files/push", post(api::push_file))
        .route("/files/pull", get(api::pull_file))
        .with_state(state.clone());

    let app = Router::new()
        .route("/ws", get(ws::ws_handler))
        .nest("/api", api)
        .fallback(assets::static_handler)
        .layer(TraceLayer::new_for_http())
        .with_state(state.clone());

    let listener = match TcpListener::bind(state.config.http_addr).await {
        Ok(listener) => listener,
        Err(err) => {
            error!("无法监听 Web 端口 {}: {err}", state.config.http_addr);
            return;
        }
    };
    info!("Web 界面已就绪：http://{}", state.config.http_addr);

    if let Err(err) = axum::serve(listener, app).await {
        error!("Web 服务退出: {err}");
    }
}

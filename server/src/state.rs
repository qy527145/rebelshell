//! 进程内共享状态。

use std::sync::Arc;

use tokio::sync::broadcast;

use crate::config::Config;
use crate::hub::Hub;

/// WebSocket 推送的消息。
#[derive(Debug, Clone)]
pub enum Event {
    /// 设备列表发生变化（上线 / 下线 / 心跳推进）。
    Devices,
    /// 命令回显的增量输出。
    Output(OutputChunk),
    /// 命令执行结束。
    Done(CommandDone),
}

/// 命令输出片段：`stream` 供前端区分正文与系统提示。
#[derive(Debug, Clone)]
pub struct OutputChunk {
    pub device_uuid: String,
    pub command: String,
    pub stream: Stream,
    pub text: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Stream {
    Stdout,
    Stderr,
    /// 服务端自己产生的提示（连接异常、超时等），不是客户端回显。
    System,
}

impl Stream {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Stdout => "stdout",
            Self::Stderr => "stderr",
            Self::System => "system",
        }
    }
}

#[derive(Debug, Clone)]
pub struct CommandDone {
    pub device_uuid: String,
    pub command: String,
    /// 命令的退出状态描述（客户端只回传输出文本，这里放成功/失败与耗时）。
    pub ok: bool,
    pub message: String,
}

/// 应用状态，克隆成本低。
#[derive(Clone)]
pub struct AppState {
    pub config: Arc<Config>,
    pub hub: Hub,
    pub events: broadcast::Sender<Event>,
}

impl AppState {
    pub fn new(config: Config) -> Self {
        // 容量足够容纳设备列表刷新 + 若干条命令输出；慢客户端会被裁剪最旧消息。
        let (events, _) = broadcast::channel(256);
        Self {
            config: Arc::new(config),
            hub: Hub::new(),
            events,
        }
    }

    /// 广播一个事件；没有订阅者时静默丢弃。
    pub fn publish(&self, event: Event) {
        let _ = self.events.send(event);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_config() -> Config {
        Config {
            admin_token: "test-token".to_string(),
            listen_addr: "127.0.0.1:0".parse().unwrap(),
            command_timeout: std::time::Duration::from_secs(1),
            transfer_timeout: std::time::Duration::from_secs(1),
            push_interval: std::time::Duration::from_secs(1),
        }
    }

    #[tokio::test]
    async fn publish_reaches_subscribers() {
        let state = AppState::new(test_config());
        let mut rx = state.events.subscribe();
        state.publish(Event::Devices);
        assert!(matches!(rx.recv().await.unwrap(), Event::Devices));
    }

    #[tokio::test]
    async fn publish_without_subscribers_is_noop() {
        let state = AppState::new(test_config());
        // 不应 panic。
        state.publish(Event::Devices);
    }

    #[test]
    fn stream_labels_are_stable() {
        assert_eq!(Stream::Stdout.as_str(), "stdout");
        assert_eq!(Stream::Stderr.as_str(), "stderr");
        assert_eq!(Stream::System.as_str(), "system");
    }
}

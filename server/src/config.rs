//! 管理端配置：环境变量 + 命令行参数。

use std::net::SocketAddr;
use std::time::Duration;

/// 默认管理令牌。仅用于「开箱即用」的本地试跑，生产必须显式覆盖。
pub const DEFAULT_ADMIN_TOKEN: &str = "rebelshell";

#[derive(Debug, Clone)]
pub struct Config {
    /// 全局管理令牌，Web 端登录与所有接口鉴权共用。
    pub admin_token: String,
    /// 监听客户端反向连接的地址。
    pub tcp_addr: SocketAddr,
    /// Web 服务监听地址。
    pub http_addr: SocketAddr,
    /// 命令 / 文件落盘类请求的等待上限。
    pub command_timeout: Duration,
    /// 文件传输的等待上限（大文件更宽松）。
    pub transfer_timeout: Duration,
    /// 设备列表推送间隔。
    pub push_interval: Duration,
}

impl Config {
    /// 从命令行参数与环境变量装载配置（命令行优先）。
    pub fn load(args: &[String]) -> Result<Self, String> {
        let cli = CliArgs::parse(args)?;

        let bind = cli
            .bind
            .or_else(|| env_string("BIND_ADDR"))
            .unwrap_or_else(|| "0.0.0.0".to_string());

        let tcp_port = match cli.tcp_port {
            Some(port) => port,
            None => env_parse("TCP_PORT")?.unwrap_or(9000),
        };
        let http_port = match cli.http_port {
            Some(port) => port,
            None => env_parse("HTTP_PORT")?.unwrap_or(8080),
        };

        let admin_token = cli
            .admin_token
            .or_else(|| env_string("ADMIN_TOKEN"))
            .unwrap_or_else(|| DEFAULT_ADMIN_TOKEN.to_string());

        Ok(Self {
            admin_token,
            tcp_addr: parse_addr(&bind, tcp_port)?,
            http_addr: parse_addr(&bind, http_port)?,
            command_timeout: Duration::from_secs(env_parse("COMMAND_TIMEOUT_SECS")?.unwrap_or(60)),
            transfer_timeout: Duration::from_secs(
                env_parse("TRANSFER_TIMEOUT_SECS")?.unwrap_or(300),
            ),
            push_interval: Duration::from_secs(env_parse("PUSH_INTERVAL_SECS")?.unwrap_or(3)),
        })
    }

    /// 是否仍在使用默认令牌（启动时据此打警告）。
    pub fn uses_default_token(&self) -> bool {
        self.admin_token == DEFAULT_ADMIN_TOKEN
    }
}

#[derive(Default)]
struct CliArgs {
    admin_token: Option<String>,
    bind: Option<String>,
    tcp_port: Option<u16>,
    http_port: Option<u16>,
}

impl CliArgs {
    fn parse(args: &[String]) -> Result<Self, String> {
        let mut parsed = Self::default();
        let mut iter = args.iter();

        while let Some(arg) = iter.next() {
            let mut value = |name: &str| -> Result<String, String> {
                iter.next()
                    .cloned()
                    .ok_or_else(|| format!("{name} 缺少取值"))
            };
            match arg.as_str() {
                "--token" | "-t" => parsed.admin_token = Some(value("--token")?),
                "--bind" | "-b" => parsed.bind = Some(value("--bind")?),
                "--tcp-port" => {
                    let raw = value("--tcp-port")?;
                    parsed.tcp_port = Some(
                        raw.parse()
                            .map_err(|_| format!("--tcp-port 需要端口号，收到 {raw:?}"))?,
                    );
                }
                "--http-port" => {
                    let raw = value("--http-port")?;
                    parsed.http_port = Some(
                        raw.parse()
                            .map_err(|_| format!("--http-port 需要端口号，收到 {raw:?}"))?,
                    );
                }
                "--help" | "-h" => {
                    print_usage();
                    std::process::exit(0);
                }
                other => return Err(format!("未知参数 {other:?}，使用 --help 查看用法")),
            }
        }
        Ok(parsed)
    }
}

pub fn print_usage() {
    println!(
        "RebelShell 管理端 {version}

用法: rebelshell-server [选项]

选项:
  -t, --token <TOKEN>      全局管理令牌（等价于环境变量 ADMIN_TOKEN）
  -b, --bind <ADDR>        绑定地址，默认 0.0.0.0
      --tcp-port <PORT>    客户端反向连接端口，默认 9000
      --http-port <PORT>   Web 服务端口，默认 8080
  -h, --help               显示本帮助

环境变量: ADMIN_TOKEN / BIND_ADDR / TCP_PORT / HTTP_PORT /
          COMMAND_TIMEOUT_SECS / TRANSFER_TIMEOUT_SECS / PUSH_INTERVAL_SECS / RUST_LOG
",
        version = env!("CARGO_PKG_VERSION"),
    );
}

fn env_string(key: &str) -> Option<String> {
    std::env::var(key).ok().filter(|value| !value.is_empty())
}

fn env_parse<T: std::str::FromStr>(key: &str) -> Result<Option<T>, String> {
    match env_string(key) {
        Some(raw) => raw
            .parse()
            .map(Some)
            .map_err(|_| format!("环境变量 {key} 的值 {raw:?} 无法解析")),
        None => Ok(None),
    }
}

fn parse_addr(bind: &str, port: u16) -> Result<SocketAddr, String> {
    // 允许直接写 host:port，也允许只写 host 并另给端口。
    if let Ok(addr) = format!("{bind}:{port}").parse::<SocketAddr>() {
        return Ok(addr);
    }
    // 主机名（如 localhost）无法直接 parse，退化为解析其 IP。
    use std::net::ToSocketAddrs;
    format!("{bind}:{port}")
        .to_socket_addrs()
        .map_err(|err| format!("无法解析绑定地址 {bind}:{port}: {err}"))?
        .next()
        .ok_or_else(|| format!("绑定地址 {bind}:{port} 没有可用的解析结果"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_ports_and_token() {
        let args: Vec<String> = [
            "--tcp-port",
            "9100",
            "--http-port",
            "9101",
            "--token",
            "s3cret",
        ]
        .iter()
        .map(|s| s.to_string())
        .collect();
        let config = Config::load(&args).unwrap();
        assert_eq!(config.tcp_addr.port(), 9100);
        assert_eq!(config.http_addr.port(), 9101);
        assert_eq!(config.admin_token, "s3cret");
        assert!(!config.uses_default_token());
    }

    #[test]
    fn rejects_unknown_flag() {
        assert!(Config::load(&["--nope".to_string()]).is_err());
    }

    #[test]
    fn flags_default_token_detection() {
        let config = Config {
            admin_token: DEFAULT_ADMIN_TOKEN.to_string(),
            tcp_addr: "0.0.0.0:9000".parse().unwrap(),
            http_addr: "0.0.0.0:8080".parse().unwrap(),
            command_timeout: Duration::from_secs(1),
            transfer_timeout: Duration::from_secs(1),
            push_interval: Duration::from_secs(1),
        };
        assert!(config.uses_default_token());
    }
}

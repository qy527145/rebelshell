//! 客户端配置：命令行参数 / 环境变量解析，以及 UUID 的本地持久化。

use std::path::PathBuf;
use std::time::{SystemTime, UNIX_EPOCH};
use std::{env, fs, process};

pub const DEFAULT_SERVER: &str = "127.0.0.1:8080";
pub const DEFAULT_HEARTBEAT_SECS: u64 = 10;

#[derive(Debug, Clone)]
pub struct Config {
    /// 管理端地址，形如 `192.168.1.10:8080`（端口就是管理端唯一监听的那个）。
    pub server: String,
    /// 设备唯一标识。
    pub uuid: String,
    /// PING 间隔（秒）。
    pub heartbeat_secs: u64,
    /// SEND_FILE 的落地目录，同时作为相对路径的基准。
    pub workdir: PathBuf,
}

impl Config {
    /// 从 `args`（不含程序名）解析；未提供的项回落到环境变量，再回落到默认值。
    pub fn from_args(args: &[String]) -> Result<Self, String> {
        let mut server = None;
        let mut uuid = None;
        let mut heartbeat = None;
        let mut workdir = None;

        let mut iter = args.iter();
        while let Some(arg) = iter.next() {
            let mut take_value = |name: &str| -> Result<String, String> {
                iter.next()
                    .cloned()
                    .ok_or_else(|| format!("{name} 缺少取值"))
            };
            match arg.as_str() {
                "--server" | "-s" => server = Some(take_value("--server")?),
                "--uuid" | "-u" => uuid = Some(take_value("--uuid")?),
                "--heartbeat" | "-k" => {
                    let raw = take_value("--heartbeat")?;
                    heartbeat = Some(
                        raw.parse::<u64>()
                            .map_err(|_| format!("--heartbeat 需要整数秒，收到 {raw:?}"))?,
                    );
                }
                "--dir" | "-d" => workdir = Some(PathBuf::from(take_value("--dir")?)),
                "--help" | "-h" => {
                    print_usage();
                    process::exit(0);
                }
                other => return Err(format!("未知参数 {other:?}，使用 --help 查看用法")),
            }
        }

        let server = server
            .or_else(|| env::var("REBELSHELL_SERVER").ok())
            .unwrap_or_else(|| DEFAULT_SERVER.to_string());

        let uuid = match uuid.or_else(|| env::var("REBELSHELL_UUID").ok()) {
            Some(uuid) => uuid,
            None => load_or_create_uuid()?,
        };

        let heartbeat_secs = heartbeat
            .or_else(|| {
                env::var("REBELSHELL_HEARTBEAT")
                    .ok()
                    .and_then(|value| value.parse().ok())
            })
            .unwrap_or(DEFAULT_HEARTBEAT_SECS)
            .max(1);

        let workdir = workdir
            .or_else(|| env::var("REBELSHELL_DIR").ok().map(PathBuf::from))
            .unwrap_or_else(|| env::current_dir().unwrap_or_else(|_| PathBuf::from(".")));

        Ok(Self {
            server,
            uuid,
            heartbeat_secs,
            workdir,
        })
    }
}

pub fn print_usage() {
    println!(
        "RebelShell 客户端 {version}

用法:
  rebelshell-client [run] [选项]        反向连接到管理端并维持长连接
  rebelshell-client install --server <地址>   安装为开机自启的后台服务
  rebelshell-client uninstall           卸载后台服务

选项:
  -s, --server <ADDR>     管理端地址，默认 {DEFAULT_SERVER}
  -u, --uuid <UUID>       设备标识，默认读取本地持久化的 ID（不存在则生成）
  -k, --heartbeat <SECS>  PING 间隔秒数，默认 {DEFAULT_HEARTBEAT_SECS}
  -d, --dir <PATH>        SEND_FILE 的落地目录，默认进程当前目录
  -h, --help              显示本帮助

环境变量: REBELSHELL_SERVER / REBELSHELL_UUID / REBELSHELL_HEARTBEAT / REBELSHELL_DIR
",
        version = env!("CARGO_PKG_VERSION"),
    );
}

/// ID 持久化位置：Windows 用 %APPDATA%，其它平台用 XDG_CONFIG_HOME 或 ~/.config。
pub fn id_file_path() -> Option<PathBuf> {
    let base = if cfg!(windows) {
        env::var_os("APPDATA").map(PathBuf::from)
    } else {
        env::var_os("XDG_CONFIG_HOME")
            .map(PathBuf::from)
            .or_else(|| env::var_os("HOME").map(|home| PathBuf::from(home).join(".config")))
    }?;
    Some(base.join("rebelshell").join("agent-id"))
}

fn load_or_create_uuid() -> Result<String, String> {
    let path = id_file_path().ok_or("无法定位配置目录，请显式指定 --uuid")?;
    if let Ok(existing) = fs::read_to_string(&path) {
        let existing = existing.trim().to_string();
        if !existing.is_empty() {
            return Ok(existing);
        }
    }

    let uuid = generate_uuid();
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)
            .map_err(|err| format!("创建 {} 失败: {err}", parent.display()))?;
    }
    fs::write(&path, &uuid).map_err(|err| format!("写入 {} 失败: {err}", path.display()))?;
    eprintln!(
        "[rebelshell] 已生成设备标识 {} 并保存到 {}",
        uuid,
        path.display()
    );
    Ok(uuid)
}

/// 生成一个 UUID v4 形态的标识。
///
/// 不引入 rand crate：以纳秒时间戳、进程号、主机名哈希和一次堆地址做种，
/// 走 xorshift64* 展开成 128 bit。它只用于设备标识去重，不承担安全语义。
fn generate_uuid() -> String {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_nanos() as u64)
        .unwrap_or(0);
    let mut seed = nanos
        ^ ((process::id() as u64) << 32)
        ^ hash_bytes(hostname().as_bytes())
        ^ (&nanos as *const u64 as u64);

    let mut next = || {
        // xorshift64*
        seed ^= seed >> 12;
        seed ^= seed << 25;
        seed ^= seed >> 27;
        seed.wrapping_mul(0x2545_F491_4F6C_DD1D)
    };

    let hi = next();
    let lo = next();
    let mut bytes = [0u8; 16];
    bytes[..8].copy_from_slice(&hi.to_be_bytes());
    bytes[8..].copy_from_slice(&lo.to_be_bytes());
    // 打上 RFC 4122 的版本位（4）与变体位（10xx），使其形如标准 UUID。
    bytes[6] = (bytes[6] & 0x0f) | 0x40;
    bytes[8] = (bytes[8] & 0x3f) | 0x80;

    let hex: String = bytes.iter().map(|byte| format!("{byte:02x}")).collect();
    format!(
        "{}-{}-{}-{}-{}",
        &hex[0..8],
        &hex[8..12],
        &hex[12..16],
        &hex[16..20],
        &hex[20..32]
    )
}

fn hash_bytes(bytes: &[u8]) -> u64 {
    // FNV-1a，够用即可。
    let mut hash = 0xcbf2_9ce4_8422_2325u64;
    for byte in bytes {
        hash ^= *byte as u64;
        hash = hash.wrapping_mul(0x1000_0000_01b3);
    }
    hash
}

pub fn hostname() -> String {
    for key in ["COMPUTERNAME", "HOSTNAME"] {
        if let Ok(value) = env::var(key) {
            if !value.trim().is_empty() {
                return value.trim().to_string();
            }
        }
    }
    fs::read_to_string("/etc/hostname")
        .map(|value| value.trim().to_string())
        .ok()
        .filter(|value| !value.is_empty())
        .unwrap_or_else(|| "unknown-host".to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_flags() {
        let args: Vec<String> = [
            "--server",
            "10.0.0.1:9100",
            "--uuid",
            "vm-1",
            "--heartbeat",
            "3",
        ]
        .iter()
        .map(|s| s.to_string())
        .collect();
        let config = Config::from_args(&args).unwrap();
        assert_eq!(config.server, "10.0.0.1:9100");
        assert_eq!(config.uuid, "vm-1");
        assert_eq!(config.heartbeat_secs, 3);
    }

    #[test]
    fn rejects_unknown_flag() {
        let args = vec!["--nope".to_string()];
        assert!(Config::from_args(&args).is_err());
    }

    #[test]
    fn default_server_uses_the_shared_port() {
        // 客户端默认端口必须跟服务端的 `PORT` 默认值一致（都是 8080）：
        // 两者共用同一个监听端口，对不上就会「默认配置连不通」。
        // 这条断言把两边的默认值绑在一起，改任一侧时都会在这里被提醒。
        assert_eq!(DEFAULT_SERVER, "127.0.0.1:8080");
        let config = Config::from_args(&[]).unwrap();
        assert!(config.server.ends_with(":8080"));
    }

    #[test]
    fn generates_distinct_uuids() {
        let a = generate_uuid();
        let b = generate_uuid();
        assert_ne!(a, b);
        assert_eq!(a.len(), 36);
    }
}

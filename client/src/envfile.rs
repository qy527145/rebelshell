//! 启动时加载 `.env`，让任何启动方式读到的都是同一份本机配置。
//!
//! 此前只有 bun 脚本会用 `--env-file` 把 `.env` 注入子进程；`cargo run` 或直接
//! 执行 release 二进制时，`REBELSHELL_SERVER` 这些变量根本不存在，客户端会静默
//! 回落到默认端口 —— 看起来像「.env 没生效」，实际是从没被读过。
//!
//! 规则：
//! - **进程里已有的变量一律不覆盖**：真实环境变量永远优先，命令行参数再优先于它。
//! - 只加载找到的**第一个** `.env`，不做多文件合并，避免「到底读的是哪份」。
//! - 查找顺序：`REBELSHELL_ENV_FILE` → 当前目录及各级父目录 → 可执行文件所在目录。
//! - 找不到 `.env` 是正常情况（发布到虚拟机上的裸二进制就是如此），静默跳过。
//!
//! 注意：`.env` 落在工作目录里就相当于配置来源之一。若从不可信目录启动，
//! 请用 `REBELSHELL_ENV_FILE` 显式指定，或干脆不放 `.env`。
//!
//! 语法只支持最小子集：`KEY=VALUE`、整行 `#` 注释、可选 `export ` 前缀、
//! 可选成对引号（引号内的转义不做处理）、CRLF 兼容。**不做变量插值**，
//! 也不支持行尾注释 —— 值里出现 `#` 会原样保留。
//!
//! 与 `server/src/envfile.rs` 保持一致：两个 crate 彼此独立（不是 cargo
//! workspace），这段逻辑只有百来行，复制比引入共享 crate 更省事。

use std::collections::HashSet;
use std::{env, fs};
use std::path::PathBuf;

/// 从当前目录向上最多找这么多层：`target/release/` → crate → 仓库根 足够。
const MAX_ANCESTORS: usize = 8;

/// 加载 `.env` 中尚未出现在进程环境里的变量，返回实际补入的个数。
pub fn load() -> usize {
    let Some(path) = locate() else {
        return 0;
    };
    let contents = match fs::read_to_string(&path) {
        Ok(contents) => contents,
        Err(err) => {
            eprintln!("[rebelshell] 读取 {} 失败: {err}；忽略", path.display());
            return 0;
        }
    };

    let mut applied = 0;
    let mut seen = HashSet::new();
    for (key, value) in parse(&contents) {
        // 同一个文件里重复定义时先出现者胜，与大多数 dotenv 实现一致。
        if !seen.insert(key.clone()) {
            continue;
        }
        if env::var_os(&key).is_some() {
            continue;
        }
        env::set_var(&key, value);
        applied += 1;
    }

    if applied > 0 {
        eprintln!(
            "[rebelshell] 已从 {} 补入 {applied} 个环境变量（已存在的变量未被覆盖）",
            path.display()
        );
    }
    applied
}

fn locate() -> Option<PathBuf> {
    // 显式指定优先；指定了却不存在值得告警，否则用户会以为它生效了。
    if let Some(explicit) = env::var_os("REBELSHELL_ENV_FILE") {
        let path = PathBuf::from(explicit);
        if path.is_file() {
            return Some(path);
        }
        eprintln!(
            "[rebelshell] REBELSHELL_ENV_FILE 指向的 {} 不存在，改为自动查找 .env",
            path.display()
        );
    }

    if let Ok(cwd) = env::current_dir() {
        for dir in cwd.ancestors().take(MAX_ANCESTORS) {
            let candidate = dir.join(".env");
            if candidate.is_file() {
                return Some(candidate);
            }
        }
    }

    // 服务式启动（systemd / 计划任务）的工作目录往往不是安装目录，
    // 所以再看一眼二进制旁边。
    if let Ok(exe) = env::current_exe() {
        if let Some(dir) = exe.parent() {
            let candidate = dir.join(".env");
            if candidate.is_file() {
                return Some(candidate);
            }
        }
    }

    None
}

/// 解析 `.env` 内容。无法识别的行直接跳过 —— 与其因为一行写错就拒绝启动，
/// 不如忽略它并在缺配置时按默认值行为。
fn parse(contents: &str) -> Vec<(String, String)> {
    // 有些编辑器会带 BOM，`KEY` 前多一个不可见字符会让变量名失效。
    let contents = contents.strip_prefix('\u{feff}').unwrap_or(contents);
    let mut entries = Vec::new();

    for raw in contents.lines() {
        let line = raw.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let line = line.strip_prefix("export ").map(str::trim_start).unwrap_or(line);
        let Some((key, value)) = line.split_once('=') else {
            continue;
        };
        let key = key.trim();
        if key.is_empty() || !key.chars().all(|ch| ch.is_ascii_alphanumeric() || ch == '_') {
            continue;
        }
        let value = unquote(value.trim());
        // 空值与「未设置」在这些配置项上没有区别，按未设置处理，
        // 免得 `REBELSHELL_SERVER=` 把地址变成空串、连不上还看不出原因。
        if value.is_empty() {
            continue;
        }
        entries.push((key.to_string(), value.to_string()));
    }

    entries
}

fn unquote(value: &str) -> &str {
    for quote in ['"', '\''] {
        if let Some(inner) = value
            .strip_prefix(quote)
            .and_then(|rest| rest.strip_suffix(quote))
        {
            return inner;
        }
    }
    value
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_basic_lines_and_skips_noise() {
        let entries = parse(
            "\u{feff}# 注释\n\nPORT=8288\n export ADMIN_TOKEN='dev' \nBROKEN\nEMPTY=\nRUST_LOG=\"info,tower_http=warn\"\r\n",
        );
        assert_eq!(
            entries,
            vec![
                ("PORT".to_string(), "8288".to_string()),
                ("ADMIN_TOKEN".to_string(), "dev".to_string()),
                ("RUST_LOG".to_string(), "info,tower_http=warn".to_string()),
            ]
        );
    }

    #[test]
    fn keeps_inner_equals_and_hashes() {
        let entries = parse("URL=http://a=b#frag\n");
        assert_eq!(
            entries,
            vec![("URL".to_string(), "http://a=b#frag".to_string())]
        );
    }

    #[test]
    fn rejects_invalid_keys() {
        let entries = parse("=x\nBAD KEY=1\n1OK=2\n");
        assert_eq!(entries, vec![("1OK".to_string(), "2".to_string())]);
    }
}
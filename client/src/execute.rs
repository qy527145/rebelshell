//! 命令执行：按平台选择 shell，捕获 stdout 与 stderr。

use std::process::{Command, Stdio};

/// 执行一条命令并返回合并后的输出文本。
///
/// - Windows：`powershell.exe -NoProfile -Command "<cmd>"`，以支持丰富的运维命令。
/// - 其它平台：`sh -c "<cmd>"`。
///
/// stdout 与 stderr 都会被捕获并拼接返回；进程无法启动时也不返回 Err，
/// 而是把错误信息当作文本回传，让管理员在网页上直接看到原因。
pub fn run(command: &str) -> String {
    let child = match shell_command(command)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
    {
        Ok(child) => child,
        Err(err) => return format!("[无法启动命令] {err}\n"),
    };

    // 输出量可能很大，wait_with_output 会一次性读走两个管道，避免管道写满导致死锁。
    let output = match child.wait_with_output() {
        Ok(output) => output,
        Err(err) => return format!("[等待命令结束失败] {err}\n"),
    };

    let mut text = decode_output(&output.stdout);
    let stderr = decode_output(&output.stderr);
    if !stderr.is_empty() {
        if !text.is_empty() && !text.ends_with('\n') {
            text.push('\n');
        }
        text.push_str(&stderr);
    }

    if text.is_empty() {
        text = format!(
            "[无输出] 退出码 {}\n",
            output
                .status
                .code()
                .map(|code| code.to_string())
                .unwrap_or_else(|| "unknown".to_string())
        );
    }
    text
}

#[cfg(windows)]
fn shell_command(command: &str) -> Command {
    // PowerShell 默认按本地代码页写出 stdout/stderr（中文 Windows 上是 GBK，
    // 例如 `echo 中文` 会输出 d6d0 cec4），而协议约定回传 UTF-8。
    // 因此先把控制台输出编码切到 UTF-8 再执行命令：PowerShell 会用它解码
    // 原生命令的输出、再用同样的编码写出，中文与其它非 ASCII 结果才不会乱码。
    const UTF8_PREAMBLE: &str = concat!(
        "[Console]::OutputEncoding=[System.Text.Encoding]::UTF8; ",
        "$OutputEncoding=[System.Text.Encoding]::UTF8; ",
    );

    // -NoProfile 保证不受目标机器 PowerShell 配置影响；-Command 接收完整命令串。
    let mut cmd = Command::new("powershell.exe");
    cmd.args([
        "-NoProfile",
        "-NonInteractive",
        "-Command",
        &format!("{UTF8_PREAMBLE}{command}"),
    ]);
    cmd
}

#[cfg(not(windows))]
fn shell_command(command: &str) -> Command {
    let mut cmd = Command::new("sh");
    cmd.args(["-c", command]);
    cmd
}

/// 把外部进程输出的字节解码成文本。
///
/// PowerShell（以及它转发的原生命令）与 `schtasks` 等 Windows 自带工具在中文
/// 系统上会按本地代码页写出，而不是 UTF-8；直接 `from_utf8_lossy` 会把中文变成
/// 替换字符。因此先试 UTF-8，失败则回退到本地 ANSI 代码页（CP936/GBK）。
pub fn decode_output(bytes: &[u8]) -> String {
    if let Ok(text) = std::str::from_utf8(bytes) {
        return text.to_string();
    }
    #[cfg(windows)]
    {
        if let Some(text) = decode_cp936(bytes) {
            return text;
        }
    }
    String::from_utf8_lossy(bytes).into_owned()
}

/// 用 CP936（GBK）解码；仅在 Windows 上使用。
///
/// 不引入编码库：GBK 的双字节区可用「前导字节 + 尾随字节」的区间表反查。
/// 这里只覆盖最常用的子集（ASCII + 简体中文常用区），解码不出的字节保留为
/// 替换字符——目标是让中文回显可读，而不是做一个完整的编码转换器。
#[cfg(windows)]
fn decode_cp936(bytes: &[u8]) -> Option<String> {
    // 通过 Windows 自身的 MultiByteToWideChar 转换，避免手写码表出错。
    use std::os::windows::ffi::OsStringExt;

    #[link(name = "kernel32")]
    extern "system" {
        fn MultiByteToWideChar(
            code_page: u32,
            flags: u32,
            multi_byte_str: *const u8,
            multi_byte_count: i32,
            wide_char_str: *mut u16,
            wide_char_count: i32,
        ) -> i32;
    }

    const CP_ACP: u32 = 0; // 系统当前 ANSI 代码页，中文 Windows 上即 936。
    const MB_ERR_INVALID_CHARS: u32 = 0x0000_0008;

    if bytes.is_empty() {
        return Some(String::new());
    }

    // 先试「严格模式」，让它能报出非法序列；失败则退回宽松模式。
    for flags in [MB_ERR_INVALID_CHARS, 0] {
        let needed = unsafe {
            MultiByteToWideChar(
                CP_ACP,
                flags,
                bytes.as_ptr(),
                bytes.len() as i32,
                std::ptr::null_mut(),
                0,
            )
        };
        if needed <= 0 {
            continue;
        }

        let mut wide = vec![0u16; needed as usize];
        let written = unsafe {
            MultiByteToWideChar(
                CP_ACP,
                flags,
                bytes.as_ptr(),
                bytes.len() as i32,
                wide.as_mut_ptr(),
                needed,
            )
        };
        if written <= 0 {
            continue;
        }
        wide.truncate(written as usize);
        let text = std::ffi::OsString::from_wide(&wide);
        // 严格模式能成功，说明整段都是合法的本地编码文本。
        if flags == MB_ERR_INVALID_CHARS {
            return text.to_string_lossy().into_owned().into();
        }
    }
    None
}

/// 系统类型标识，用于 REGISTER 帧。
pub fn os_type() -> &'static str {
    if cfg!(windows) {
        "windows"
    } else if cfg!(target_os = "macos") {
        "macos"
    } else if cfg!(target_os = "linux") {
        "linux"
    } else {
        "unknown"
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn captures_stdout() {
        let out = run(if cfg!(windows) {
            "Write-Output hi"
        } else {
            "echo hi"
        });
        assert!(out.contains("hi"), "unexpected output: {out}");
    }

    #[test]
    fn captures_stderr_and_keeps_going() {
        let cmd = if cfg!(windows) {
            "Write-Error boom; Write-Output after"
        } else {
            "echo boom 1>&2; echo after"
        };
        let out = run(cmd);
        assert!(out.contains("boom"), "unexpected output: {out}");
        assert!(out.contains("after"), "unexpected output: {out}");
    }

    #[test]
    fn captures_non_ascii_output() {
        // 中文 Windows 上 PowerShell 默认按 GBK 写出；这里确认回传的是可读文本，
        // 而不是一串替换字符。
        let out = run("echo 中文测试");
        assert!(out.contains("中文测试"), "输出被破坏: {out:?}");
    }

    #[test]
    fn decodes_utf8_preferentially() {
        assert_eq!(decode_output("中文".as_bytes()), "中文");
        assert_eq!(decode_output(b"plain ascii"), "plain ascii");
    }

    #[cfg(windows)]
    #[test]
    fn decodes_gbk_when_not_utf8() {
        // GBK 的 “中文” 是 d6d0 cec4，不是合法 UTF-8，应回退到本地代码页解码。
        assert_eq!(decode_output(&[0xd6, 0xd0, 0xce, 0xc4]), "中文");
    }
}

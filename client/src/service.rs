//! 把 Agent 安装为开机自启的后台服务。
//!
//! - Windows：计划任务（`schtasks`），开机启动、以最高权限运行、崩溃后重试。
//! - Linux：systemd 单元 `rebelshell-agent.service`，`Restart=always`。
//! - macOS：`~/Library/LaunchAgents` 下的 launchd plist，`KeepAlive`。
//!
//! 安装/卸载通常需要管理员（Windows）或 root（Linux）权限。

use std::env;
#[cfg(not(windows))]
use std::fs;
use std::path::Path;
#[cfg(not(windows))]
use std::path::PathBuf;
use std::process::Command;

use crate::config::Config;

#[cfg_attr(windows, allow(dead_code))] // Windows 分支用计划任务名 TASK_NAME，不需要服务名。
const SERVICE_NAME: &str = "rebelshell-agent";
#[cfg_attr(not(windows), allow(dead_code))] // 计划任务名只在 Windows 分支用到。
const TASK_NAME: &str = "RebelShellAgent";

pub fn install(config: &Config) -> Result<(), String> {
    let exe = env::current_exe().map_err(|err| format!("无法定位当前可执行文件: {err}"))?;
    let args = format!("--server {} --uuid {}", config.server, config.uuid);

    if cfg!(windows) {
        install_windows(&exe, &args)
    } else if cfg!(target_os = "macos") {
        install_macos(&exe, &args)
    } else {
        install_linux(&exe, &args)
    }
}

pub fn uninstall() -> Result<(), String> {
    if cfg!(windows) {
        uninstall_windows()
    } else if cfg!(target_os = "macos") {
        uninstall_macos()
    } else {
        uninstall_linux()
    }
}

#[cfg(windows)]
fn uninstall_windows() -> Result<(), String> {
    // 先查一下任务是否存在，不存在时给一句人话，而不是把 schtasks 的原始报错抛出去。
    let existing = Command::new("schtasks")
        .args(["/Query", "/TN", TASK_NAME])
        .output()
        .map(|output| output.status.success())
        .unwrap_or(false);
    if !existing {
        println!("计划任务 {TASK_NAME} 并不存在，无需卸载");
        return Ok(());
    }

    run(
        "schtasks",
        &["/Delete", "/TN", TASK_NAME, "/F"],
        "删除计划任务",
    )?;
    println!("已卸载计划任务 {TASK_NAME}");
    Ok(())
}

#[cfg(not(windows))]
fn uninstall_windows() -> Result<(), String> {
    unreachable!("uninstall_windows 只在 Windows 上编译")
}

#[cfg(target_os = "linux")]
fn uninstall_linux() -> Result<(), String> {
    // 先停再删，避免单元残留仍在运行。
    let _ = run(
        "systemctl",
        &["disable", "--now", SERVICE_NAME],
        "停止并禁用服务",
    );
    let unit = linux_unit_path();
    if unit.exists() {
        fs::remove_file(&unit).map_err(|err| format!("删除 {} 失败: {err}", unit.display()))?;
    }
    let _ = run("systemctl", &["daemon-reload"], "重载 systemd");
    println!("已卸载 {SERVICE_NAME}");
    Ok(())
}

#[cfg(not(target_os = "linux"))]
fn uninstall_linux() -> Result<(), String> {
    unreachable!("uninstall_linux 只在 Linux 上编译")
}

#[cfg(target_os = "macos")]
fn uninstall_macos() -> Result<(), String> {
    let plist = macos_plist_path()?;
    let _ = run(
        "launchctl",
        &["unload", &plist.to_string_lossy()],
        "卸载 launchd 任务",
    );
    if plist.exists() {
        fs::remove_file(&plist).map_err(|err| format!("删除 {} 失败: {err}", plist.display()))?;
    }
    println!("已卸载 {}", plist.display());
    Ok(())
}

#[cfg(not(target_os = "macos"))]
fn uninstall_macos() -> Result<(), String> {
    unreachable!("uninstall_macos 只在 macOS 上编译")
}

#[cfg(windows)]
fn install_windows(exe: &Path, args: &str) -> Result<(), String> {
    // schtasks 的 /TR 取值里若含空格需要整体加引号，内层再用 \" 包住 exe 路径。
    let command = format!("\"{}\" {}", exe.display(), args);
    run(
        "schtasks",
        &[
            "/Create", "/TN", TASK_NAME, "/TR", &command, "/SC", "ONSTART", "/RU", "SYSTEM", "/RL",
            "HIGHEST", "/F",
        ],
        "创建计划任务",
    )?;
    run("schtasks", &["/Run", "/TN", TASK_NAME], "立即启动计划任务")?;
    println!("已安装计划任务 {TASK_NAME}（开机自启，SYSTEM 权限）");
    Ok(())
}

#[cfg(not(windows))]
fn install_windows(_exe: &Path, _args: &str) -> Result<(), String> {
    unreachable!("install_windows 只在 Windows 上编译")
}

#[cfg(target_os = "linux")]
fn install_linux(exe: &Path, args: &str) -> Result<(), String> {
    let unit_path = linux_unit_path();
    let unit = format!(
        "[Unit]
Description=RebelShell Agent (reverse admin channel)
After=network-online.target
Wants=network-online.target

[Service]
Type=simple
ExecStart={exe} {args}
Restart=always
RestartSec=5
StandardOutput=journal
StandardError=journal

[Install]
WantedBy=multi-user.target
",
        exe = exe.display(),
    );
    fs::write(&unit_path, unit)
        .map_err(|err| format!("写入 {} 失败（需要 root 权限）: {err}", unit_path.display()))?;
    run("systemctl", &["daemon-reload"], "重载 systemd")?;
    run(
        "systemctl",
        &["enable", "--now", SERVICE_NAME],
        "启用并启动服务",
    )?;
    println!("已安装 {SERVICE_NAME}.service，可用 systemctl status {SERVICE_NAME} 查看状态");
    Ok(())
}

#[cfg(not(target_os = "linux"))]
fn install_linux(_exe: &Path, _args: &str) -> Result<(), String> {
    unreachable!("install_linux 只在 Linux 上编译")
}

#[cfg(target_os = "macos")]
fn install_macos(exe: &Path, args: &str) -> Result<(), String> {
    let plist_path = macos_plist_path()?;
    // args 里的每个空格分隔项都要独立成 <string>。
    let mut arg_xml = String::new();
    for arg in args.split_whitespace() {
        arg_xml.push_str(&format!("    <string>{}</string>\n", xml_escape(arg)));
    }
    let plist = format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
  <key>Label</key>
  <string>{SERVICE_NAME}</string>
  <key>ProgramArguments</key>
  <array>
    <string>{exe}</string>
{arg_xml}  </array>
  <key>RunAtLoad</key>
  <true/>
  <key>KeepAlive</key>
  <true/>
</dict>
</plist>
"#,
        exe = xml_escape(&exe.display().to_string()),
    );
    if let Some(parent) = plist_path.parent() {
        fs::create_dir_all(parent)
            .map_err(|err| format!("创建 {} 失败: {err}", parent.display()))?;
    }
    fs::write(&plist_path, plist)
        .map_err(|err| format!("写入 {} 失败: {err}", plist_path.display()))?;
    let _ = run(
        "launchctl",
        &["unload", &plist_path.to_string_lossy()],
        "卸载旧任务",
    );
    run(
        "launchctl",
        &["load", &plist_path.to_string_lossy()],
        "加载 launchd 任务",
    )?;
    println!("已安装 {}", plist_path.display());
    Ok(())
}

#[cfg(not(target_os = "macos"))]
fn install_macos(_exe: &Path, _args: &str) -> Result<(), String> {
    unreachable!("install_macos 只在 macOS 上编译")
}

#[cfg(target_os = "linux")]
fn linux_unit_path() -> PathBuf {
    PathBuf::from(format!("/etc/systemd/system/{SERVICE_NAME}.service"))
}

#[cfg(target_os = "macos")]
fn macos_plist_path() -> Result<PathBuf, String> {
    let home = env::var_os("HOME").ok_or("无法读取 HOME 环境变量")?;
    Ok(PathBuf::from(home)
        .join("Library/LaunchAgents")
        .join(format!("{SERVICE_NAME}.plist")))
}

#[cfg(target_os = "macos")]
fn xml_escape(input: &str) -> String {
    input
        .replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
}

fn run(program: &str, args: &[&str], what: &str) -> Result<(), String> {
    let output = Command::new(program)
        .args(args)
        .output()
        .map_err(|err| format!("{what} 失败（{program} 无法执行）: {err}"))?;
    if output.status.success() {
        return Ok(());
    }
    // schtasks / systemctl 在中文系统上按本地代码页写出，需要走统一解码。
    let stderr = crate::execute::decode_output(&output.stderr);
    let stdout = crate::execute::decode_output(&output.stdout);
    Err(format!(
        "{what} 失败（退出码 {:?}）: {}{}",
        output.status.code(),
        stdout.trim(),
        stderr.trim()
    ))
}

//! 构建脚本：保证 `web/dist` 目录始终存在，且至少有一个 `index.html`。
//!
//! `rust-embed` 要求 `#[folder = "web/dist"]` 在编译期存在，否则直接编译失败。
//! 全新的克隆还没有构建过前端，因此这里补一个占位页面，
//! 让「先编译服务端、后构建前端」的顺序也能成功。

use std::path::Path;
use std::process::Command;

const PLACEHOLDER: &str = r#"<!doctype html>
<html lang="zh-CN">
  <head>
    <meta charset="UTF-8" />
    <title>RebelShell 前端未构建</title>
    <style>
      body { background: #0f1115; color: #e6e9ef; font-family: system-ui, "Microsoft YaHei", sans-serif;
             margin: 0; padding: 48px; line-height: 1.7; }
      code { background: #1d222b; border-radius: 4px; font-family: ui-monospace, Consolas, monospace; padding: 2px 6px; }
      h1 { font-size: 20px; }
      ol { max-width: 640px; }
    </style>
  </head>
  <body>
    <h1>前端资源尚未构建</h1>
    <p>你看到的是占位页面。服务端已内嵌它，因为构建时 <code>server/web/dist</code> 里没有前端产物。</p>
    <ol>
      <li>在 <code>web/</code> 下执行 <code>bun install</code></li>
      <li>执行 <code>bun run build</code>（产物会写入 <code>server/web/dist</code>）</li>
      <li>重新编译并重启服务端，然后刷新本页</li>
    </ol>
    <p>后端 API 不受影响，可访问 <code>/api/health</code> 验证服务是否在运行。</p>
  </body>
</html>
"#;

fn main() {
    let dist = Path::new("web/dist");
    let index = dist.join("index.html");

    if !index.exists() {
        if let Err(err) = std::fs::create_dir_all(dist) {
            // 目录建不出来也不必中断构建：rust-embed 会给出更明确的报错。
            println!("cargo:warning=创建 {} 失败: {err}", dist.display());
        }
        if let Err(err) = std::fs::write(&index, PLACEHOLDER) {
            println!("cargo:warning=写入占位 {} 失败: {err}", index.display());
        }
    }

    // 前端产物变化后需要重新编译，否则内嵌的还是旧页面。
    println!("cargo:rerun-if-changed=web/dist");

    // 把构建信息带进二进制，便于在日志与 /api/health 里核对版本。
    embed_git_revision();
}

fn embed_git_revision() {
    println!("cargo:rerun-if-changed=../.git/HEAD");
    let revision = Command::new("git")
        .args(["rev-parse", "--short", "HEAD"])
        .output()
        .ok()
        .filter(|output| output.status.success())
        .map(|output| String::from_utf8_lossy(&output.stdout).trim().to_string())
        .unwrap_or_else(|| "unknown".to_string());
    println!("cargo:rustc-env=REBELSHELL_GIT_REVISION={revision}");
}

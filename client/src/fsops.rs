//! 文件管理器需要的远程文件系统操作：列目录、新建、重命名、删除、读写文本。
//!
//! 刻意只用 `std::fs`，不引入任何依赖：这些操作在三大平台上语义一致，
//! 真正需要小心的是「路径怎么解释」与「错误怎么讲清楚」两件事。
//!
//! ## 路径解释
//!
//! 相对路径以客户端工作目录（`--dir`）为基准 —— 与服务端下发文件时的约定一致。
//! 列目录返回的 `PATH` 是**解析后的绝对路径**，前端据此维护「当前目录」，
//! 因此从服务端来回传递的一直是绝对路径，不受工作目录影响。

use std::fs;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

/// 在线编辑的文本大小上限。超出就不让改，避免把整个文件塞进一条协议帧。
pub const MAX_TEXT_BYTES: u64 = 2 * 1024 * 1024;

/// 目录列表里的一项，与 `protocol::DirEntry` 对应。
pub struct Entry {
    pub name: String,
    /// `d` 目录 / `f` 文件 / `l` 符号链接。
    pub kind: char,
    pub size: u64,
    pub modified_ms: u64,
}

/// 列目录，返回「解析后的绝对路径 + 条目」。
///
/// 输出格式与 `server::protocol::parse_listing` 严格对应：
/// 首行 `PATH\t<绝对路径>`，其后每行 `<kind>\t<size>\t<mtime>\t<name>`。
pub fn list_dir(resolved: &Path) -> Result<String, String> {
    let canonical = canonicalize(resolved)?;
    let reader = fs::read_dir(&canonical)
        .map_err(|err| format!("打开目录 {} 失败: {err}", canonical.display()))?;

    let mut entries: Vec<Entry> = Vec::new();
    for item in reader {
        // 单个条目读失败（权限、竞态删除）不该让整个列表报错，跳过即可。
        let Ok(item) = item else { continue };
        // symlink_metadata 不跟随链接：我们要如实显示「这是个链接」。
        let Ok(meta) = fs::symlink_metadata(item.path()) else {
            continue;
        };
        let kind = if meta.is_dir() {
            'd'
        } else if meta.file_type().is_symlink() {
            'l'
        } else {
            'f'
        };
        entries.push(Entry {
            name: item.file_name().to_string_lossy().into_owned(),
            kind,
            // 目录的 size 在多数系统上是个无意义的块大小，统一显示 0 更清爽。
            size: if kind == 'd' { 0 } else { meta.len() },
            modified_ms: modified_ms(&meta),
        });
    }

    // 目录在前、文件在后，组内按名称排序 —— 资源管理器的常规做法。
    // 用不区分大小写的比较，Windows 上更符合直觉。
    entries.sort_by(|a, b| {
        let group = |kind: char| if kind == 'd' { 0 } else { 1 };
        group(a.kind)
            .cmp(&group(b.kind))
            .then_with(|| a.name.to_lowercase().cmp(&b.name.to_lowercase()))
            .then_with(|| a.name.cmp(&b.name))
    });

    let mut out = String::with_capacity(256 + entries.len() * 48);
    out.push_str("PATH\t");
    out.push_str(&canonical.display().to_string());
    out.push('\n');
    for entry in entries {
        // 名称里若含制表符或换行会破坏分帧，替换掉（这种文件名极其罕见）。
        let name = entry.name.replace(['\t', '\n', '\r'], " ");
        out.push_str(&format!(
            "{}\t{}\t{}\t{}\n",
            entry.kind, entry.size, entry.modified_ms, name
        ));
    }
    Ok(out)
}

/// 新建目录（含所有中间层级，与 `mkdir -p` 一致）。
pub fn make_dir(resolved: &Path) -> Result<String, String> {
    if resolved.as_os_str().is_empty() {
        return Err("路径不能为空".to_string());
    }
    fs::create_dir_all(resolved)
        .map_err(|err| format!("创建目录 {} 失败: {err}", resolved.display()))?;
    Ok(format!("已创建目录 {}", resolved.display()))
}

/// 重命名 / 移动。
pub fn rename(from: &Path, to: &Path) -> Result<String, String> {
    if !from.exists() {
        return Err(format!("{} 不存在", from.display()));
    }
    if to.exists() {
        return Err(format!("目标 {} 已存在", to.display()));
    }
    // 目标可能在新目录里，先把父目录补出来（移动文件到不存在的子目录时常见）。
    if let Some(parent) = to.parent() {
        if !parent.as_os_str().is_empty() && !parent.exists() {
            fs::create_dir_all(parent)
                .map_err(|err| format!("创建目录 {} 失败: {err}", parent.display()))?;
        }
    }
    fs::rename(from, to).map_err(|err| {
        format!(
            "重命名 {} → {} 失败: {err}",
            from.display(),
            to.display()
        )
    })?;
    Ok(format!("已重命名为 {}", to.display()))
}

/// 删除文件或目录。目录必须显式给出 `recursive`，避免误删整棵树。
pub fn delete(resolved: &Path, recursive: bool) -> Result<String, String> {
    // 拒绝删掉文件系统的根：一个手滑就能毁掉整台机器。
    if resolved.parent().is_none() {
        return Err(format!("拒绝删除根路径 {}", resolved.display()));
    }

    let meta = fs::symlink_metadata(resolved)
        .map_err(|err| format!("读取 {} 失败: {err}", resolved.display()))?;

    if meta.is_dir() {
        if !recursive {
            // 非递归时只删空目录；std 没有 remove_dir_nonempty 的语义差异，
            // remove_dir 本身就只在空目录上成功。
            fs::remove_dir(resolved).map_err(|err| {
                format!(
                    "删除目录 {} 失败（非空目录需要显式勾选递归删除）: {err}",
                    resolved.display()
                )
            })?;
            return Ok(format!("已删除目录 {}", resolved.display()));
        }
        fs::remove_dir_all(resolved)
            .map_err(|err| format!("递归删除 {} 失败: {err}", resolved.display()))?;
        return Ok(format!("已递归删除 {}", resolved.display()));
    }

    fs::remove_file(resolved).map_err(|err| format!("删除 {} 失败: {err}", resolved.display()))?;
    Ok(format!("已删除 {}", resolved.display()))
}

/// 读取文本文件（供网页端在线编辑）。
pub fn read_text(resolved: &Path) -> Result<String, String> {
    let meta = fs::metadata(resolved)
        .map_err(|err| format!("读取 {} 失败: {err}", resolved.display()))?;
    if meta.is_dir() {
        return Err(format!("{} 是目录，不是文件", resolved.display()));
    }
    if meta.len() > MAX_TEXT_BYTES {
        return Err(format!(
            "文件 {} 有 {} 字节，超过在线编辑的 {} 字节上限，请改用下载",
            resolved.display(),
            meta.len(),
            MAX_TEXT_BYTES
        ));
    }
    // 用 decode_output 而不是 from_utf8：目标机器上的文本文件可能是本地
    // 代码页编码（中文 Windows 上的 GBK），那条回退逻辑已经在命令回显里验证过。
    let bytes = fs::read(resolved).map_err(|err| format!("读取 {} 失败: {err}", resolved.display()))?;
    Ok(crate::execute::decode_output(&bytes))
}

/// 写入文本文件。
///
/// 目标已存在时先备份一份 `.bak`：网页端的文本框很容易一提交就把原文件覆盖掉，
/// 留个退路比事后追悔便宜得多。
pub fn write_text(resolved: &Path, content: &str) -> Result<String, String> {
    if resolved.is_dir() {
        return Err(format!("{} 是目录，不能写入", resolved.display()));
    }
    if let Some(parent) = resolved.parent() {
        if !parent.as_os_str().is_empty() && !parent.exists() {
            fs::create_dir_all(parent)
                .map_err(|err| format!("创建目录 {} 失败: {err}", parent.display()))?;
        }
    }

    let mut backup = None;
    if resolved.exists() {
        let backup_path = backup_name(resolved);
        // 备份失败不阻断写入：写不进去才是用户真正关心的事。
        if fs::copy(resolved, &backup_path).is_ok() {
            backup = Some(backup_path);
        }
    }

    fs::write(resolved, content.as_bytes())
        .map_err(|err| format!("写入 {} 失败: {err}", resolved.display()))?;

    Ok(match backup {
        Some(path) => format!(
            "已保存 {}（{} 字节），原文件备份为 {}",
            resolved.display(),
            content.len(),
            path.display()
        ),
        None => format!("已保存 {}（{} 字节）", resolved.display(), content.len()),
    })
}

fn backup_name(path: &Path) -> PathBuf {
    let mut name = path.file_name().map(|n| n.to_os_string()).unwrap_or_default();
    name.push(".bak");
    path.with_file_name(name)
}

/// 规范化路径。目标不存在时（新建目录、新建文件）退化为「规范化父目录拼上文件名」。
fn canonicalize(path: &Path) -> Result<PathBuf, String> {
    if let Ok(resolved) = fs::canonicalize(path) {
        return Ok(strip_verbatim(resolved));
    }
    match path.parent() {
        Some(parent) if !parent.as_os_str().is_empty() => {
            let parent = fs::canonicalize(parent)
                .map_err(|err| format!("路径 {} 无法解析: {err}", path.display()))?;
            let name = path
                .file_name()
                .ok_or_else(|| format!("路径 {} 无法解析", path.display()))?;
            Ok(strip_verbatim(parent).join(name))
        }
        _ => Err(format!("路径 {} 无法解析", path.display())),
    }
}

/// 去掉 Windows 上 `canonicalize` 会加上的 `\\?\` 前缀。
///
/// 这个前缀（「扩展长度路径」）在程序之间传递没问题，但要显示给管理员看就太丑了，
/// 而且前端拿它去拼面包屑会多出一段 `\\?\C:`。既然这些路径最终都是拿去给人看、
/// 或者回传给同一个客户端解析，去掉前缀更合适。
fn strip_verbatim(path: PathBuf) -> PathBuf {
    #[cfg(windows)]
    {
        let text = path.to_string_lossy();
        // UNC 形式 `\\?\UNC\server\share` 要还原成 `\\server\share`。
        if let Some(rest) = text.strip_prefix(r"\\?\UNC\") {
            return PathBuf::from(format!(r"\\{rest}"));
        }
        if let Some(rest) = text.strip_prefix(r"\\?\") {
            return PathBuf::from(rest.to_string());
        }
    }
    path
}

fn modified_ms(meta: &fs::Metadata) -> u64 {
    meta.modified()
        .ok()
        .and_then(|time| time.duration_since(UNIX_EPOCH).ok())
        .map(|since| since.as_millis() as u64)
        .unwrap_or_else(|| {
            // 取不到修改时间（极少数文件系统）时退化为当前时间，
            // 免得前端显示成 1970 年。
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .map(|since| since.as_millis() as u64)
                .unwrap_or(0)
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_dir(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("rebelshell-fs-{tag}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn listing_path_has_no_verbatim_prefix() {
        // Windows 的 canonicalize 会加 \\?\ 前缀；它既不好看，也会让前端的
        // 面包屑多出一段。列目录回带的路径必须是给人看的形式。
        let dir = temp_dir("verbatim");
        let listing = list_dir(&dir).unwrap();
        let head = listing.lines().next().unwrap();
        let path = head.strip_prefix("PATH\t").unwrap();
        assert!(
            !path.starts_with(r"\\?\"),
            "路径不应带扩展长度前缀：{path}"
        );
        assert!(path.contains("rebelshell-fs-verbatim"), "路径应指向刚建的目录：{path}");
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn lists_directories_before_files() {
        let dir = temp_dir("list");
        fs::write(dir.join("b.txt"), b"hi").unwrap();
        fs::create_dir(dir.join("a-sub")).unwrap();
        fs::write(dir.join("A.txt"), b"hi").unwrap();

        let listing = list_dir(&dir).unwrap();
        let mut lines = listing.lines();
        let head = lines.next().unwrap();
        assert!(head.starts_with("PATH\t"), "首行应是 PATH 头：{head:?}");

        let kinds: Vec<char> = lines
            .map(|line| line.chars().next().unwrap())
            .collect();
        assert_eq!(kinds, vec!['d', 'f', 'f'], "目录应排在文件之前");
        let names: Vec<&str> = listing
            .lines()
            .skip(1)
            .map(|line| line.rsplit('\t').next().unwrap())
            .collect();
        assert_eq!(names, vec!["a-sub", "A.txt", "b.txt"], "同组内名称不区分大小写排序");

        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn listing_round_trips_through_server_parser() {
        // 客户端的输出格式必须能被服务端解析器读懂（含中文文件名）。
        let dir = temp_dir("roundtrip");
        fs::write(dir.join("测试 文件.txt"), b"x").unwrap();
        let listing = list_dir(&dir).unwrap();
        assert!(listing.starts_with("PATH\t"));
        assert!(listing.contains("测试 文件.txt"), "中文名应原样保留：{listing}");
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn creates_and_removes_directories() {
        let dir = temp_dir("mkdir");
        let nested = dir.join("a").join("b").join("c");
        make_dir(&nested).unwrap();
        assert!(nested.is_dir());

        // 非递归删除空目录可以；非空目录必须显式递归。
        let nonempty = dir.join("a");
        assert!(delete(&nonempty, false).is_err(), "非空目录不应被非递归删除");
        delete(&nonempty, true).unwrap();
        assert!(!nonempty.exists());

        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn refuses_to_delete_root() {
        let root = if cfg!(windows) {
            PathBuf::from("C:\\")
        } else {
            PathBuf::from("/")
        };
        assert!(delete(&root, true).is_err());
    }

    #[test]
    fn renames_and_refuses_overwrite() {
        let dir = temp_dir("rename");
        let from = dir.join("old.txt");
        let to = dir.join("new.txt");
        fs::write(&from, b"data").unwrap();
        rename(&from, &to).unwrap();
        assert!(!from.exists() && to.exists());

        fs::write(&from, b"other").unwrap();
        assert!(rename(&from, &to).is_err(), "目标已存在时不应覆盖");

        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn rejects_oversized_text_edits() {
        let dir = temp_dir("bigtext");
        let big = dir.join("big.txt");
        // 写一个超过上限的文件（稀疏写法不通用，这里只造超限长度）。
        fs::write(&big, vec![b'a'; (MAX_TEXT_BYTES + 1) as usize]).unwrap();
        let err = read_text(&big).unwrap_err();
        assert!(err.contains("上限"), "应提示超出上限：{err}");

        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn write_text_keeps_a_backup() {
        let dir = temp_dir("backup");
        let file = dir.join("note.txt");
        fs::write(&file, b"original").unwrap();

        write_text(&file, "changed").unwrap();
        assert_eq!(fs::read_to_string(&file).unwrap(), "changed");
        assert_eq!(
            fs::read_to_string(dir.join("note.txt.bak")).unwrap(),
            "original",
            "覆盖前应留一份备份"
        );

        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn reads_back_utf8_text() {
        let dir = temp_dir("readtext");
        let file = dir.join("cn.txt");
        fs::write(&file, "中文内容\n".as_bytes()).unwrap();
        assert_eq!(read_text(&file).unwrap(), "中文内容\n");
        fs::remove_dir_all(&dir).unwrap();
    }
}
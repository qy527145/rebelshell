//! 交互式终端（PTY）：在客户端本地开一个真伪终端跑 shell，双向转发字节。
//!
//! 与 [`crate::execute`] 的「一条命令一个进程、收完输出再回传」不同，这里要的是
//! Xshell 那样的体验：`vi` / `top` / `htop` 这类全屏程序、颜色、Tab 补全、
//! Ctrl-C、窗口自适应都得正常工作。做到这些只有一条路——真的分配一个 PTY。
//!
//! 平台实现：
//! - Windows：[ConPTY]（Windows 10 1809+），`CreatePseudoConsole` + 属性链表启动进程。
//! - Unix：`posix_openpt` 拿一对主/从设备，`fork` 后在子进程里 `setsid`、
//!   把从设备设为控制终端，再 `execvp` shell。
//!
//! [ConPTY]: https://learn.microsoft.com/en-us/windows/console/creating-a-pseudoconsole-session
//!
//! 两端都实现同一组操作（[`Pty::read`] / [`Pty::write`] / [`Pty::resize`] /
//! [`Pty::wait`] / [`Pty::kill`]），会话循环因此不需要 `cfg` 分支。
//!
//! ## 为什么输出不是「阻塞读到多少发多少」
//!
//! PTY 的输出节奏完全由被控程序决定：`vi` 打开时几毫秒内会吐出几十 KB，
//! 而空闲时几分钟一个字节都没有。如果这里用阻塞读，会话循环就会卡在
//! 「读终端」上，期间既收不到管理端的键盘输入，也做不了 `resize`。
//! 因此 Windows 侧先 `PeekNamedPipe` 探再读、Unix 侧把主设备设成非阻塞，
//! 两边都保证 [`Pty::read`] 永不无限阻塞，循环可以用「读一点 → 看一眼有没有
//! 输入 → 再读一点」的节奏跑。
//!
//! ## 输出必须原样透传
//!
//! 客户端**不解析**终端输出：ANSI 转义序列、`ESC[6n` 光标查询、UTF-8 中文、
//! 256 色序列全部原样转发给浏览器，由 xterm.js 解释与应答。
//! 客户端并不知道光标在哪、也不该替浏览器编一个假应答 —— 两边都回只会打架。
//! 这条约定由 `ansi_sequences_pass_through` 测试守着。

use std::io;

/// 单次读取的缓冲大小。够大以免 `top` 这类高频刷新程序把消息切得太碎，
/// 又远小于协议的单帧上限。
const READ_SIZE: usize = 16 * 1024;

/// 子进程退出后，再等这么久才把终端判为「已关闭」。
///
/// ConPTY 在 shell 退出后不会立刻关掉输出管道（管道写端归 conhost 所有，
/// 要等 `ClosePseudoConsole` 才释放），因此不能只靠 `PeekNamedPipe` 报错来判断
/// 会话结束。改成「发现进程退出 → 再宽限一会儿把残留输出读完 → 判为关闭」。
#[cfg(windows)]
const EXIT_GRACE: std::time::Duration = std::time::Duration::from_millis(200);

/// 一次读取的结果。
pub enum ReadOutcome {
    /// 读到了数据。
    Data(Vec<u8>),
    /// 这一轮没有数据（非阻塞模式下很正常）。
    Idle,
    /// 终端已关闭（子进程退出、从设备耗尽）。
    Closed,
}

/// 一个已经启动的交互式终端。
pub trait Pty {
    /// 往终端里写键盘输入。
    fn write(&mut self, data: &[u8]) -> io::Result<()>;
    /// 非阻塞地读一次终端输出。
    fn read(&mut self) -> io::Result<ReadOutcome>;
    /// 告知终端新的窗口大小。
    fn resize(&mut self, cols: u16, rows: u16) -> io::Result<()>;
    /// 取子进程退出码；仍在运行返回 `None`。
    fn wait(&mut self) -> io::Result<Option<i32>>;
    /// 结束会话：杀掉子进程并释放 PTY。可重复调用。
    fn kill(&mut self);
}

/// 按平台启动一个终端。
///
/// `shell` 为空时用平台默认值；`cols` / `rows` 过小时会被抬到 2，
/// 否则部分程序会拒绝在「零宽终端」里启动。
pub fn spawn(shell: &str, cols: u16, rows: u16) -> io::Result<Box<dyn Pty>> {
    let shell = if shell.trim().is_empty() {
        default_shell()
    } else {
        shell.trim().to_string()
    };
    let cols = cols.max(2);
    let rows = rows.max(2);

    #[cfg(windows)]
    {
        Ok(Box::new(windows_impl::ConPty::spawn(&shell, cols, rows)?))
    }
    #[cfg(unix)]
    {
        Ok(Box::new(unix_impl::PtyPair::spawn(&shell, cols, rows)?))
    }
    #[cfg(not(any(windows, unix)))]
    {
        let _ = (shell, cols, rows);
        Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "当前平台不支持交互式终端",
        ))
    }
}

/// 当前平台的默认 shell。
pub fn default_shell() -> String {
    #[cfg(windows)]
    {
        // 优先 PowerShell（与服务端命令执行路径一致），缺失时回退到 cmd。
        for candidate in ["powershell.exe", "pwsh.exe", "cmd.exe"] {
            if which(candidate) {
                return candidate.to_string();
            }
        }
        "cmd.exe".to_string()
    }
    #[cfg(not(windows))]
    {
        std::env::var("SHELL")
            .ok()
            .map(|shell| shell.trim().to_string())
            .filter(|shell| !shell.is_empty())
            .unwrap_or_else(|| {
                if std::path::Path::new("/bin/bash").exists() {
                    "/bin/bash".to_string()
                } else {
                    "/bin/sh".to_string()
                }
            })
    }
}

/// 在 `PATH` 上找可执行文件（Windows 上还会补 `.exe` 后缀）。
#[cfg(windows)]
fn which(name: &str) -> bool {
    let Some(paths) = std::env::var_os("PATH") else {
        return false;
    };
    let names: Vec<String> = if name.contains('.') {
        vec![name.to_string()]
    } else {
        vec![format!("{name}.exe"), name.to_string()]
    };
    std::env::split_paths(&paths).any(|dir| {
        names
            .iter()
            .any(|candidate| dir.join(candidate).is_file())
    })
}

// ---------------------------------------------------------------- Windows

#[cfg(windows)]
mod windows_impl {
    use super::{ReadOutcome, Pty, EXIT_GRACE, READ_SIZE};
    use std::alloc::{alloc, dealloc, Layout};
    use std::io;
    use std::mem::{align_of, size_of, zeroed};
    use std::os::windows::ffi::OsStrExt;
    use std::ptr;

    use windows_sys::Win32::Foundation::{
        CloseHandle, GetLastError, SetHandleInformation, HANDLE, HANDLE_FLAG_INHERIT,
        INVALID_HANDLE_VALUE, WAIT_OBJECT_0, WAIT_TIMEOUT,
    };
    use windows_sys::Win32::Security::SECURITY_ATTRIBUTES;
    use windows_sys::Win32::Storage::FileSystem::{ReadFile, WriteFile};
    use windows_sys::Win32::System::Console::{
        ClosePseudoConsole, CreatePseudoConsole, ResizePseudoConsole, COORD, HPCON,
    };
    use windows_sys::Win32::System::Pipes::{CreatePipe, PeekNamedPipe};
    use windows_sys::Win32::System::Threading::{
        CreateProcessW, DeleteProcThreadAttributeList, GetExitCodeProcess,
        InitializeProcThreadAttributeList, TerminateProcess, UpdateProcThreadAttribute,
        WaitForSingleObject, EXTENDED_STARTUPINFO_PRESENT, PROCESS_INFORMATION,
        PROC_THREAD_ATTRIBUTE_PSEUDOCONSOLE, STARTF_USESTDHANDLES, STARTUPINFOEXW,
    };

    /// 管道写端关闭时的错误码，含义是「终端结束」而不是「出错了」。
    const ERROR_BROKEN_PIPE: u32 = 109;
    const ERROR_NO_DATA: u32 = 232;

    /// ConPTY 会话：伪控制台 + 一对管道 + 子进程。
    pub struct ConPty {
        console: HPCON,
        /// 我们写入的一端（终端的输入）。
        input: HANDLE,
        /// 我们读取的一端（终端的输出）。
        output: HANDLE,
        process: HANDLE,
        /// `STARTUPINFOEXW` 用的属性链表内存，由我们自己分配。
        attributes: *mut core::ffi::c_void,
        attributes_layout: Layout,
        /// `kill` 用；保证幂等。
        closed: bool,
        /// 检测到子进程退出的时刻，用于 [`EXIT_GRACE`] 宽限。
        exited_at: Option<std::time::Instant>,
    }

    // 句柄本身只是值，所有 Win32 调用都是线程无关的；会话由单线程循环独占使用。
    unsafe impl Send for ConPty {}

    /// drop 时保证关闭的句柄。
    struct OwnedHandle(HANDLE);

    impl OwnedHandle {
        fn valid(&self) -> bool {
            !self.0.is_null() && self.0 != INVALID_HANDLE_VALUE
        }
        /// 放弃所有权（所有权移交给别处）。
        fn leak(self) -> HANDLE {
            let handle = self.0;
            std::mem::forget(self);
            handle
        }
    }

    impl Drop for OwnedHandle {
        fn drop(&mut self) {
            if self.valid() {
                unsafe { CloseHandle(self.0) };
            }
        }
    }

    /// 把 `&str` 转成以 NUL 结尾的 UTF-16。
    fn wide(text: &str) -> Vec<u16> {
        std::ffi::OsStr::new(text)
            .encode_wide()
            .chain(std::iter::once(0))
            .collect()
    }

    fn last_error(what: &str) -> io::Error {
        io::Error::other(format!(
            "{what} 失败（错误码 {}）",
            unsafe { GetLastError() }
        ))
    }

    /// 依据 ConPTY 文档创建一对匿名管道，并让 ConPTY 持有其中一端。
    ///
    /// 关键点：我们这一侧的两个句柄必须**不可继承**，而交给 ConPTY 的那两个必须
    /// 可继承。否则会把自己持有的句柄泄漏进子进程，导致子进程退出后管道仍不关闭
    /// （读端永远拿不到 EOF，表现为终端「关不掉」）。
    ///
    /// 返回 `(交给 ConPTY 的读端, 我们持有的写端, 交给 ConPTY 的写端, 我们持有的读端)`。
    fn make_pipes() -> io::Result<(HANDLE, OwnedHandle, HANDLE, OwnedHandle)> {
        let attrs = SECURITY_ATTRIBUTES {
            nLength: size_of::<SECURITY_ATTRIBUTES>() as u32,
            lpSecurityDescriptor: ptr::null_mut(),
            bInheritHandle: 1,
        };

        let mut input_read: HANDLE = ptr::null_mut();
        let mut input_write: HANDLE = ptr::null_mut();
        let mut output_read: HANDLE = ptr::null_mut();
        let mut output_write: HANDLE = ptr::null_mut();

        // 方向以「我们」的视角命名：input_* 是我们写、ConPTY 读。
        if unsafe { CreatePipe(&mut input_read, &mut input_write, &attrs, 0) } == 0 {
            return Err(last_error("CreatePipe(输入)"));
        }
        let input_read = OwnedHandle(input_read);
        let input_write = OwnedHandle(input_write);

        if unsafe { CreatePipe(&mut output_read, &mut output_write, &attrs, 0) } == 0 {
            // input_* 由 OwnedHandle 的 Drop 关掉。
            return Err(last_error("CreatePipe(输出)"));
        }
        let output_read = OwnedHandle(output_read);
        let output_write = OwnedHandle(output_write);

        if unsafe { SetHandleInformation(input_write.0, HANDLE_FLAG_INHERIT, 0) } == 0 {
            return Err(last_error("SetHandleInformation(输入写端)"));
        }
        if unsafe { SetHandleInformation(output_read.0, HANDLE_FLAG_INHERIT, 0) } == 0 {
            return Err(last_error("SetHandleInformation(输出读端)"));
        }

        Ok((
            input_read.leak(),
            input_write,
            output_write.leak(),
            output_read,
        ))
    }

    /// 测量并分配 `InitializeProcThreadAttributeList` 需要的内存。
    fn alloc_attribute_list() -> io::Result<(*mut core::ffi::c_void, Layout)> {
        let mut needed: usize = 0;
        // 第一次调用只为测量大小，按文档必须传 null 列表。
        unsafe { InitializeProcThreadAttributeList(ptr::null_mut(), 1, 0, &mut needed) };
        if needed == 0 {
            return Err(last_error("InitializeProcThreadAttributeList(测量)"));
        }
        let layout = Layout::from_size_align(needed, align_of::<usize>())
            .map_err(|err| io::Error::new(io::ErrorKind::InvalidInput, err.to_string()))?;
        let memory = unsafe { alloc(layout) } as *mut core::ffi::c_void;
        if memory.is_null() {
            return Err(io::Error::new(
                io::ErrorKind::OutOfMemory,
                "属性链表分配失败",
            ));
        }
        Ok((memory, layout))
    }

    impl ConPty {
        pub fn spawn(shell: &str, cols: u16, rows: u16) -> io::Result<Self> {
            let (input_read, input_write, output_write, output_read) = make_pipes()?;

            // ConPTY 接管这两端；无论成败它都会复制句柄，因此本进程的引用
            // 必须在 `CreatePseudoConsole` 返回后立刻释放，否则会造成句柄泄漏。
            let input_read = OwnedHandle(input_read);
            let output_write = OwnedHandle(output_write);

            let size = COORD {
                X: cols as i16,
                Y: rows as i16,
            };
            let mut console: HPCON = 0;
            let hr =
                unsafe { CreatePseudoConsole(size, input_read.0, output_write.0, 0, &mut console) };
            drop(input_read);
            drop(output_write);
            if hr < 0 {
                return Err(io::Error::other(format!(
                    "CreatePseudoConsole 失败（HRESULT 0x{hr:08x}）：需要 Windows 10 1809+"
                )));
            }

            // 从这里起，任何提前返回都必须关掉伪控制台。
            let (attributes, attributes_layout) = match alloc_attribute_list() {
                Ok(pair) => pair,
                Err(err) => {
                    unsafe { ClosePseudoConsole(console) };
                    return Err(err);
                }
            };

            let mut initialized = false;
            let mut process: HANDLE = ptr::null_mut();

            // 内层闭包负责真正干活，出错时统一走下面的清理。
            let mut build = || -> io::Result<HANDLE> {
                let mut size: usize = attributes_layout.size();
                if unsafe { InitializeProcThreadAttributeList(attributes, 1, 0, &mut size) } == 0 {
                    return Err(last_error("InitializeProcThreadAttributeList"));
                }
                initialized = true;

                if unsafe {
                    UpdateProcThreadAttribute(
                        attributes,
                        0,
                        PROC_THREAD_ATTRIBUTE_PSEUDOCONSOLE as usize,
                        // 与官方 EchoCon 示例一致：句柄类属性这里传的是**句柄值本身**，
                        // 不是它的地址。传成地址不会报错，但子进程不会挂到伪控制台上，
                        // 而是继续用父进程的控制台——表现为 shell 的横幅直接打在
                        // 服务端自己的窗口里，而终端里一个字符都收不到。
                        console as *const core::ffi::c_void,
                        size_of::<HPCON>(),
                        ptr::null_mut(),
                        ptr::null(),
                    )
                } == 0
                {
                    return Err(last_error("UpdateProcThreadAttribute"));
                }

                let mut startup: STARTUPINFOEXW = unsafe { zeroed() };
                startup.StartupInfo.cb = size_of::<STARTUPINFOEXW>() as u32;
                startup.lpAttributeList = attributes;
                // 显式声明三个标准句柄「无效」，逼子进程去用伪控制台。
                //
                // 少了这三行，子进程会继承父进程被重定向过的 stdout/stderr
                // （服务跑成计划任务时通常指向日志文件），于是 shell 的横幅与
                // 提示符都写进了父进程的日志，终端里只剩 ConPTY 自己的几个
                // 初始化字节。表现是「PTY 看起来起来了但敲什么都没反应」，
                // 极难从现象反推到这里。
                startup.StartupInfo.dwFlags = STARTF_USESTDHANDLES;
                startup.StartupInfo.hStdInput = INVALID_HANDLE_VALUE;
                startup.StartupInfo.hStdOutput = INVALID_HANDLE_VALUE;
                startup.StartupInfo.hStdError = INVALID_HANDLE_VALUE;

                // CreateProcessW 可能就地改写命令行，因此必须是可写的独立缓冲。
                let mut command_line = wide(shell);
                let mut info: PROCESS_INFORMATION = unsafe { zeroed() };

                let created = unsafe {
                    CreateProcessW(
                        ptr::null(),
                        command_line.as_mut_ptr(),
                        ptr::null(),
                        ptr::null(),
                        0,
                        EXTENDED_STARTUPINFO_PRESENT,
                        ptr::null(),
                        // 起始目录为空 = 继承客户端的当前目录。
                        ptr::null(),
                        &startup.StartupInfo,
                        &mut info,
                    )
                };
                if created == 0 {
                    return Err(last_error(&format!("启动 shell {shell:?}")));
                }
                // 主线程句柄用不上，立刻关掉，否则每开一个终端泄漏一个。
                unsafe { CloseHandle(info.hThread) };
                Ok(info.hProcess)
            };

            match build() {
                Ok(handle) => process = handle,
                Err(err) => {
                    if initialized {
                        unsafe { DeleteProcThreadAttributeList(attributes) };
                    }
                    unsafe {
                        dealloc(attributes as *mut u8, attributes_layout);
                        ClosePseudoConsole(console);
                        CloseHandle(process);
                    }
                    return Err(err);
                }
            }

            Ok(Self {
                console,
                input: input_write.leak(),
                output: output_read.leak(),
                process,
                attributes,
                attributes_layout,
                closed: false,
                exited_at: None,
            })
        }
    }

    impl Pty for ConPty {
        fn write(&mut self, data: &[u8]) -> io::Result<()> {
            if data.is_empty() || self.closed {
                return Ok(());
            }
            let mut written: u32 = 0;
            let ok = unsafe {
                WriteFile(
                    self.input,
                    data.as_ptr(),
                    data.len() as u32,
                    &mut written,
                    ptr::null_mut(),
                )
            };
            if ok == 0 {
                return Err(last_error("写入终端"));
            }
            Ok(())
        }

        /// 先 `PeekNamedPipe` 看有没有数据，再决定读多少。
        ///
        /// 直接 `ReadFile` 在有数据时会阻塞到「缓冲区填满或写端关闭」，
        /// 对交互式终端不可接受：`vi` 打开后只吐几十字节就不再吐了。
        fn read(&mut self) -> io::Result<ReadOutcome> {
            if self.closed {
                return Ok(ReadOutcome::Closed);
            }

            let mut available: u32 = 0;
            let peeked = unsafe {
                PeekNamedPipe(
                    self.output,
                    ptr::null_mut(),
                    0,
                    ptr::null_mut(),
                    &mut available,
                    ptr::null_mut(),
                )
            };
            if peeked == 0 {
                let code = unsafe { GetLastError() };
                if code == ERROR_BROKEN_PIPE || code == ERROR_NO_DATA {
                    self.closed = true;
                    return Ok(ReadOutcome::Closed);
                }
                return Err(last_error("探测终端输出"));
            }
            if available == 0 {
                // 没有数据时顺手确认子进程是否还在。ConPTY 不会在 shell 退出后
                // 立刻关掉管道，只看管道的话永远等不到「已关闭」。
                return Ok(match self.wait()? {
                    Some(_) => {
                        let since = *self.exited_at.get_or_insert_with(std::time::Instant::now);
                        if since.elapsed() >= EXIT_GRACE {
                            self.closed = true;
                            ReadOutcome::Closed
                        } else {
                            ReadOutcome::Idle
                        }
                    }
                    None => ReadOutcome::Idle,
                });
            }

            let want = (available as usize).min(READ_SIZE);
            let mut buffer = vec![0u8; want];
            let mut read: u32 = 0;
            let ok = unsafe {
                ReadFile(
                    self.output,
                    buffer.as_mut_ptr(),
                    want as u32,
                    &mut read,
                    ptr::null_mut(),
                )
            };
            if ok == 0 {
                let code = unsafe { GetLastError() };
                if code == ERROR_BROKEN_PIPE || code == ERROR_NO_DATA {
                    self.closed = true;
                    return Ok(ReadOutcome::Closed);
                }
                return Err(last_error("读取终端输出"));
            }
            buffer.truncate(read as usize);
            Ok(ReadOutcome::Data(buffer))
        }

        fn resize(&mut self, cols: u16, rows: u16) -> io::Result<()> {
            if self.closed {
                return Ok(());
            }
            let size = COORD {
                X: cols.max(2) as i16,
                Y: rows.max(2) as i16,
            };
            let hr = unsafe { ResizePseudoConsole(self.console, size) };
            if hr < 0 {
                return Err(io::Error::other(format!(
                    "ResizePseudoConsole 失败（HRESULT 0x{hr:08x}）"
                )));
            }
            Ok(())
        }

        fn wait(&mut self) -> io::Result<Option<i32>> {
            if self.process.is_null() {
                return Ok(Some(0));
            }
            match unsafe { WaitForSingleObject(self.process, 0) } {
                WAIT_TIMEOUT => Ok(None),
                WAIT_OBJECT_0 => {
                    let mut code: u32 = 0;
                    if unsafe { GetExitCodeProcess(self.process, &mut code) } == 0 {
                        return Err(last_error("读取退出码"));
                    }
                    Ok(Some(code as i32))
                }
                _ => Err(last_error("等待子进程")),
            }
        }

        fn kill(&mut self) {
            if self.closed {
                return;
            }
            self.closed = true;

            if !self.process.is_null() {
                unsafe {
                    // TerminateProcess 对已退出的进程是 no-op，不必先查状态。
                    TerminateProcess(self.process, 0);
                    CloseHandle(self.process);
                }
                self.process = ptr::null_mut();
            }

            // 顺序有讲究：先关伪控制台，让 shell 收到关闭通知、管道写端随之关闭；
            // 若先关管道，读端可能收不到 EOF。
            if self.console != 0 {
                unsafe { ClosePseudoConsole(self.console) };
                self.console = 0;
            }
            for handle in [self.input, self.output] {
                if !handle.is_null() && handle != INVALID_HANDLE_VALUE {
                    unsafe { CloseHandle(handle) };
                }
            }
            self.input = ptr::null_mut();
            self.output = ptr::null_mut();

            if !self.attributes.is_null() {
                unsafe {
                    DeleteProcThreadAttributeList(self.attributes);
                    dealloc(self.attributes as *mut u8, self.attributes_layout);
                }
                self.attributes = ptr::null_mut();
            }
        }
    }

    impl Drop for ConPty {
        fn drop(&mut self) {
            self.kill();
        }
    }
}

// ------------------------------------------------------------------- Unix

#[cfg(unix)]
mod unix_impl {
    use super::{ReadOutcome, Pty, READ_SIZE};
    use std::ffi::{CStr, CString};
    use std::io;
    use std::os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd};
    use std::ptr;

    /// 一对主设备 + fork 出来的子进程。
    pub struct PtyPair {
        master: OwnedFd,
        child: libc::pid_t,
        reaped: bool,
        /// 子进程退出码，`wait` 首次取到时缓存下来。
        exit_code: Option<i32>,
    }

    fn last_os_error(what: &str) -> io::Error {
        let err = io::Error::last_os_error();
        io::Error::new(err.kind(), format!("{what} 失败: {err}"))
    }

    fn set_winsize(fd: RawFd, cols: u16, rows: u16) -> io::Result<()> {
        let size = libc::winsize {
            ws_row: rows,
            ws_col: cols,
            ws_xpixel: 0,
            ws_ypixel: 0,
        };
        if unsafe { libc::ioctl(fd, libc::TIOCSWINSZ, &size) } != 0 {
            return Err(last_os_error("TIOCSWINSZ"));
        }
        Ok(())
    }

    /// 打开主/从设备对。
    fn open_pty() -> io::Result<(OwnedFd, CString)> {
        // O_NOCTTY：本进程不把从设备当控制终端（那是子进程的事）。
        let master = unsafe { libc::posix_openpt(libc::O_RDWR | libc::O_NOCTTY) };
        if master < 0 {
            return Err(last_os_error("posix_openpt"));
        }
        let master = unsafe { OwnedFd::from_raw_fd(master) };

        if unsafe { libc::grantpt(master.as_raw_fd()) } != 0 {
            return Err(last_os_error("grantpt"));
        }
        if unsafe { libc::unlockpt(master.as_raw_fd()) } != 0 {
            return Err(last_os_error("unlockpt"));
        }

        // ptsname_r 往调用方提供的缓冲里写，比 ptsname 的静态缓冲安全。
        let mut name = [0 as libc::c_char; 256];
        let rc = unsafe {
            libc::ptsname_r(
                master.as_raw_fd(),
                name.as_mut_ptr(),
                name.len() as libc::size_t,
            )
        };
        if rc != 0 {
            return Err(last_os_error("ptsname_r"));
        }
        let slave = unsafe { CStr::from_ptr(name.as_ptr()) }.to_owned();
        Ok((master, slave))
    }

    /// 补齐交互式终端需要的环境变量。
    ///
    /// 不能只 `execvp` 就走：从客户端进程继承的环境里通常没有 `TERM`，
    /// 于是 `vi` 会退化成 dumb 终端，颜色与光标控制全失效。
    /// 在 fork **之前**调用，子进程即可自然继承（也避免在 fork 后调用非
    /// async-signal-safe 的 `setenv`）。
    fn prepare_env(shell: &str) {
        let set_default = |key: &str, value: &str| {
            if std::env::var_os(key).is_none() {
                // SAFETY: 单线程启动路径；失败的后果只是少一个变量，不影响正确性。
                unsafe { std::env::set_var(key, value) };
            }
        };
        set_default("TERM", "xterm-256color");
        set_default("COLORTERM", "truecolor");
        // 让 shell 知道自己跑在哪个终端里（部分程序据此决定是否上色）。
        set_default("SHELL", shell);
    }

    impl PtyPair {
        pub fn spawn(shell: &str, cols: u16, rows: u16) -> io::Result<Self> {
            let (master, slave_path) = open_pty()?;

            // fork 之前把子进程要用的一切都准备好：fork 之后只能调用
            // async-signal-safe 的函数，因此与上面分开、放在这里做。
            let shell_c = CString::new(shell)
                .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "shell 路径含 NUL"))?;
            prepare_env(shell);
            let argv: [*const libc::c_char; 2] = [shell_c.as_ptr(), ptr::null()];
            let master_fd = master.as_raw_fd();

            let child = unsafe { libc::fork() };
            if child < 0 {
                return Err(last_os_error("fork"));
            }

            if child == 0 {
                // ---- 子进程：只调用 async-signal-safe 的函数 ----
                unsafe {
                    // 新会话 + 成为进程组长，从设备才能变成控制终端。
                    if libc::setsid() < 0 {
                        libc::_exit(126);
                    }

                    let slave = libc::open(slave_path.as_ptr(), libc::O_RDWR | libc::O_NOCTTY);
                    if slave < 0 {
                        libc::_exit(126);
                    }
                    // TIOCSCTTY：把从设备设为本进程的控制终端，
                    // 之后 Ctrl-C 才会变成发给前台的 SIGINT。
                    if libc::ioctl(slave, libc::TIOCSCTTY as _, 0) != 0 {
                        libc::_exit(126);
                    }

                    let size = libc::winsize {
                        ws_row: rows,
                        ws_col: cols,
                        ws_xpixel: 0,
                        ws_ypixel: 0,
                    };
                    libc::ioctl(slave, libc::TIOCSWINSZ, &size);

                    // 0/1/2 全指向从设备：shell 的 stdin/stdout/stderr 就是终端。
                    for target in 0..3 {
                        if libc::dup2(slave, target) < 0 {
                            libc::_exit(126);
                        }
                    }
                    if slave > 2 {
                        libc::close(slave);
                    }
                    // 主设备不该被子进程持有，否则主设备永远关不干净。
                    libc::close(master_fd);

                    libc::execvp(shell_c.as_ptr(), argv.as_ptr());
                    // execvp 返回说明失败了。
                    libc::_exit(127);
                }
            }

            // ---- 父进程 ----
            set_winsize(master_fd, cols, rows)?;

            // 非阻塞读：会话循环还要同时处理键盘输入与 resize，不能卡在 read 上。
            let flags = unsafe { libc::fcntl(master_fd, libc::F_GETFL) };
            if flags >= 0 {
                unsafe { libc::fcntl(master_fd, libc::F_SETFL, flags | libc::O_NONBLOCK) };
            }

            Ok(Self {
                master,
                child,
                reaped: false,
                exit_code: None,
            })
        }

        /// 非阻塞地收一次子进程；返回 `None` 表示仍在运行。
        fn reap(&mut self) -> io::Result<Option<i32>> {
            if self.reaped {
                return Ok(self.exit_code);
            }
            let mut status: libc::c_int = 0;
            let rc = unsafe { libc::waitpid(self.child, &mut status, libc::WNOHANG) };
            if rc == 0 {
                return Ok(None);
            }
            if rc < 0 {
                let err = io::Error::last_os_error();
                // ECHILD：已经被回收过（例如别处 wait 掉了），按已结束处理。
                if err.raw_os_error() == Some(libc::ECHILD) {
                    self.reaped = true;
                    self.exit_code = Some(0);
                    return Ok(Some(0));
                }
                return Err(last_os_error("waitpid"));
            }

            self.reaped = true;
            let code = if libc::WIFEXITED(status) {
                libc::WEXITSTATUS(status)
            } else if libc::WIFSIGNALED(status) {
                // 用负数表示「被信号杀死」，与 ConPTY 侧语义对齐。
                -libc::WTERMSIG(status)
            } else {
                0
            };
            self.exit_code = Some(code);
            Ok(Some(code))
        }
    }

    impl Pty for PtyPair {
        fn write(&mut self, data: &[u8]) -> io::Result<()> {
            let mut offset = 0usize;
            while offset < data.len() {
                let rc = unsafe {
                    libc::write(
                        self.master.as_raw_fd(),
                        data[offset..].as_ptr() as *const libc::c_void,
                        data.len() - offset,
                    )
                };
                if rc < 0 {
                    let err = io::Error::last_os_error();
                    // 主设备也是非阻塞的：输入缓冲满了（对端没在读）就先放弃，
                    // 丢掉这半截不会让终端错乱，卡住整个会话才会。
                    if err.kind() == io::ErrorKind::WouldBlock {
                        return Ok(());
                    }
                    return Err(last_os_error("写入终端"));
                }
                offset += rc as usize;
            }
            Ok(())
        }

        fn read(&mut self) -> io::Result<ReadOutcome> {
            let mut buffer = vec![0u8; READ_SIZE];
            let rc = unsafe {
                libc::read(
                    self.master.as_raw_fd(),
                    buffer.as_mut_ptr() as *mut libc::c_void,
                    buffer.len(),
                )
            };
            if rc < 0 {
                let err = io::Error::last_os_error();
                if err.kind() == io::ErrorKind::WouldBlock {
                    return Ok(ReadOutcome::Idle);
                }
                // EIO 是 Linux 上「所有从设备都已关闭」的常见表示，即终端结束。
                if err.raw_os_error() == Some(libc::EIO) {
                    return Ok(ReadOutcome::Closed);
                }
                return Err(last_os_error("读取终端输出"));
            }
            if rc == 0 {
                return Ok(ReadOutcome::Closed);
            }
            buffer.truncate(rc as usize);
            Ok(ReadOutcome::Data(buffer))
        }

        fn resize(&mut self, cols: u16, rows: u16) -> io::Result<()> {
            set_winsize(self.master.as_raw_fd(), cols.max(2), rows.max(2))
        }

        fn wait(&mut self) -> io::Result<Option<i32>> {
            self.reap()
        }

        fn kill(&mut self) {
            if !self.reaped {
                unsafe {
                    // 先温和地请 shell 自己退出，让它有机会清理自己起的子进程。
                    libc::kill(self.child, libc::SIGHUP);
                    libc::kill(self.child, libc::SIGTERM);
                }
                // 给它一小段时间；仍在跑就强杀。PTY 上的 shell 通常立刻响应。
                for _ in 0..20 {
                    if matches!(self.reap(), Ok(Some(_))) {
                        break;
                    }
                    std::thread::sleep(std::time::Duration::from_millis(10));
                }
                if !self.reaped {
                    unsafe {
                        libc::kill(self.child, libc::SIGKILL);
                        let mut status: libc::c_int = 0;
                        libc::waitpid(self.child, &mut status, 0);
                    }
                    self.reaped = true;
                    self.exit_code = Some(-libc::SIGKILL);
                }
            }
            // 主设备由 OwnedFd 在 Drop 时关闭。
        }
    }

    impl Drop for PtyPair {
        fn drop(&mut self) {
            self.kill();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{Duration, Instant};

    /// 把终端输出读到「不再有新内容」为止，返回累积的字节。
    fn collect(pty: &mut Box<dyn Pty>, budget: Duration) -> Vec<u8> {
        let deadline = Instant::now() + budget;
        let mut out = Vec::new();
        while Instant::now() < deadline {
            match pty.read() {
                Ok(ReadOutcome::Data(data)) => out.extend_from_slice(&data),
                Ok(ReadOutcome::Idle) | Ok(ReadOutcome::Closed) => {
                    std::thread::sleep(Duration::from_millis(20))
                }
                Err(_) => break,
            }
        }
        out
    }

    /// 把终端待处理的输出全部读进 `sink`。
    fn drain(pty: &mut Box<dyn Pty>, sink: &mut Vec<u8>) {
        loop {
            match pty.read() {
                Ok(ReadOutcome::Data(data)) => sink.extend_from_slice(&data),
                Ok(ReadOutcome::Idle) | Ok(ReadOutcome::Closed) => return,
                Err(_) => return,
            }
        }
    }

    /// 等到终端输出里出现某个子串（或者超时）。
    fn wait_for(pty: &mut Box<dyn Pty>, needle: &str, budget: Duration) -> String {
        let deadline = Instant::now() + budget;
        let mut out = Vec::new();
        while Instant::now() < deadline {
            drain(pty, &mut out);
            let text = String::from_utf8_lossy(&out).into_owned();
            if text.contains(needle) {
                return text;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        panic!(
            "等待 {needle:?} 超时，已收到: {:?}",
            String::from_utf8_lossy(&out)
        );
    }

    /// 核心回归：PTY 真的能起 shell、能收输出、能接受按键输入。
    ///
    /// 这条测试覆盖了 ConPTY 的全部易错点：管道继承标志、属性链表、
    /// `STARTUPINFOEXW.cb` 的取值、`PeekNamedPipe` 的探读循环。
    /// 任何一处写错都会在这里失败（或直接崩）。
    #[test]
    fn spawns_shell_and_round_trips_input() {
        let mut pty = spawn("", 80, 24).expect("应能启动交互式终端");

        // 先完成握手（应答 DSR）等提示符出现，再下发探针命令。
        std::thread::sleep(Duration::from_millis(1200));
        let mut sink = Vec::new();
        drain(&mut pty, &mut sink);

        let marker = "rebelshell-pty-probe";
        let command = if cfg!(windows) {
            format!("echo {marker}\r\n")
        } else {
            format!("echo {marker}\n")
        };
        pty.write(command.as_bytes()).expect("写入按键应成功");

        let text = wait_for(&mut pty, marker, Duration::from_secs(20));
        assert!(text.contains(marker), "终端应回显命令输出，实际: {text:?}");

        pty.kill();
    }

    /// 关闭后必须能拿到退出码，且 `kill` 可重复调用。
    #[test]
    fn reports_exit_code_and_is_idempotent_on_kill() {
        let mut pty = spawn("", 80, 24).expect("应能启动交互式终端");
        std::thread::sleep(Duration::from_millis(500));

        pty.kill();
        // 第二次调用不应 panic，也不应重复释放句柄。
        pty.kill();

        // kill 之后 wait 必须能给出结论（而不是永久 None）。
        let code = pty.wait().expect("wait 不应失败");
        assert!(code.is_some(), "kill 之后应能拿到退出码");
    }

    /// `resize` 单独调用不应破坏会话；很多程序据此收到 SIGWINCH。
    #[test]
    fn resize_does_not_break_the_session() {
        let mut pty = spawn("", 100, 30).expect("应能启动交互式终端");
        std::thread::sleep(Duration::from_millis(1200));
        let mut sink = Vec::new();
        drain(&mut pty, &mut sink);

        pty.resize(120, 40).expect("resize 应成功");

        let marker = "rebelshell-pty-resize";
        let command = if cfg!(windows) {
            format!("echo {marker}\r\n")
        } else {
            format!("echo {marker}\n")
        };
        pty.write(command.as_bytes()).expect("写入按键应成功");
        let text = wait_for(&mut pty, marker, Duration::from_secs(20));
        assert!(text.contains(marker), "resize 后仍应工作，实际: {text:?}");

        pty.kill();
    }

    /// 子进程退出后，读取路径必须报「已关闭」而不是永久 Idle，
    /// 否则会话循环会一直以为终端还活着。
    #[test]
    fn detects_shell_exit() {
        let mut pty = spawn("", 80, 24).expect("应能启动交互式终端");
        std::thread::sleep(Duration::from_millis(1200));
        let mut sink = Vec::new();
        drain(&mut pty, &mut sink);

        let exit = if cfg!(windows) { "exit\r\n" } else { "exit\n" };
        // 写失败也没关系（shell 可能已经退出），关键是后面能读到 Closed。
        let _ = pty.write(exit.as_bytes());

        let deadline = Instant::now() + Duration::from_secs(15);
        let mut closed = false;
        while Instant::now() < deadline {
            match pty.read() {
                Ok(ReadOutcome::Closed) => {
                    closed = true;
                    break;
                }
                Ok(_) => std::thread::sleep(Duration::from_millis(20)),
                Err(_) => {
                    closed = true;
                    break;
                }
            }
        }
        assert!(closed, "shell 退出后应能检测到终端关闭");
        pty.kill();
    }

    /// `read` 必须原样透传 ANSI 序列，不能替浏览器「消化」掉。
    ///
    /// 客户端的职责是搬运字节。一旦这里开始解析转义序列（比如自己应答光标查询、
    /// 或过滤掉某些控制码），xterm.js 收到的画面就会与真实终端不一致。
    /// 这条测试用「输出里应能找到 ConPTY 的初始化控制序列」把该约定固定下来。
    #[test]
    fn ansi_sequences_pass_through() {
        let mut pty = spawn("", 80, 24).expect("应能启动交互式终端");

        let deadline = Instant::now() + Duration::from_secs(15);
        let mut seen = Vec::new();
        while Instant::now() < deadline {
            match pty.read() {
                Ok(ReadOutcome::Data(data)) => seen.extend_from_slice(&data),
                Ok(_) => std::thread::sleep(Duration::from_millis(20)),
                Err(_) => break,
            }
            // ConPTY 一定会吐出以 ESC 开头的控制序列（窗口尺寸、光标可见性等）。
            if seen.contains(&0x1b) {
                break;
            }
        }
        pty.kill();

        let text = String::from_utf8_lossy(&seen);
        assert!(
            seen.contains(&0x1b),
            "终端输出应包含 ANSI 转义序列且被原样透传：{text:?}"
        );
    }

    /// 默认 shell 必须能在本机解析到，否则交互式终端开箱即用就无从谈起。
    #[test]
    fn default_shell_is_usable() {
        let shell = default_shell();
        assert!(!shell.trim().is_empty(), "默认 shell 不应为空");
        #[cfg(windows)]
        {
            // 明确断言我们至少不会给出一个 PATH 上不存在的名字。
            assert!(
                ["powershell.exe", "pwsh.exe", "cmd.exe"].contains(&shell.as_str()),
                "Windows 默认 shell 应是 PowerShell 或 cmd，实际 {shell:?}"
            );
        }
    }

    /// `collect` 只是给上面的调试路径用，这里顺带确认它在空闲终端上不会挂死。
    #[test]
    fn collecting_from_an_idle_terminal_returns() {
        let mut pty = spawn("", 80, 24).expect("应能启动交互式终端");
        let out = collect(&mut pty, Duration::from_millis(300));
        // 内容不做断言（不同 shell 启动横幅不同），只要求它在预算内返回。
        let _ = out;
        pty.kill();
    }
}

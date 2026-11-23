use regex::Regex;
use serde::{Deserialize, Serialize};
use std::fs;
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::{
    atomic::{AtomicBool, Ordering},
    mpsc, OnceLock,
};
use std::thread;
use std::time::Duration;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Diagnostic {
    pub path: PathBuf,
    pub line: u64,
    pub column: u64,
    pub message: String,
}
#[derive(Clone, Debug, Default)]
pub struct BuildReport {
    pub output: String,
    pub diagnostics: Vec<Diagnostic>,
    pub success: bool,
    pub cancelled: bool,
}
#[derive(Serialize, Deserialize, Clone, Copy, Debug, Default, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum ToolMode {
    #[default]
    Output,
    Filter,
}
#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct UserTool {
    pub name: String,
    pub program: String,
    #[serde(default)]
    pub args: Vec<String>,
    #[serde(default)]
    pub mode: ToolMode,
    #[serde(default)]
    pub shortcut: Option<String>,
}
#[derive(Default, Serialize, Deserialize, Clone, Debug)]
pub struct ToolConfig {
    #[serde(default)]
    pub tools: Vec<UserTool>,
}
impl ToolConfig {
    pub fn load(path: &Path) -> Result<Self, String> {
        match fs::read(path) {
            Ok(bytes) => {
                serde_json::from_slice(&bytes).map_err(|e| format!("{}: {e}", path.display()))
            }
            Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(Self::default()),
            Err(e) => Err(e.to_string()),
        }
    }
    /// Creates a personal starter file the first time Edit Tools is chosen.
    pub fn ensure_example(path: &Path) -> Result<(), String> {
        if path.exists() {
            return Ok(());
        }
        if let Some(dir) = path.parent() {
            fs::create_dir_all(dir).map_err(|e| e.to_string())?;
        }
        let example=ToolConfig{tools:vec![UserTool{name:"Format selected JSON".into(),program:"python".into(),
            args: vec!["-X".into(),"utf8".into(),"-c".into(),"import json, sys; sys.stdout.write(json.dumps(json.load(sys.stdin), indent=2, ensure_ascii=False))".into()],
            mode:ToolMode::Filter,shortcut:None}]};
        let mut file = match fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(path)
        {
            Ok(file) => file,
            Err(e) if e.kind() == io::ErrorKind::AlreadyExists => return Ok(()),
            Err(e) => return Err(e.to_string()),
        };
        serde_json::to_writer_pretty(&mut file, &example).map_err(|e| e.to_string())
    }
}
pub type ToolResult = BuildReport;

pub fn default_build(root: &Path) -> Option<String> {
    if root.join("Cargo.toml").is_file() {
        return Some("cargo check --message-format short".into());
    }
    if root.join("CMakeLists.txt").is_file() && root.join("build").is_dir() {
        return Some("cmake --build build".into());
    }
    if let Ok(data) = fs::read(root.join("package.json")) {
        if serde_json::from_slice::<serde_json::Value>(&data)
            .ok()
            .and_then(|v| v.get("scripts")?.get("build")?.as_str().map(str::to_owned))
            .is_some()
        {
            return Some("npm run build".into());
        }
    }
    if root.join("go.mod").is_file() {
        return Some("go build ./...".into());
    }
    if root.join("pyproject.toml").is_file() {
        return Some("python -m compileall -q .".into());
    }
    None
}
pub fn build(root: &Path, command: &str, cancel: &AtomicBool) -> Result<BuildReport, String> {
    if command.trim().is_empty() {
        return Err("Enter a build command.".into());
    }
    #[cfg(windows)]
    let mut process = {
        use std::os::windows::process::CommandExt;
        let mut c = Command::new(std::env::var_os("COMSPEC").unwrap_or_else(|| "cmd.exe".into()));
        c.args(["/d", "/s", "/c"]).raw_arg(format!("\"{command}\""));
        c
    };
    #[cfg(not(windows))]
    let mut process = {
        let mut c = Command::new("sh");
        c.args(["-c", command]);
        c
    };
    run_process(&mut process, root, None, false, cancel)
}
pub fn run_tool(
    tool: &UserTool,
    root: &Path,
    file: Option<&Path>,
    selected: &str,
    cancel: &AtomicBool,
) -> Result<ToolResult, String> {
    if tool.program.trim().is_empty() {
        return Err("This tool needs a program.".into());
    }
    let folder = root.to_string_lossy();
    let path = file.map(|p| p.to_string_lossy()).unwrap_or_default();
    let filename = file
        .and_then(|p| p.file_name())
        .map(|s| s.to_string_lossy())
        .unwrap_or_default();
    let expand = |s: &str| {
        s.replace("{folder}", &folder)
            .replace("{file}", &path)
            .replace("{filename}", &filename)
    };
    let mut cmd = Command::new(expand(&tool.program));
    cmd.args(tool.args.iter().map(|s| expand(s)));
    run_process(
        &mut cmd,
        root,
        Some(selected.as_bytes().to_owned()),
        tool.mode == ToolMode::Filter,
        cancel,
    )
}

#[cfg(windows)]
struct Family(windows_sys::Win32::Foundation::HANDLE);
#[cfg(windows)]
impl Family {
    fn attach(child: &Child) -> io::Result<Self> {
        use std::os::windows::io::AsRawHandle;
        use windows_sys::Win32::{
            Foundation::{CloseHandle, FALSE},
            System::JobObjects::*,
        };
        unsafe {
            let job = CreateJobObjectW(std::ptr::null(), std::ptr::null());
            if job.is_null() {
                return Err(io::Error::last_os_error());
            }
            let mut info: JOBOBJECT_EXTENDED_LIMIT_INFORMATION = std::mem::zeroed();
            info.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
            if SetInformationJobObject(
                job,
                JobObjectExtendedLimitInformation,
                &info as *const _ as _,
                std::mem::size_of_val(&info) as u32,
            ) == FALSE
                || AssignProcessToJobObject(job, child.as_raw_handle() as _) == FALSE
            {
                let e = io::Error::last_os_error();
                CloseHandle(job);
                return Err(e);
            }
            Ok(Self(job))
        }
    }
    fn cancel(&self) {
        unsafe {
            windows_sys::Win32::System::JobObjects::TerminateJobObject(self.0, 1);
        }
    }
}
#[cfg(windows)]
impl Drop for Family {
    fn drop(&mut self) {
        unsafe {
            windows_sys::Win32::Foundation::CloseHandle(self.0);
        }
    }
}

fn run_process(
    cmd: &mut Command,
    root: &Path,
    input: Option<Vec<u8>>,
    filter: bool,
    cancel: &AtomicBool,
) -> Result<BuildReport, String> {
    const LIMIT: usize = 16 * 1024 * 1024;
    if cancel.load(Ordering::Relaxed) {
        return Ok(BuildReport {
            cancelled: true,
            ..Default::default()
        });
    }
    cmd.current_dir(root)
        .stdin(if input.is_some() {
            Stdio::piped()
        } else {
            Stdio::null()
        })
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        cmd.creation_flags(windows_sys::Win32::System::Threading::CREATE_NO_WINDOW);
    }
    let mut child = cmd
        .spawn()
        .map_err(|e| format!("Could not start command: {e}"))?;
    #[cfg(windows)]
    let family = Family::attach(&child).ok(); // Some managed Windows jobs forbid nested job assignment.
    if let (Some(bytes), Some(mut stdin)) = (input, child.stdin.take()) {
        thread::spawn(move || {
            let _ = stdin.write_all(&bytes);
        });
    }
    let out = child.stdout.take().unwrap();
    let err = child.stderr.take().unwrap();
    let (tx, rx) = mpsc::sync_channel(32);
    fn pump(
        mut input: impl Read + Send + 'static,
        stderr: bool,
        tx: mpsc::SyncSender<(bool, Vec<u8>)>,
    ) {
        thread::spawn(move || {
            let mut buf = [0u8; 8192];
            loop {
                match input.read(&mut buf) {
                    Ok(0) | Err(_) => break,
                    Ok(n) => {
                        if tx.send((stderr, buf[..n].into())).is_err() {
                            break;
                        }
                    }
                }
            }
        });
    }
    pump(out, false, tx.clone());
    pump(err, true, tx);
    let mut output = Vec::new();
    let mut errors = Vec::new();
    let mut overflow = false;
    let mut cancelled = false;
    let mut exited_at = None;
    let mut code = None;
    let mut pipes_closed = false;
    loop {
        if !cancelled && cancel.load(Ordering::Relaxed) {
            cancelled = true;
            #[cfg(windows)]
            if let Some(job) = &family {
                job.cancel();
            }
            let _ = child.kill();
        }
        if !pipes_closed {
            match rx.recv_timeout(Duration::from_millis(25)) {
                Ok((is_err, b)) => {
                    let dst = if filter && is_err {
                        &mut errors
                    } else {
                        &mut output
                    };
                    let room = LIMIT.saturating_sub(dst.len());
                    dst.extend_from_slice(&b[..room.min(b.len())]);
                    overflow |= room < b.len();
                }
                Err(mpsc::RecvTimeoutError::Timeout) => {}
                Err(mpsc::RecvTimeoutError::Disconnected) => pipes_closed = true,
            }
        } else if code.is_none() {
            thread::sleep(Duration::from_millis(20));
        }
        if code.is_none() {
            code = child.try_wait().map_err(|e| e.to_string())?;
        }
        if code.is_some() {
            // Detached grandchildren may have inherited a pipe handle.
            let t = exited_at.get_or_insert_with(std::time::Instant::now);
            if pipes_closed || t.elapsed() > Duration::from_secs(2) {
                break;
            }
        }
    }
    let success = code.is_some_and(|s| s.success()) && !cancelled && (!filter || !overflow);
    if filter && !success && !errors.is_empty() {
        output = errors;
    }
    if filter && success && std::str::from_utf8(&output).is_err() {
        return Err("Selection filter returned non-UTF-8 text.".into());
    }
    let mut output = String::from_utf8_lossy(&output).into_owned();
    if overflow {
        output.push_str("\n[Output exceeded 16 MiB]");
    }
    let diagnostics = diagnostics(root, &output);
    Ok(BuildReport {
        output,
        success,
        diagnostics,
        cancelled,
    })
}

pub fn diagnostics(root: &Path, output: &str) -> Vec<Diagnostic> {
    static PATTERNS: OnceLock<[Regex; 4]> = OnceLock::new();
    let patterns = PATTERNS.get_or_init(|| {
        [
            // GCC, Clang, Cargo short, Go, and rustc's arrow location.
            Regex::new(r"^(?:-->\s*)?(.+?):(\d+):(\d+)(?:\s*:\s*|\s+)?(.*)$").unwrap(),
            Regex::new(r"^(.+?)\((\d+),(\d+)\)\s*:\s*(.*)$").unwrap(),
            Regex::new(r"^(.+?)\((\d+)\)\s*:\s*(.*)$").unwrap(),
            Regex::new(r#"^File "(.+?)", line (\d+)(?:, (.*))?$"#).unwrap(),
        ]
    });
    static ANSI: OnceLock<Regex> = OnceLock::new();
    let ansi = ANSI.get_or_init(|| Regex::new(r"\x1b\[[0-9;]*[A-Za-z]").unwrap());
    let mut found = Vec::new();
    let mut recent = String::new();
    for line in output.lines() {
        let line = ansi.replace_all(line.trim(), "");
        if line.starts_with("error") || line.starts_with("warning") {
            recent = line.to_string();
        }
        for (idx, pattern) in patterns.iter().enumerate() {
            if let Some(m) = pattern.captures(&line) {
                let file = Path::new(&m[1]);
                if idx == 0
                    && file.extension().is_none()
                    && file.components().count() < 2
                    && !root.join(file).is_file()
                {
                    continue;
                }
                let path = if file.is_absolute() {
                    file.into()
                } else {
                    root.join(file)
                };
                let lineno = m[2].parse::<u64>().unwrap_or(1).max(1);
                let (column, msg) = if idx <= 1 {
                    (m[3].parse::<u64>().unwrap_or(1).max(1), m.get(4))
                } else {
                    (1, m.get(3))
                };
                let message = msg
                    .map(|v| v.as_str())
                    .filter(|s| !s.is_empty())
                    .unwrap_or(&recent)
                    .to_owned();
                found.push(Diagnostic {
                    path,
                    line: lineno,
                    column,
                    message,
                });
                break;
            }
        }
    }
    found
}

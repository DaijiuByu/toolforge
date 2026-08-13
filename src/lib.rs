//! Policy-controlled tools for coding agents.
//!
//! The library intentionally exposes a small surface: reading files, searching
//! a workspace, and running an explicitly allow-listed test command. The CLI
//! wraps the same library with a JSONL protocol.

use serde::{Deserialize, Serialize};
use std::collections::BTreeSet;
use std::fs::{self, File, OpenOptions};
use std::io::{self, BufRead, BufReader, Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

const DEFAULT_MAX_OUTPUT_BYTES: usize = 32 * 1024;
const DEFAULT_MAX_FILE_BYTES: u64 = 256 * 1024;

#[derive(Debug, Clone, Deserialize)]
#[serde(tag = "tool", rename_all = "snake_case")]
pub enum ToolRequest {
    ListFiles {
        path: Option<String>,
        max_results: Option<usize>,
    },
    ReadFile {
        path: String,
    },
    SearchCode {
        query: String,
        path: Option<String>,
        max_results: Option<usize>,
    },
    RunTest {
        command: Vec<String>,
        timeout_ms: Option<u64>,
    },
}

impl ToolRequest {
    pub fn name(&self) -> &'static str {
        match self {
            Self::ListFiles { .. } => "list_files",
            Self::ReadFile { .. } => "read_file",
            Self::SearchCode { .. } => "search_code",
            Self::RunTest { .. } => "run_test",
        }
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct ToolResponse {
    pub ok: bool,
    pub tool: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub result: Option<serde_json::Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    pub duration_ms: u128,
}

impl ToolResponse {
    fn success(tool: &str, result: serde_json::Value, started: Instant) -> Self {
        Self {
            ok: true,
            tool: tool.to_owned(),
            result: Some(result),
            error: None,
            duration_ms: started.elapsed().as_millis(),
        }
    }

    fn failure(tool: &str, error: impl Into<String>, started: Instant) -> Self {
        Self {
            ok: false,
            tool: tool.to_owned(),
            result: None,
            error: Some(error.into()),
            duration_ms: started.elapsed().as_millis(),
        }
    }
}

#[derive(Debug, Clone)]
pub struct Policy {
    pub workspace: PathBuf,
    pub allowed_commands: BTreeSet<String>,
    pub max_calls: usize,
    pub max_output_bytes: usize,
    pub max_file_bytes: u64,
    pub max_runtime: Duration,
}

impl Policy {
    pub fn new(workspace: impl AsRef<Path>) -> io::Result<Self> {
        let workspace = fs::canonicalize(workspace)?;
        if !workspace.is_dir() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "workspace must be a directory",
            ));
        }

        Ok(Self {
            workspace,
            allowed_commands: [
                "cargo", "pytest", "python", "python3", "npm", "go", "dotnet",
            ]
            .into_iter()
            .map(str::to_owned)
            .collect(),
            max_calls: 24,
            max_output_bytes: DEFAULT_MAX_OUTPUT_BYTES,
            max_file_bytes: DEFAULT_MAX_FILE_BYTES,
            max_runtime: Duration::from_secs(30),
        })
    }

    fn resolve_existing_path(&self, requested: &str) -> Result<PathBuf, String> {
        if requested.trim().is_empty() {
            return Err("path must not be empty".to_owned());
        }

        let candidate = self.workspace.join(requested);
        let resolved = fs::canonicalize(&candidate)
            .map_err(|_| format!("path does not exist: {requested}"))?;
        if !resolved.starts_with(&self.workspace) {
            return Err("path escapes the configured workspace".to_owned());
        }
        Ok(resolved)
    }

    fn resolve_directory(&self, requested: Option<&str>) -> Result<PathBuf, String> {
        let path = requested.unwrap_or(".");
        let resolved = self.resolve_existing_path(path)?;
        if !resolved.is_dir() {
            return Err(format!("not a directory: {path}"));
        }
        Ok(resolved)
    }
}

pub struct Executor {
    policy: Policy,
    calls: usize,
    audit: Option<File>,
}

impl Executor {
    pub fn new(policy: Policy, audit_path: Option<&Path>) -> io::Result<Self> {
        let audit = match audit_path {
            Some(path) => Some(OpenOptions::new().create(true).append(true).open(path)?),
            None => None,
        };
        Ok(Self {
            policy,
            calls: 0,
            audit,
        })
    }

    pub fn execute(&mut self, request: ToolRequest) -> ToolResponse {
        let started = Instant::now();
        let tool = request.name();
        if self.calls >= self.policy.max_calls {
            return ToolResponse::failure(tool, "call budget exceeded", started);
        }
        self.calls += 1;

        let response = match request {
            ToolRequest::ListFiles { path, max_results } => {
                self.list_files(path.as_deref(), max_results)
            }
            ToolRequest::ReadFile { path } => self.read_file(&path),
            ToolRequest::SearchCode {
                query,
                path,
                max_results,
            } => self.search_code(&query, path.as_deref(), max_results),
            ToolRequest::RunTest {
                command,
                timeout_ms,
            } => self.run_test(&command, timeout_ms),
        };

        let result = match response {
            Ok(value) => ToolResponse::success(tool, value, started),
            Err(error) => ToolResponse::failure(tool, error, started),
        };
        self.write_audit(tool, &result);
        result
    }

    fn list_files(
        &self,
        requested: Option<&str>,
        max_results: Option<usize>,
    ) -> Result<serde_json::Value, String> {
        let root = self.policy.resolve_directory(requested)?;
        let limit = max_results.unwrap_or(200).min(1_000);
        let mut files = Vec::new();
        collect_files(&root, &self.policy.workspace, &mut files, limit)?;
        Ok(serde_json::json!({ "files": files, "truncated": files.len() >= limit }))
    }

    fn read_file(&self, requested: &str) -> Result<serde_json::Value, String> {
        let path = self.policy.resolve_existing_path(requested)?;
        if !path.is_file() {
            return Err(format!("not a file: {requested}"));
        }
        let metadata = fs::metadata(&path).map_err(|e| e.to_string())?;
        if metadata.len() > self.policy.max_file_bytes {
            return Err(format!(
                "file is larger than the {} byte limit",
                self.policy.max_file_bytes
            ));
        }
        let content =
            fs::read_to_string(&path).map_err(|_| "file is not valid UTF-8 text".to_owned())?;
        Ok(serde_json::json!({
            "path": relative_display(&path, &self.policy.workspace),
            "content": content
        }))
    }

    fn search_code(
        &self,
        query: &str,
        requested: Option<&str>,
        max_results: Option<usize>,
    ) -> Result<serde_json::Value, String> {
        if query.trim().is_empty() {
            return Err("query must not be empty".to_owned());
        }
        let root = self.policy.resolve_directory(requested)?;
        let limit = max_results.unwrap_or(50).min(500);
        let mut files = Vec::new();
        collect_files(&root, &self.policy.workspace, &mut files, 10_000)?;
        let mut matches = Vec::new();
        for relative in files {
            if matches.len() >= limit {
                break;
            }
            let absolute = self.policy.workspace.join(&relative);
            let Ok(metadata) = fs::metadata(&absolute) else {
                continue;
            };
            if metadata.len() > self.policy.max_file_bytes {
                continue;
            }
            let Ok(content) = fs::read_to_string(&absolute) else {
                continue;
            };
            for (line_number, line) in content.lines().enumerate() {
                if line.contains(query) {
                    matches.push(serde_json::json!({
                        "path": relative,
                        "line": line_number + 1,
                        "text": truncate(line, self.policy.max_output_bytes.min(1_000))
                    }));
                    if matches.len() >= limit {
                        break;
                    }
                }
            }
        }
        Ok(serde_json::json!({ "matches": matches, "truncated": matches.len() >= limit }))
    }

    fn run_test(
        &self,
        command: &[String],
        timeout_ms: Option<u64>,
    ) -> Result<serde_json::Value, String> {
        let Some(program) = command.first() else {
            return Err("command must contain an executable".to_owned());
        };
        if !self.policy.allowed_commands.contains(program) {
            return Err(format!("command is not allow-listed: {program}"));
        }
        if command.iter().any(|arg| arg.contains('\0')) {
            return Err("command contains a NUL byte".to_owned());
        }

        let requested_timeout_ms = timeout_ms.unwrap_or_else(|| {
            self.policy
                .max_runtime
                .as_millis()
                .try_into()
                .unwrap_or(u64::MAX)
        });
        let policy_timeout_ms = self
            .policy
            .max_runtime
            .as_millis()
            .try_into()
            .unwrap_or(u64::MAX);
        let timeout = Duration::from_millis(requested_timeout_ms.min(policy_timeout_ms));
        let mut child = Command::new(program)
            .args(&command[1..])
            .current_dir(&self.policy.workspace)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .map_err(|e| format!("failed to start {program}: {e}"))?;
        let stdout = child
            .stdout
            .take()
            .ok_or_else(|| "failed to capture command stdout".to_owned())?;
        let stderr = child
            .stderr
            .take()
            .ok_or_else(|| "failed to capture command stderr".to_owned())?;
        let output_limit = self.policy.max_output_bytes;
        let stdout_reader = std::thread::spawn(move || read_limited(stdout, output_limit));
        let stderr_reader = std::thread::spawn(move || read_limited(stderr, output_limit));
        let started = Instant::now();
        let status = loop {
            match child.try_wait().map_err(|e| e.to_string())? {
                Some(status) => break status,
                None if started.elapsed() >= timeout => {
                    let _ = child.kill();
                    let _ = child.wait();
                    let _ = stdout_reader.join();
                    let _ = stderr_reader.join();
                    return Err(format!(
                        "command timed out after {} ms",
                        timeout.as_millis()
                    ));
                }
                None => std::thread::sleep(Duration::from_millis(10)),
            }
        };
        let stdout = truncate(
            &String::from_utf8_lossy(
                &stdout_reader
                    .join()
                    .map_err(|_| "stdout reader panicked".to_owned())?
                    .map_err(|e| format!("failed to collect command stdout: {e}"))?,
            ),
            self.policy.max_output_bytes,
        );
        let stderr = truncate(
            &String::from_utf8_lossy(
                &stderr_reader
                    .join()
                    .map_err(|_| "stderr reader panicked".to_owned())?
                    .map_err(|e| format!("failed to collect command stderr: {e}"))?,
            ),
            self.policy.max_output_bytes,
        );
        Ok(serde_json::json!({
            "command": command,
            "exit_code": status.code(),
            "success": status.success(),
            "stdout": stdout,
            "stderr": stderr,
            "timed_out": false
        }))
    }

    fn write_audit(&mut self, tool: &str, response: &ToolResponse) {
        let Some(audit) = self.audit.as_mut() else {
            return;
        };
        let record = serde_json::json!({
            "timestamp_ms": SystemTime::now().duration_since(UNIX_EPOCH).unwrap_or_default().as_millis(),
            "call_number": self.calls,
            "tool": tool,
            "ok": response.ok,
            "duration_ms": response.duration_ms,
            "error": response.error
        });
        let _ = writeln!(audit, "{}", record);
        let _ = audit.flush();
    }
}

pub fn serve_jsonl<R: io::Read, W: Write>(
    reader: R,
    mut writer: W,
    executor: &mut Executor,
) -> io::Result<()> {
    for line in BufReader::new(reader).lines() {
        let line = line?;
        if line.trim().is_empty() {
            continue;
        }
        let response = match serde_json::from_str::<ToolRequest>(&line) {
            Ok(request) => executor.execute(request),
            Err(error) => ToolResponse::failure(
                "unknown",
                format!("invalid request: {error}"),
                Instant::now(),
            ),
        };
        serde_json::to_writer(&mut writer, &response)?;
        writeln!(writer)?;
        writer.flush()?;
    }
    Ok(())
}

fn collect_files(
    root: &Path,
    workspace: &Path,
    output: &mut Vec<String>,
    limit: usize,
) -> Result<(), String> {
    if output.len() >= limit {
        return Ok(());
    }
    let entries = fs::read_dir(root).map_err(|e| e.to_string())?;
    for entry in entries.flatten() {
        if output.len() >= limit {
            break;
        }
        let path = entry.path();
        let name = entry.file_name().to_string_lossy().to_string();
        if name.starts_with('.')
            || name == "target"
            || name == "node_modules"
            || name == "__pycache__"
        {
            continue;
        }
        let metadata = match fs::symlink_metadata(&path) {
            Ok(value) => value,
            Err(_) => continue,
        };
        if metadata.file_type().is_symlink() {
            continue;
        }
        if metadata.is_dir() {
            collect_files(&path, workspace, output, limit)?;
        } else if metadata.is_file() {
            output.push(relative_display(&path, workspace));
        }
    }
    Ok(())
}

fn relative_display(path: &Path, workspace: &Path) -> String {
    path.strip_prefix(workspace)
        .unwrap_or(path)
        .to_string_lossy()
        .replace('\\', "/")
}

fn truncate(value: &str, max_bytes: usize) -> String {
    if value.len() <= max_bytes {
        return value.to_owned();
    }
    let mut end = max_bytes.saturating_sub("\n...[truncated]".len());
    while end > 0 && !value.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}\n...[truncated]", &value[..end])
}

fn read_limited<R: Read>(mut reader: R, limit: usize) -> io::Result<Vec<u8>> {
    let mut output = Vec::with_capacity(limit.saturating_add(1));
    let mut buffer = [0_u8; 8 * 1024];
    loop {
        let read = reader.read(&mut buffer)?;
        if read == 0 {
            break;
        }
        if output.len() < limit.saturating_add(1) {
            let keep = read.min(limit.saturating_add(1) - output.len());
            output.extend_from_slice(&buffer[..keep]);
        }
    }
    Ok(output)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;
    use tempfile::tempdir;

    fn executor() -> (tempfile::TempDir, Executor) {
        let dir = tempdir().unwrap();
        fs::write(dir.path().join("main.py"), "print('hello')\nneedle = 1\n").unwrap();
        let policy = Policy::new(dir.path()).unwrap();
        (dir, Executor::new(policy, None).unwrap())
    }

    #[test]
    fn reads_and_searches_only_inside_workspace() {
        let (dir, mut executor) = executor();
        let read = executor.execute(ToolRequest::ReadFile {
            path: "main.py".into(),
        });
        assert!(read.ok);
        let search = executor.execute(ToolRequest::SearchCode {
            query: "needle".into(),
            path: None,
            max_results: None,
        });
        assert!(search.ok);
        let escape = executor.execute(ToolRequest::ReadFile {
            path: "../outside".into(),
        });
        assert!(!escape.ok);
        assert!(dir.path().join("main.py").exists());
    }

    #[test]
    fn rejects_unapproved_commands() {
        let (_dir, mut executor) = executor();
        let response = executor.execute(ToolRequest::RunTest {
            command: vec!["sh".into(), "-c".into(), "echo unsafe".into()],
            timeout_ms: Some(100),
        });
        assert!(!response.ok);
        assert!(response.error.unwrap().contains("allow-listed"));
    }

    #[test]
    fn bounds_command_output_and_timeout() {
        let (_dir, mut executor) = executor();
        executor.policy.max_output_bytes = 32;
        let output = executor.execute(ToolRequest::RunTest {
            command: vec!["python".into(), "-c".into(), "print('x' * 1000)".into()],
            timeout_ms: Some(1_000),
        });
        assert!(output.ok);
        let output_result = output.result.unwrap();
        let stdout = output_result["stdout"].as_str().unwrap();
        assert!(stdout.contains("truncated"));

        let timeout = executor.execute(ToolRequest::RunTest {
            command: vec![
                "python".into(),
                "-c".into(),
                "import time; time.sleep(1)".into(),
            ],
            timeout_ms: Some(20),
        });
        assert!(!timeout.ok);
        assert!(timeout.error.unwrap().contains("timed out"));
    }

    #[test]
    fn serves_jsonl_and_enforces_call_budget() {
        let (_dir, mut executor) = executor();
        executor.policy.max_calls = 1;
        let input = r#"{"tool":"list_files"}
{"tool":"list_files"}
invalid
"#;
        let mut output = Vec::new();
        serve_jsonl(Cursor::new(input), &mut output, &mut executor).unwrap();
        let output_text = String::from_utf8(output).unwrap();
        let lines: Vec<_> = output_text.lines().collect();
        assert_eq!(lines.len(), 3);
        assert!(lines[0].contains("\"ok\":true"));
        assert!(lines[1].contains("call budget exceeded"));
        assert!(lines[2].contains("invalid request"));
    }
}

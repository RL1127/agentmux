//! 使用临时 Codex 目录验证 agentmux 二进制命令，不访问用户真实会话或配置。

use std::env;
use std::ffi::OsString;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

use serde_json::{Value, json};

const INTERACTIVE_ID: &str = "019f4a0b-a42e-7103-ac37-2ceffc73cb52";
const NON_INTERACTIVE_ID: &str = "019f4a0c-9f71-72e3-94bd-93d2a1080936";

/// 保存集成测试使用的临时 Codex 主目录和命令 PATH。
struct Fixture {
    directory: tempfile::TempDir,
    path_value: OsString,
}

impl Fixture {
    /// 返回临时 CODEX_HOME。
    fn home(&self) -> &Path {
        self.directory.path()
    }
}

/// 创建包含交互与非交互会话、配置、索引和模拟 CLI 的隔离环境。
fn fixture() -> Fixture {
    let directory = tempfile::tempdir().expect("应能创建临时 Codex 目录");
    let session_directory = directory.path().join("sessions/2026/07/10");
    let project_directory = directory.path().join("中文项目");
    let bin_directory = directory.path().join("bin");
    fs::create_dir_all(&session_directory).expect("应能创建会话目录");
    fs::create_dir_all(&project_directory).expect("应能创建中文项目目录");
    fs::create_dir_all(&bin_directory).expect("应能创建模拟 CLI 目录");

    write_session(
        &session_directory,
        INTERACTIVE_ID,
        &project_directory,
        "vscode",
        "user",
    );
    write_session(
        &session_directory,
        NON_INTERACTIVE_ID,
        &project_directory,
        "exec",
        "subagent",
    );
    let index = [
        json!({
            "id": INTERACTIVE_ID,
            "thread_name": "中文交互会话",
            "updated_at": "2026-07-10T04:00:00Z"
        }),
        json!({
            "id": NON_INTERACTIVE_ID,
            "thread_name": "后台会话",
            "updated_at": "2026-07-10T03:00:00Z"
        }),
    ]
    .into_iter()
    .map(|record| serde_json::to_string(&record).expect("索引 JSON 应可序列化"))
    .collect::<Vec<_>>()
    .join("\n");
    write_utf8(
        &directory.path().join("session_index.jsonl"),
        &(index + "\n"),
    );
    write_utf8(
        &directory.path().join("config.toml"),
        concat!(
            "model_provider = \"custom\"\n",
            "[model_providers.custom]\n",
            "name = \"Custom\"\n",
            "base_url = \"https://example.invalid/v1\"\n",
            "wire_api = \"responses\"\n"
        ),
    );
    create_fake_codex(&bin_directory);

    let mut paths = vec![bin_directory];
    if let Some(current_path) = env::var_os("PATH") {
        paths.extend(env::split_paths(&current_path));
    }
    let path_value = env::join_paths(paths).expect("测试 PATH 应可构造");
    Fixture {
        directory,
        path_value,
    }
}

/// 写入一条结构化 Codex JSONL 会话。
fn write_session(
    session_directory: &Path,
    id: &str,
    cwd: &Path,
    source: &str,
    thread_source: &str,
) {
    let records = [
        json!({
            "timestamp": "2026-07-10T03:00:00Z",
            "type": "session_meta",
            "payload": {
                "id": id,
                "timestamp": "2026-07-10T03:00:00Z",
                "cwd": cwd,
                "source": source,
                "thread_source": thread_source,
                "model_provider": "custom"
            }
        }),
        json!({
            "timestamp": "2026-07-10T03:30:00Z",
            "type": "turn_context",
            "payload": {
                "model": "gpt-5",
                "summary": "安全摘要 token=test-secret"
            }
        }),
    ];
    let body = records
        .into_iter()
        .map(|record| serde_json::to_string(&record).expect("会话 JSON 应可序列化"))
        .collect::<Vec<_>>()
        .join("\n");
    let path = session_directory.join(format!("rollout-2026-07-10T03-00-00-{id}.jsonl"));
    write_utf8(&path, &(body + "\n"));
}

/// 以 UTF-8 无 BOM 写入测试文本。
fn write_utf8(path: &Path, content: &str) {
    fs::write(path, content.as_bytes()).expect("应能写入 UTF-8 测试文件");
}

/// 创建始终成功的模拟 Codex CLI，仅用于可用性检查和 dry-run 前置条件。
fn create_fake_codex(directory: &Path) -> PathBuf {
    #[cfg(windows)]
    {
        let path = directory.join("codex.cmd");
        write_utf8(&path, "@echo off\r\nexit /b 0\r\n");
        path
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;

        let path = directory.join("codex");
        write_utf8(&path, "#!/bin/sh\nexit 0\n");
        let mut permissions = fs::metadata(&path)
            .expect("应能读取模拟 CLI 权限")
            .permissions();
        permissions.set_mode(0o755);
        fs::set_permissions(&path, permissions).expect("应能设置模拟 CLI 执行权限");
        path
    }
}

/// 创建指向当前测试构建产物的 agentmux 命令。
fn agentmux(fixture: &Fixture) -> Command {
    let mut command = Command::new(env!("CARGO_BIN_EXE_agentmux"));
    command
        .env("CODEX_HOME", fixture.home())
        .env("PATH", &fixture.path_value);
    command
}

/// 将子进程 stdout 按严格 UTF-8 解码。
fn stdout(output: &Output) -> String {
    String::from_utf8(output.stdout.clone()).expect("stdout 应为 UTF-8")
}

/// 将子进程 stderr 按严格 UTF-8 解码。
fn stderr(output: &Output) -> String {
    String::from_utf8(output.stderr.clone()).expect("stderr 应为 UTF-8")
}

/// 验证 JSON list 默认排除非交互会话，并保持摘要脱敏。
#[test]
fn list_json_filters_non_interactive_by_default() {
    let fixture = fixture();
    let output = agentmux(&fixture)
        .args(["list", "--json"])
        .output()
        .expect("list 命令应能执行");
    assert!(output.status.success(), "{}", stderr(&output));

    let value: Value = serde_json::from_slice(&output.stdout).expect("list 应输出合法 JSON");
    let sessions = value["groups"][0]["sessions"]
        .as_array()
        .expect("JSON 应包含会话数组");
    assert_eq!(sessions.len(), 1);
    assert_eq!(sessions[0]["id"], INTERACTIVE_ID);
    assert_eq!(sessions[0]["project"], "中文项目");
    assert_eq!(sessions[0]["summary"], "安全摘要 token=[REDACTED]");
}

/// 验证 include-non-interactive 会显示后台会话。
#[test]
fn list_includes_non_interactive_when_requested() {
    let fixture = fixture();
    let output = agentmux(&fixture)
        .args(["list", "--json", "--include-non-interactive"])
        .output()
        .expect("list 命令应能执行");
    assert!(output.status.success(), "{}", stderr(&output));
    let value: Value = serde_json::from_slice(&output.stdout).expect("list 应输出合法 JSON");
    let count = value["groups"]
        .as_array()
        .expect("JSON 应包含分组")
        .iter()
        .flat_map(|group| group["sessions"].as_array().into_iter().flatten())
        .count();
    assert_eq!(count, 2);
}

/// 验证 resume dry-run 输出官方命令且不要求真实 TTY。
#[test]
fn resume_dry_run_prints_official_command() {
    let fixture = fixture();
    let output = agentmux(&fixture)
        .args(["resume", INTERACTIVE_ID, "--dry-run"])
        .output()
        .expect("resume dry-run 应能执行");
    assert!(output.status.success(), "{}", stderr(&output));
    assert_eq!(
        stdout(&output).trim(),
        format!(
            "将执行: codex resume {INTERACTIVE_ID}\n恢复成功后将打开: codex://threads/{INTERACTIVE_ID}"
        )
    );
}

/// 验证全局关闭参数会让 dry-run 只展示官方恢复命令。
#[test]
fn resume_can_disable_app_navigation() {
    let fixture = fixture();
    let output = agentmux(&fixture)
        .args(["resume", INTERACTIVE_ID, "--dry-run", "--no-open-in-app"])
        .output()
        .expect("resume dry-run 应能执行");
    assert!(output.status.success(), "{}", stderr(&output));
    assert_eq!(
        stdout(&output).trim(),
        format!("将执行: codex resume {INTERACTIVE_ID}")
    );
}

/// 验证不存在的会话返回失败而不启动模拟 Codex。
#[test]
fn resume_rejects_missing_session() {
    let fixture = fixture();
    let output = agentmux(&fixture)
        .args([
            "resume",
            "00000000-0000-0000-0000-000000000000",
            "--dry-run",
        ])
        .output()
        .expect("resume 命令应能执行");
    assert!(!output.status.success());
    assert!(stderr(&output).contains("未找到会话"));
}

/// 验证 doctor、sources 和 completion 在隔离环境中均可执行。
#[test]
fn doctor_sources_and_completion_are_available() {
    let fixture = fixture();
    let doctor = agentmux(&fixture)
        .args(["doctor", "--json"])
        .output()
        .expect("doctor 应能执行");
    assert!(doctor.status.success(), "{}", stderr(&doctor));
    let diagnostics: Value =
        serde_json::from_slice(&doctor.stdout).expect("doctor 应输出合法 JSON");
    assert_eq!(diagnostics.as_array().map(Vec::len), Some(4));

    let sources = agentmux(&fixture)
        .args(["sources", "--json"])
        .output()
        .expect("sources 应能执行");
    assert!(sources.status.success(), "{}", stderr(&sources));
    let source_value: Value =
        serde_json::from_slice(&sources.stdout).expect("sources 应输出合法 JSON");
    assert_eq!(source_value[0]["id"], "codex");

    let completion = agentmux(&fixture)
        .args(["completion", "bash"])
        .output()
        .expect("completion 应能执行");
    assert!(completion.status.success(), "{}", stderr(&completion));
    assert!(stdout(&completion).contains("_agentmux"));
}

/// 验证 clap 拒绝同时请求配置修复和 dry-run。
#[test]
fn repair_provider_conflicts_with_dry_run() {
    let fixture = fixture();
    let output = agentmux(&fixture)
        .args(["resume", INTERACTIVE_ID, "--dry-run", "--repair-provider"])
        .output()
        .expect("参数校验应能执行");
    assert!(!output.status.success());
    assert!(stderr(&output).contains("cannot be used with"));
}

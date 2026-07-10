//! 负责在真实终端中启动来源官方恢复命令并传递退出码。

use std::env;
use std::ffi::OsString;
use std::io::{self, IsTerminal};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use thiserror::Error;

use crate::domain::CommandSpec;

/// 表示恢复执行前或启动进程时的结构化错误。
#[derive(Debug, Error)]
pub enum ResumeError {
    /// 当前标准流不是交互终端，无法承载 Codex TUI。
    #[error(
        "当前环境不是交互式终端，无法恢复 Codex 会话；请在 PowerShell、CMD 或 Windows Terminal 中运行"
    )]
    NonInteractiveTerminal,
    /// 命令名称或显式路径无法解析。
    #[error("找不到恢复命令: {program}")]
    CommandUnavailable {
        /// 未找到的程序名称。
        program: String,
    },
    /// 子进程创建或等待失败。
    #[error("无法启动恢复命令 {program}: {source}")]
    Spawn {
        /// 命令显示名称。
        program: String,
        /// 底层进程错误。
        #[source]
        source: io::Error,
    },
    /// 子进程被信号或平台异常终止，没有可传递退出码。
    #[error("恢复命令异常终止，未返回退出码")]
    MissingExitCode,
}

/// 汇总恢复命令是否执行及其退出码。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResumeExecution {
    /// 供 dry-run 和日志展示的安全命令文本。
    pub command: String,
    /// 是否实际创建了子进程。
    pub executed: bool,
    /// 应原样传递给 agentmux 调用方的退出码。
    pub exit_code: i32,
}

/// 执行恢复命令；dry-run 不要求 TTY，也不会创建子进程。
pub fn execute(spec: &CommandSpec, dry_run: bool) -> Result<ResumeExecution, ResumeError> {
    execute_with_terminal_state(spec, dry_run, terminal_is_interactive())
}

/// 使用显式终端状态执行恢复，便于稳定测试非 TTY 拒绝行为。
fn execute_with_terminal_state(
    spec: &CommandSpec,
    dry_run: bool,
    interactive_terminal: bool,
) -> Result<ResumeExecution, ResumeError> {
    let display = spec.display();
    if dry_run {
        return Ok(ResumeExecution {
            command: display,
            executed: false,
            exit_code: 0,
        });
    }
    if !interactive_terminal {
        return Err(ResumeError::NonInteractiveTerminal);
    }
    let exit_code = run_inherited(spec)?;
    Ok(ResumeExecution {
        command: display,
        executed: true,
        exit_code,
    })
}

/// 判断标准输入、输出和错误流是否都连接到真实终端。
fn terminal_is_interactive() -> bool {
    io::stdin().is_terminal() && io::stdout().is_terminal() && io::stderr().is_terminal()
}

/// 使用继承的三个标准流启动子进程，并返回其精确退出码。
fn run_inherited(spec: &CommandSpec) -> Result<i32, ResumeError> {
    let mut command =
        command_for(&spec.program, &spec.args).ok_or_else(|| ResumeError::CommandUnavailable {
            program: spec.program.display().to_string(),
        })?;
    let status = command
        .stdin(Stdio::inherit())
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit())
        .status()
        .map_err(|source| ResumeError::Spawn {
            program: spec.program.display().to_string(),
            source,
        })?;
    status.code().ok_or(ResumeError::MissingExitCode)
}

/// 为 crate 内部调用方解析程序并创建保留参数边界的平台命令。
pub(crate) fn command_for(program: &Path, args: &[OsString]) -> Option<Command> {
    let resolved = resolve_program(program)?;
    Some(platform_command(&resolved, args))
}

/// 创建平台进程；Windows 批处理包装器由 cmd.exe 解释，其他程序直接执行。
fn platform_command(program: &Path, args: &[OsString]) -> Command {
    #[cfg(windows)]
    {
        let extension = program
            .extension()
            .and_then(|value| value.to_str())
            .unwrap_or_default();
        if extension.eq_ignore_ascii_case("cmd") || extension.eq_ignore_ascii_case("bat") {
            let mut command = Command::new("cmd.exe");
            command.arg("/D").arg("/S").arg("/C").arg(program);
            command.args(args);
            return command;
        }
        if extension.eq_ignore_ascii_case("ps1") {
            let mut command = Command::new("powershell.exe");
            command
                .arg("-NoLogo")
                .arg("-NoProfile")
                .arg("-File")
                .arg(program);
            command.args(args);
            return command;
        }
    }
    let mut command = Command::new(program);
    command.args(args);
    command
}

/// 从显式路径或 PATH 解析恢复程序，并按 PATHEXT 保持 Windows 命令优先级。
fn resolve_program(program: &Path) -> Option<PathBuf> {
    if program.components().count() > 1 || program.is_absolute() {
        return program.is_file().then(|| program.to_path_buf());
    }
    let path = env::var_os("PATH")?;
    let names = executable_names(program);
    env::split_paths(&path)
        .flat_map(|directory| names.iter().map(move |name| directory.join(name)))
        .find(|candidate| candidate.is_file())
}

/// 返回当前平台用于 PATH 搜索的程序候选名称。
fn executable_names(program: &Path) -> Vec<OsString> {
    let base = program.as_os_str().to_os_string();
    #[cfg(windows)]
    {
        if program.extension().is_some() {
            return vec![base];
        }
        let extensions = env::var_os("PATHEXT")
            .map(|value| {
                value
                    .to_string_lossy()
                    .split(';')
                    .filter(|item| !item.is_empty())
                    .map(ToOwned::to_owned)
                    .collect::<Vec<_>>()
            })
            .unwrap_or_else(|| {
                vec![
                    ".COM".to_owned(),
                    ".EXE".to_owned(),
                    ".BAT".to_owned(),
                    ".CMD".to_owned(),
                ]
            });
        extensions
            .into_iter()
            .map(|extension| {
                let mut name = base.clone();
                name.push(extension);
                name
            })
            .collect()
    }
    #[cfg(not(windows))]
    {
        vec![base]
    }
}

#[cfg(test)]
mod tests {
    use std::fs;

    use super::*;

    /// 验证 dry-run 在非 TTY 环境中仍只返回官方命令文本。
    #[test]
    fn dry_run_does_not_require_terminal() {
        let spec = CommandSpec::new(
            "codex",
            vec![OsString::from("resume"), OsString::from("session-id")],
        );
        let result =
            execute_with_terminal_state(&spec, true, false).expect("dry-run 应跳过 TTY 检查");
        assert!(!result.executed);
        assert_eq!(result.exit_code, 0);
        assert_eq!(result.command, "codex resume session-id");
    }

    /// 验证实际恢复在非 TTY 环境中被明确拒绝。
    #[test]
    fn rejects_non_interactive_terminal() {
        let spec = CommandSpec::new(
            "codex",
            vec![OsString::from("resume"), OsString::from("session-id")],
        );
        let error = execute_with_terminal_state(&spec, false, false).expect_err("非 TTY 应被拒绝");
        assert!(matches!(error, ResumeError::NonInteractiveTerminal));
    }

    /// 创建返回指定退出码的模拟可执行文件。
    fn fake_program(directory: &Path, exit_code: i32) -> PathBuf {
        #[cfg(windows)]
        {
            let path = directory.join("fake-codex.cmd");
            let body = format!("@echo off\r\nexit /b {exit_code}\r\n");
            fs::write(&path, body.as_bytes()).expect("应能写入 UTF-8 模拟批处理");
            path
        }
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;

            let path = directory.join("fake-codex");
            let body = format!("#!/bin/sh\nexit {exit_code}\n");
            fs::write(&path, body.as_bytes()).expect("应能写入 UTF-8 模拟脚本");
            let mut permissions = fs::metadata(&path)
                .expect("应能读取模拟脚本权限")
                .permissions();
            permissions.set_mode(0o755);
            fs::set_permissions(&path, permissions).expect("应能设置模拟脚本执行权限");
            path
        }
    }

    /// 验证模拟 Codex 子进程退出码被原样返回。
    #[test]
    fn passes_through_child_exit_code() {
        let directory = tempfile::tempdir().expect("应能创建临时目录");
        let program = fake_program(directory.path(), 37);
        let spec = CommandSpec::new(program, Vec::new());

        let exit_code = run_inherited(&spec).expect("模拟子进程应能执行");
        assert_eq!(exit_code, 37);
    }

    /// 验证 Windows 不会优先选择 npm 生成的无扩展名 shell shim。
    #[cfg(windows)]
    #[test]
    fn windows_candidates_exclude_extensionless_shim() {
        let candidates = executable_names(Path::new("codex"));

        assert!(!candidates.is_empty());
        assert!(
            candidates
                .iter()
                .all(|candidate| Path::new(candidate).extension().is_some())
        );
    }
}

//! Shell syntax and conservative read-only inspection policy.
//! The selected platform dialect owns these rules; tool/process code only
//! consumes the classification.

use crate::workspace_path::resolve_workspace_path;
use std::path::Path;

#[cfg(any(target_os = "linux", test))]
pub(super) fn bash_quote(value: &str) -> String {
    format!("'{}'", value.replace('\'', "'\\''"))
}

pub(super) fn process_description() -> &'static str {
    if cfg!(windows) {
        "Start a persistent development process (server, watcher, long build) in the workspace with cmd.exe. Returns a process id for exec_poll and exec_terminate. WORK mode only; policy and dangerous-command checks apply."
    } else {
        "Start a persistent development process (server, watcher, long build) in the workspace with Bash. Returns a process id for exec_poll and exec_terminate. WORK mode only; policy and dangerous-command checks apply."
    }
}

pub(super) fn shell_description() -> &'static str {
    if cfg!(windows) {
        "Run a bounded Windows cmd.exe developer command in the workspace. Use cmd syntax for pipelines, redirection, and command chaining. ASK/PLAN allow only conservative read-only commands and deny test/build execution."
    } else {
        "Run a bounded Bash developer command. Commands execute with the workspace as the working directory, so `cd <workspace> &&` is redundant — prefer plain `git log --oneline -20`. A `cd` into a subdirectory is allowed for read-only inspection (for example `cd src && rg normalize_username .`), but never `cd` out of the workspace. ASK/PLAN allow only conservative read-only commands and deny test/build execution."
    }
}

pub(super) fn shell_guidance() -> &'static str {
    if cfg!(windows) {
        "\nThe model-facing shell is native cmd.exe. Use Windows paths, `dir`/`type`, `&&` and `|`; use `>` for redirection. Use `rg`, `git`, `cargo`, `rustc`, `node`, `npm`, and `python` when installed. Bash syntax and PowerShell are not supported by this shell."
    } else {
        ""
    }
}

/// Executables whose semantics are not representable by the current
/// capability mapper. Keep this check on every host: a Bash command can
/// launch a Windows executable in a cross-platform workspace.
pub(super) fn executable_is_unmodelled(token: &str) -> bool {
    let name = token.rsplit(['/', '\\']).next().unwrap_or(token);
    matches!(
        name,
        "powershell.exe"
            | "pwsh.exe"
            | "cmd.exe"
            | "reg.exe"
            | "regedit.exe"
            | "sc.exe"
            | "net.exe"
            | "netsh.exe"
            | "schtasks.exe"
            | "wmic.exe"
            | "rundll32.exe"
            | "mshta.exe"
    )
}

/// Conservatively classifies a shell command as read-only.
///
/// Only simple inspection commands and compound commands built exclusively
/// from them are accepted. Any shell feature that could expand, substitute,
/// redirect, background, or nest is rejected, as is `||`. A `cd` into a
/// workspace-local subdirectory is accepted for compound read-only inspection,
/// but the target is resolved and normalized against the workspace root and
/// may never escape through `..`, an absolute path, or a symlink. This
/// deliberately keeps read-only classification stricter than the shell's
/// actual grammar so ASK/PLAN can never be used to mutate the workspace.
pub(crate) fn is_read_only_shell(command: &str, workspace: &Path) -> bool {
    let command = command.trim();
    #[cfg(windows)]
    if command.contains(['%', '^']) {
        return false;
    }
    #[cfg(windows)]
    if command.match_indices('~').any(|(index, _)| {
        let bytes = command.as_bytes();
        !bytes
            .get(index.wrapping_sub(1))
            .is_some_and(u8::is_ascii_alphanumeric)
            || !bytes.get(index + 1).is_some_and(u8::is_ascii_digit)
    }) {
        // NTFS 8.3 names such as RUNNER~1 are literal cmd.exe paths. A bare
        // tilde or other shell-looking spelling remains outside this grammar.
        return false;
    }
    if command.is_empty() || command.contains("||") {
        return false;
    }
    // `&&` is the only context where `&` is permitted; reject it everywhere
    // else (background jobs) along with expansion and redirection operators.
    let without_and = command.replace("&&", " ");
    if without_and.chars().any(|ch| {
        matches!(
            ch,
            '$' | '`'
                | '<'
                | '>'
                | '&'
                | '('
                | ')'
                | '\\'
                | '\n'
                | '\r'
                | '"'
                | '\''
                | '*'
                | '?'
                | '['
                | ']'
                | '{'
                | '}'
                | '!'
        ) || (ch == '~' && !cfg!(windows))
    }) {
        return false;
    }
    let Some(segments) = split_simple_commands(command) else {
        return false;
    };
    if !segments
        .iter()
        .any(|segment| segment.split_whitespace().next() == Some("cd"))
    {
        // No `cd`: workspace is irrelevant, keep the fast conservative path.
        return segments.iter().all(|segment| is_read_only_command(segment));
    }
    // Workspace-aware: thread a virtual cwd through the chain starting at the
    // workspace root. Every `cd` target must resolve to a path that stays
    // inside the workspace, both lexically and canonically.
    let Some(root) = workspace.canonicalize().ok() else {
        return false;
    };
    let mut cwd = root.clone();
    for segment in segments {
        let mut words = segment.split_whitespace();
        let Some(first) = words.next() else {
            return false;
        };
        if first == "cd" {
            let args = words.collect::<Vec<_>>();
            if args.len() != 1 {
                return false;
            }
            let target = cwd.join(args[0]);
            let Ok(resolved) = resolve_workspace_path(&root, &target.to_string_lossy()) else {
                return false;
            };
            cwd = resolved;
        } else if !is_read_only_command(segment) {
            return false;
        }
    }
    true
}

fn split_simple_commands(command: &str) -> Option<Vec<&str>> {
    let mut segments = Vec::new();
    let mut start = 0;
    let bytes = command.as_bytes();
    let mut index = 0;
    while index < bytes.len() {
        let separator = match bytes[index] {
            b'&' if bytes.get(index + 1) == Some(&b'&') => 2,
            b'|' | b';' => 1,
            _ => {
                index += 1;
                continue;
            }
        };
        let segment = command[start..index].trim();
        if segment.is_empty() {
            return None;
        }
        segments.push(segment);
        index += separator;
        start = index;
    }
    let segment = command[start..].trim();
    if segment.is_empty() {
        return None;
    }
    segments.push(segment);
    Some(segments)
}

fn is_read_only_command(command: &str) -> bool {
    let mut words = command.split_whitespace();
    let Some(first) = words.next() else {
        return false;
    };
    let args = words.collect::<Vec<_>>();
    match first {
        "rg" => !args.iter().any(|arg| arg.starts_with("--pre")),
        "grep" | "ls" | "pwd" | "head" | "tail" | "wc" | "cat" => true,
        #[cfg(windows)]
        "dir" | "type" | "where" | "ver" => true,
        "find" => !args.iter().any(|arg| {
            matches!(
                *arg,
                "-delete"
                    | "-exec"
                    | "-execdir"
                    | "-ok"
                    | "-okdir"
                    | "-fls"
                    | "-fprint"
                    | "-fprint0"
                    | "-fprintf"
            )
        }),
        "git" => {
            if args
                .iter()
                .any(|arg| *arg == "--output" || arg.starts_with("--output="))
            {
                return false;
            }
            match args.first().copied() {
                Some(
                    "status" | "diff" | "log" | "show" | "rev-parse" | "ls-files" | "describe"
                    | "blame" | "shortlog",
                ) => true,
                Some("branch") => args[1..].iter().all(|arg| {
                    matches!(
                        *arg,
                        "--list"
                            | "--all"
                            | "--remotes"
                            | "-a"
                            | "-r"
                            | "-v"
                            | "-vv"
                            | "--show-current"
                    )
                }),
                Some("remote") => args[1..]
                    .iter()
                    .all(|arg| matches!(*arg, "-v" | "--verbose" | "show")),
                _ => false,
            }
        }
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::bash_quote;

    #[test]
    fn fixed_arguments_cannot_escape_bash_quotes() {
        assert_eq!(bash_quote("a'b; touch outside"), "'a'\\''b; touch outside'");
    }
}

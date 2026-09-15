//! The clap command surface for the `latch` binary.
//!
//! Two usage styles share one definition:
//!
//! - the legacy top-level flags (`latch`, `latch -p ...`, `latch --resume`),
//!   which keep the interactive TUI and one-shot compatibility path;
//! - the explicit machine commands (`latch run`, `latch resume`,
//!   `latch sessions ...`), which are non-interactive and structured.
//!
//! Profile and configuration flags (`--config`, `--mode`, `--provider`,
//! `--model`, `--effort`, `--attach`, `--output`) are global so both styles use
//! exactly the same values.

use clap::{ArgGroup, Args as ClapArgs, Parser, Subcommand, ValueEnum};
use latch_protocol::Mode;
use std::path::PathBuf;
use std::str::FromStr;

#[derive(Parser)]
#[command(
    name = "latch",
    version,
    about = "A quiet, programmable terminal coding agent"
)]
pub struct Args {
    /// Continue the latest session for this workspace: visible transcript,
    /// effective mode, task state, evidence, failures, and change ownership.
    #[arg(long)]
    pub resume: bool,
    /// Resume an exact session UUID or unambiguous UUID prefix.
    #[arg(long, requires = "resume")]
    pub session: Option<String>,
    /// Resume the most recently active session in this workspace.
    #[arg(long, requires = "resume", conflicts_with = "session")]
    pub latest: bool,
    #[arg(short = 'p', long)]
    pub prompt: Option<String>,
    /// Load this config file instead of the default config location.
    #[arg(long, global = true)]
    pub config: Option<PathBuf>,
    #[arg(long, value_parser = parse_mode, global = true)]
    pub mode: Option<Mode>,
    /// Override the configured provider for this invocation.
    #[arg(long, global = true)]
    pub provider: Option<String>,
    /// Override the configured model for this invocation.
    #[arg(long, global = true)]
    pub model: Option<String>,
    /// Override the reasoning effort for this invocation.
    #[arg(long, global = true)]
    pub effort: Option<String>,
    /// Attach an image (PNG, JPEG, or WebP) to the prompt. Repeatable. The
    /// same ingestion path is used by the TUI's `/attach`.
    #[arg(long = "attach", visible_alias = "image", global = true)]
    pub attach: Vec<PathBuf>,
    /// Structured output for machine commands. `text` preserves the
    /// human-readable one-shot behavior; `json` prints exactly one final
    /// object; `jsonl` streams semantic records and always ends with `final`.
    #[arg(long, value_enum, default_value_t = OutputFormat::Text, global = true)]
    pub output: OutputFormat,
    #[command(subcommand)]
    pub command: Option<Commands>,
}

#[derive(Subcommand)]
pub enum Commands {
    /// Run one task without the TUI and exit. Never waits for human input.
    Run(RunArgs),
    /// Resume a durable session and run one task without the TUI.
    Resume(ResumeArgs),
    /// List or inspect durable sessions without opening the TUI.
    Sessions(SessionsArgs),
    /// Inspect internal prompt compilation and diagnostics without running a task.
    Debug {
        #[command(subcommand)]
        command: DebugCommand,
    },
}

/// Exactly one prompt source is required for machine commands. Ambiguous
/// combinations are rejected by clap before any runtime work starts.
#[derive(ClapArgs, Debug)]
pub struct PromptArgs {
    /// Inline prompt text.
    #[arg(long, value_name = "TEXT")]
    pub prompt: Option<String>,
    /// Read the prompt from a file.
    #[arg(long = "prompt-file", value_name = "PATH")]
    pub prompt_file: Option<PathBuf>,
    /// Read the prompt from standard input.
    #[arg(long)]
    pub stdin: bool,
}

#[derive(ClapArgs, Debug)]
#[command(group(
    ArgGroup::new("prompt_source")
        .required(true)
        .multiple(false)
        .args(["prompt", "prompt_file", "stdin"])
))]
pub struct RunArgs {
    #[command(flatten)]
    pub prompt: PromptArgs,
    /// Operate on this workspace instead of the current directory. The
    /// resolved path is used for session selection, policy, prompt
    /// compilation, and tools.
    #[arg(long, value_name = "PATH")]
    pub workspace: Option<PathBuf>,
}

#[derive(ClapArgs, Debug)]
#[command(group(
    ArgGroup::new("prompt_source")
        .required(true)
        .multiple(false)
        .args(["prompt", "prompt_file", "stdin"])
))]
pub struct ResumeArgs {
    /// Resume an exact session UUID or unambiguous UUID prefix.
    #[arg(long, value_name = "UUID-OR-PREFIX", conflicts_with = "latest")]
    pub session: Option<String>,
    /// Resume the most recently active session in the workspace.
    #[arg(long)]
    pub latest: bool,
    #[command(flatten)]
    pub prompt: PromptArgs,
    /// Workspace used for `--latest`-style selection. An explicit `--session`
    /// keeps that session's persisted workspace.
    #[arg(long, value_name = "PATH")]
    pub workspace: Option<PathBuf>,
}

#[derive(ClapArgs, Debug)]
pub struct SessionsArgs {
    #[command(subcommand)]
    pub command: SessionsCommand,
}

#[derive(Subcommand, Debug)]
pub enum SessionsCommand {
    /// List durable root sessions, newest first.
    List {
        /// Only sessions recorded for this workspace.
        #[arg(long, value_name = "PATH")]
        workspace: Option<PathBuf>,
    },
    /// Show a compact semantic summary of one durable session.
    Show {
        /// Exact session UUID or unambiguous UUID prefix.
        selector: String,
    },
}

#[derive(Subcommand)]
pub enum DebugCommand {
    Prompt {
        #[arg(long)]
        fragment: Option<String>,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, ValueEnum)]
pub enum OutputFormat {
    #[default]
    Text,
    Json,
    Jsonl,
}

impl OutputFormat {
    #[must_use]
    pub const fn is_machine(self) -> bool {
        matches!(self, Self::Json | Self::Jsonl)
    }
}

pub fn parse_mode(s: &str) -> Result<Mode, String> {
    Mode::from_str(s)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bare_invocation_keeps_the_interactive_path() {
        let args = Args::try_parse_from(["latch"]).unwrap();
        assert!(args.command.is_none());
        assert!(args.prompt.is_none());
        assert!(!args.resume);
        assert_eq!(args.output, OutputFormat::Text);
    }

    #[test]
    fn run_requires_exactly_one_prompt_source() {
        let args =
            Args::try_parse_from(["latch", "run", "--prompt", "x", "--output", "json"]).unwrap();
        assert!(matches!(args.command, Some(Commands::Run(_))));
        assert_eq!(args.output, OutputFormat::Json);
        assert!(Args::try_parse_from(["latch", "run", "--prompt", "x", "--stdin"]).is_err());
        assert!(Args::try_parse_from(["latch", "run"]).is_err());
    }

    #[test]
    fn global_profile_flags_work_after_the_subcommand() {
        let args = Args::try_parse_from([
            "latch", "run", "--prompt", "x", "--model", "m", "--effort", "high", "--mode", "plan",
        ])
        .unwrap();
        assert_eq!(args.model.as_deref(), Some("m"));
        assert_eq!(args.effort.as_deref(), Some("high"));
        assert_eq!(args.mode, Some(Mode::Plan));
        assert!(matches!(args.command, Some(Commands::Run(_))));
    }

    #[test]
    fn resume_selector_flags_conflict() {
        let args = Args::try_parse_from(["latch", "resume", "--latest", "--prompt", "x"]).unwrap();
        assert!(matches!(args.command, Some(Commands::Resume(_))));
        assert!(
            Args::try_parse_from([
                "latch",
                "resume",
                "--session",
                "s",
                "--latest",
                "--prompt",
                "x"
            ])
            .is_err()
        );
    }

    #[test]
    fn legacy_invocations_parse() {
        let args = Args::try_parse_from(["latch", "-p", "hi", "--model", "m"]).unwrap();
        assert_eq!(args.prompt.as_deref(), Some("hi"));
        assert_eq!(args.model.as_deref(), Some("m"));
        let args = Args::try_parse_from(["latch", "--resume", "--session", "deadbeef"]).unwrap();
        assert!(args.resume);
        assert_eq!(args.session.as_deref(), Some("deadbeef"));
    }
}

//! Terminal rendering over shared authoritative sidebar state.
use chrono::Duration;
use latch_protocol::{AgentStatus, CompletionState, Event};
pub use latch_ui::sidebar::*;
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use unicode_segmentation::UnicodeSegmentation;
use unicode_width::UnicodeWidthStr;

#[derive(Debug, Clone, PartialEq)]
pub struct SidebarModel(SidebarState);
impl std::ops::Deref for SidebarModel {
    type Target = SidebarState;
    fn deref(&self) -> &Self::Target {
        &self.0
    }
}
impl std::ops::DerefMut for SidebarModel {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.0
    }
}
impl SidebarModel {
    pub fn new(session: SidebarSession) -> Self {
        Self(SidebarState::new(session))
    }
    pub fn from_events(session: SidebarSession, events: &[Event]) -> Self {
        Self(SidebarState::from_events(session, events))
    }
}

fn dim() -> Style {
    crate::theme::palette().faint()
}
fn cyan() -> Style {
    crate::theme::palette().accent()
}
fn green() -> Style {
    crate::theme::palette().success()
}
fn red() -> Style {
    crate::theme::palette().failure()
}
fn yellow() -> Style {
    crate::theme::palette().attention()
}
fn section_title(text: &str) -> Line<'static> {
    Line::styled(
        text.to_owned(),
        crate::theme::palette().faint().add_modifier(Modifier::BOLD),
    )
}

impl SidebarModel {
    /// Renders the sidebar at the given inner size. Lower-priority detail is
    /// progressively removed (never scrolled) as height shrinks.
    #[must_use]
    pub fn render_lines(&self, width: u16, height: u16) -> Vec<Line<'static>> {
        if width == 0 || height == 0 {
            return Vec::new();
        }
        let width = width as usize;
        let height = height as usize;
        let mut out = self.session_lines(width);
        for section in [
            self.current_request()
                .as_ref()
                .map(|_| vec![self.request_lines(width)]),
            Some(vec![
                self.task_lines(width, false),
                self.task_summary_lines(width),
            ]),
            (self.run().started_at.is_some() || self.run().requests > 0)
                .then(|| vec![self.run_lines(width, false)]),
            (!self.subagents().is_empty()).then(|| vec![self.children_lines(width)]),
            (!self.group().is_empty()).then(|| vec![self.group_lines(width)]),
            Some(vec![
                self.context_lines(width, false, true),
                self.context_lines(width, false, false),
            ]),
            (!self.usage().is_empty()).then(|| vec![self.usage_lines(width, false)]),
            (self.change_lines(width, false).len() > 1)
                .then(|| vec![self.change_lines(width, false)]),
        ]
        .into_iter()
        .flatten()
        {
            if !push_section(&mut out, height, section) {
                break;
            }
        }
        out.truncate(height);
        out
    }

    #[must_use]
    pub fn advanced_context_text(&self) -> String {
        self.context_lines(100, true, true)
            .into_iter()
            .chain(self.run_lines(100, true))
            .chain(self.usage_lines(100, true))
            .map(|line| {
                line.spans
                    .into_iter()
                    .map(|span| span.content.into_owned())
                    .collect::<String>()
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    fn request_lines(&self, width: usize) -> Vec<Line<'static>> {
        let mut lines = vec![section_title("CURRENT REQUEST")];
        if let Some(request) = &self.current_request() {
            lines.extend(wrap_words(request, width, 2).into_iter().map(Line::from));
        }
        lines
    }

    fn session_lines(&self, width: usize) -> Vec<Line<'static>> {
        let mut lines = Vec::new();
        lines.push(Line::styled(
            fit(&self.session().model, width),
            Style::default().add_modifier(Modifier::BOLD),
        ));
        let mut meta = format!("{}", self.session().mode);
        meta.push_str(&format!(" · {} turns", self.turns()));
        if let Some(age) = self.session_age() {
            meta.push_str(&format!(" · {}", format_age(age)));
        }
        lines.push(Line::styled(fit(&meta, width), cyan()));
        let mut sub = Vec::new();
        if !self.session().branch.is_empty() && self.session().branch != "-" {
            sub.push(format!("on {}", self.session().branch));
        }
        if self.session().resumed {
            sub.push("resumed".into());
        }
        if !sub.is_empty() {
            lines.push(Line::styled(fit(&sub.join(" · "), width), dim()));
        }
        lines
    }

    /// Compact child-agent section: name, status, and the latest bounded
    /// semantic summary. Empty when no child sessions are known.
    fn children_lines(&self, width: usize) -> Vec<Line<'static>> {
        if self.subagents().is_empty() {
            return Vec::new();
        }
        let mut lines = vec![section_title("CHILDREN")];
        let active = self.subagents().active_count();
        let total = self.subagents().agents().len();
        lines.push(Line::styled(
            fit(&format!("{active} active · {total} known"), width),
            if active > 0 { cyan() } else { dim() },
        ));
        for agent in self.subagents().agents().iter().take(6) {
            let (dot, dot_style) = match agent.status {
                AgentStatus::Running | AgentStatus::Starting => ("●", cyan()),
                AgentStatus::Completed => ("✓", green()),
                AgentStatus::Failed => ("✗", red()),
                AgentStatus::Interrupted => ("●", yellow()),
                AgentStatus::Closed => ("○", dim()),
            };
            let name = agent.agent_type.as_deref().map_or_else(
                || agent.task_name.clone(),
                |kind| format!("{} [{kind}]", agent.task_name),
            );
            lines.push(Line::from(vec![
                Span::styled(dot.to_owned(), dot_style),
                Span::raw(" "),
                Span::raw(fit(&name, width.saturating_sub(2))),
            ]));
            if !agent.summary.is_empty() {
                lines.push(Line::styled(
                    format!("  {}", fit(&agent.summary, width.saturating_sub(2))),
                    dim(),
                ));
            }
        }
        if self.subagents().agents().len() > 6 {
            lines.push(Line::styled(
                format!("  … and {} more", self.subagents().agents().len() - 6),
                dim(),
            ));
        }
        lines
    }

    /// Restrained coordination block: counts plus at most a few active or
    /// ready task titles. The full DAG stays behind `/group`.
    fn group_lines(&self, width: usize) -> Vec<Line<'static>> {
        if self.group().is_empty() {
            return Vec::new();
        }
        let counts = self.group().counts();
        let mut lines = vec![section_title("GROUP")];
        let name = self.group().name().unwrap_or("group");
        lines.push(Line::styled(
            fit(&format!("{name} · tasks {}", counts.total), width),
            dim(),
        ));
        lines.push(Line::styled(
            fit(
                &format!(
                    "ready {} · active {} · blocked {} · done {}",
                    counts.ready,
                    counts.active(),
                    counts.blocked,
                    counts.completed
                ),
                width,
            ),
            if counts.active() > 0 { cyan() } else { dim() },
        ));
        let owner = |agent: uuid::Uuid| {
            self.subagents()
                .agents()
                .iter()
                .find(|view| view.agent_id == Some(agent))
                .map(|view| view.task_name.clone())
                .unwrap_or_else(|| agent.to_string()[..8].to_owned())
        };
        for task in self.group().active_tasks().iter().take(3) {
            lines.push(Line::from(vec![
                Span::styled("●", cyan()),
                Span::raw(" "),
                Span::raw(fit(
                    &task.title,
                    width.saturating_sub(2 + owner(task.assignee.unwrap_or_default()).len()),
                )),
                Span::styled(
                    format!(" {}", owner(task.assignee.unwrap_or_default())),
                    dim(),
                ),
            ]));
        }
        if counts.active() == 0 {
            for task in self.group().ready_tasks().iter().take(2) {
                lines.push(Line::from(vec![
                    Span::styled("○", dim()),
                    Span::raw(" "),
                    Span::raw(fit(
                        &format!("ready · {}", task.title),
                        width.saturating_sub(2),
                    )),
                ]));
            }
        }
        lines
    }

    fn context_lines(&self, width: usize, detail: bool, show_bar: bool) -> Vec<Line<'static>> {
        let Some(context) = &self.context() else {
            return vec![section_title("CONTEXT")];
        };
        let mut lines = vec![section_title("CONTEXT")];
        // The working set is the complete estimated request; the denominator is
        // the model's context window. Provider-reported usage, shown below when
        // available, is authoritative for the request that actually ran.
        let working = if context.window_tokens > 0 {
            format!(
                "≈{} / {} tok",
                format_tokens(context.total_tokens as u64),
                format_tokens(context.window_tokens as u64)
            )
        } else {
            format!("≈{} tok", format_tokens(context.total_tokens as u64))
        };
        lines.push(Line::from(vec![
            Span::styled("Working set  ", dim()),
            Span::raw(fit(&working, width.saturating_sub(13))),
        ]));
        if show_bar && context.budget_tokens > 0 {
            lines.push(Line::styled(
                bar(context.total_tokens, context.budget_tokens, width),
                if context.total_tokens > context.budget_tokens {
                    yellow()
                } else {
                    cyan()
                },
            ));
        }
        if detail && context.request_tokens > 0 {
            // Estimated architecture cacheability from Latch's canonical
            // serialization and token estimator; the provider's own tokenizer
            // may differ, so provider-reported cache reads below are the
            // authoritative measurement. The prefix and the request are
            // estimated from slightly different shapes, so clamp the display to
            // a fully-reusable prefix.
            let cacheable =
                architecture_cacheability(context.common_prefix_tokens, context.request_tokens);
            lines.push(Line::from(vec![
                Span::styled("Arch prefix ", dim()),
                Span::raw(format!(
                    "≈{cacheable}% est. · {} tok shared",
                    format_tokens(context.common_prefix_tokens as u64)
                )),
            ]));
            if let Some(last) = &self.last_usage()
                && let Some(read) = last.cache_read_tokens
                && context.common_prefix_tokens > 0
            {
                // How much of Latch's theoretically reusable prefix the
                // provider actually reused. This is not a hit rate; provider
                // tokenizer and wire framing can differ from the estimate.
                let utilization = provider_prefix_utilization(read, context.common_prefix_tokens);
                lines.push(Line::from(vec![
                    Span::styled("Prefix use  ", dim()),
                    Span::raw(format!("≈{utilization}% of shared prefix reused")),
                ]));
            }
        }
        if detail {
            for (label, value) in [
                ("Recent", context.recent_tokens),
                ("State", context.state_tokens),
                ("Recall", context.recall_tokens),
                (
                    "Tools+ext",
                    context
                        .tools_tokens
                        .saturating_add(context.extension_tokens),
                ),
            ] {
                if value == 0 {
                    continue;
                }
                lines.push(Line::from(vec![
                    Span::styled(format!("{label:<12}"), dim()),
                    Span::raw(fit(
                        &format!("{} tok", format_tokens(value as u64)),
                        width.saturating_sub(12),
                    )),
                ]));
            }
            if context.cache_epoch > 0 || context.cache_epoch_tokens > 0 {
                lines.push(Line::from(vec![
                    Span::styled("Epoch       ", dim()),
                    Span::raw(fit(
                        &format!(
                            "gen {} · {} turns · ≈{} tok",
                            context.cache_epoch,
                            context.cache_epoch_turns,
                            format_tokens(context.cache_epoch_tokens as u64)
                        ),
                        width.saturating_sub(12),
                    )),
                ]));
                if !context.cache_rotation_reason.is_empty() {
                    let reason = match context.cache_rotation_reason.as_str() {
                        "working budget high-water mark reached" => "high-water",
                        other => other,
                    };
                    lines.push(Line::from(vec![
                        Span::styled("Rotated     ", dim()),
                        Span::raw(fit(
                            &format!(
                                "{} (retained ≈{} tok)",
                                reason,
                                format_tokens(context.cache_rotation_retained_tokens as u64)
                            ),
                            width.saturating_sub(12),
                        )),
                    ]));
                }
            }
            lines.push(Line::from(vec![
                Span::styled("Reserve     ", dim()),
                Span::raw(format!(
                    "{} tok",
                    format_tokens(context.reserve_tokens as u64)
                )),
            ]));
            lines.push(Line::from(vec![
                Span::styled("Headroom    ", dim()),
                Span::raw(format!(
                    "{} tok",
                    format_tokens(context.headroom_tokens as u64)
                )),
            ]));
        }
        if detail && let Some(last) = &self.last_usage() {
            lines.push(Line::styled(
                fit(
                    &format!(
                        "Reported    last request {} tok",
                        format_tokens(last.input_tokens)
                    ),
                    width,
                ),
                dim(),
            ));
            if let Some(read) = last.cache_read_tokens
                && let Some(miss) = last.uncached_input_tokens()
                && let Some(hit_pct) = measured_cache_hit_rate(read, miss)
            {
                // Provider-reported actual hit rate, normalized over reported
                // hit and miss categories. Unknown categories stay unknown and
                // the line is simply not shown.
                lines.push(Line::styled(
                    fit(
                        &format!(
                            "Measured    {}% cache hit · {} hit · {} miss",
                            hit_pct,
                            format_tokens(read),
                            format_tokens(miss)
                        ),
                        width,
                    ),
                    dim(),
                ));
            }
        }
        if detail {
            lines.push(Line::styled(
                fit(
                    &format!(
                        "{} events · {} episodes",
                        context.durable_events, context.episodes
                    ),
                    width,
                ),
                dim(),
            ));
        }
        if context.status != "bounded" {
            lines.push(Line::styled(
                fit(&format!("⚠ context {}", context.status), width),
                yellow(),
            ));
        }
        lines
    }

    fn task_lines(&self, width: usize, detail: bool) -> Vec<Line<'static>> {
        let Some(task) = &self.task() else {
            return vec![
                section_title("TASK"),
                Line::styled("InProgress", cyan()),
                Line::from("Implementation unchanged"),
                Line::from("Validation pending"),
            ];
        };
        let mut lines = vec![section_title("TASK")];
        if !task.goal.trim().is_empty() && (detail || self.current_request().is_none()) {
            let goal_lines = if detail { 2 } else { 1 };
            for (index, line) in wrap_words(&task.goal, width, goal_lines)
                .into_iter()
                .enumerate()
            {
                lines.push(Line::styled(
                    line,
                    if index == 0 { Style::default() } else { dim() },
                ));
            }
        }
        let completion = if self.validation_stale() && task.completion == CompletionState::Verified
        {
            CompletionState::ImplementedNotVerified
        } else {
            task.completion.clone()
        };
        let (label, style) = completion_label(&completion);
        lines.push(Line::styled(fit(label, width), style));
        let implementation = if task.implementation_done {
            "claimed"
        } else if !self.changes().is_empty() {
            "changed"
        } else {
            "unchanged"
        };
        lines.push(Line::styled(
            fit(&format!("Implementation {implementation}"), width),
            dim(),
        ));
        lines.push(Line::styled(
            fit(&format!("Validation {}", self.validation_status()), width),
            if self.validation_status() == "passed" {
                green()
            } else {
                yellow()
            },
        ));
        if detail && !task.required_validations.is_empty() {
            lines.push(Line::from(vec![
                Span::styled("Validations  ", dim()),
                Span::raw(format!(
                    "{} / {}",
                    self.validations_passed(),
                    task.required_validations.len()
                )),
            ]));
        }
        if let Some(stall) = self.stall() {
            lines.push(Line::styled(fit("⚠ STALLED", width), yellow()));
            if detail {
                lines.push(Line::styled(
                    fit(
                        &format!(
                            "  no progress for {} turn(s); {} redundant observation(s)",
                            stall.redundant_turns, stall.unchanged
                        ),
                        width,
                    ),
                    dim(),
                ));
            }
        }
        lines
    }

    fn task_summary_lines(&self, width: usize) -> Vec<Line<'static>> {
        let completion = self
            .task()
            .as_ref()
            .map_or(CompletionState::InProgress, |task| {
                if self.validation_stale() && task.completion == CompletionState::Verified {
                    CompletionState::ImplementedNotVerified
                } else {
                    task.completion.clone()
                }
            });
        let (label, style) = completion_label(&completion);
        vec![
            section_title("TASK"),
            Line::styled(fit(label, width), style),
        ]
    }

    fn run_lines(&self, width: usize, detail: bool) -> Vec<Line<'static>> {
        let mut lines = vec![section_title("RUN")];
        if self.run().started_at.is_none() && self.run().requests == 0 {
            return lines;
        }
        let status = match self.run().outcome.as_deref() {
            None => "running",
            Some("completed") => "finished",
            Some("cancelled") => "cancelled",
            Some("error") => "error",
            Some(other) => other,
        };
        lines.push(Line::styled(
            fit(status, width),
            if status == "error" { red() } else { cyan() },
        ));
        let elapsed = self
            .run()
            .elapsed_seconds()
            .map(format_duration)
            .unwrap_or_else(|| "…".to_owned());
        if detail {
            let row = |label: &str, value: String| {
                // 11-column label plus an explicit separator so longer labels
                // (for example "tool result") never collide with the value.
                Line::from(vec![
                    Span::styled(format!("{label:<11} "), dim()),
                    Span::raw(fit(&value, width.saturating_sub(12))),
                ])
            };
            lines.push(row("time", elapsed));
            lines.push(row("requests", self.run().requests.to_string()));
            lines.push(row("tools", format!("{}", self.run().tool_calls)));
            lines.push(row("in", option_tokens(self.run().usage.input)));
            lines.push(row("out", option_tokens(self.run().usage.output)));
            let cache = format!(
                "{} read · {} miss",
                option_tokens(self.run().usage.cache_read),
                option_tokens(self.run().usage.cache_miss)
            );
            lines.push(row("cache", cache));
            lines.push(row(
                "replay",
                format!(
                    "{} cumulative · {} last",
                    format_tokens(self.run().reasoning_replay_tokens as u64),
                    format_tokens(self.run().last_reasoning_replay_tokens as u64)
                ),
            ));
            lines.push(row(
                "tool args",
                format!(
                    "{} cumulative · {} last",
                    format_tokens(self.run().tool_argument_tokens as u64),
                    format_tokens(self.run().last_tool_argument_tokens as u64)
                ),
            ));
            lines.push(row(
                "tool result",
                format!(
                    "{} cumulative · {} last",
                    format_tokens(self.run().tool_result_tokens as u64),
                    format_tokens(self.run().last_tool_result_tokens as u64)
                ),
            ));
            if self.run().files_changed > 0 || self.run().files_read > 0 {
                lines.push(row(
                    "files",
                    format!(
                        "{} read · {} changed",
                        self.run().files_read,
                        self.run().files_changed
                    ),
                ));
            }
            lines.push(row("validations", self.run().validations.to_string()));
            if let Some(outcome) = &self.run().outcome {
                lines.push(row("outcome", outcome.clone()));
            }
        } else {
            lines.push(Line::styled(
                fit(
                    &format!(
                        "{} req · {} tools · {} · in {} out {}",
                        self.run().requests,
                        self.run().tool_calls,
                        elapsed,
                        option_tokens(self.run().usage.input),
                        option_tokens(self.run().usage.output)
                    ),
                    width,
                ),
                Style::default(),
            ));
        }
        lines
    }

    fn usage_lines(&self, width: usize, detail: bool) -> Vec<Line<'static>> {
        if self.usage().is_empty() && self.last_usage().is_none() {
            return vec![section_title("SESSION")];
        }
        let mut lines = vec![section_title("SESSION")];
        if detail {
            if let Some(last) = &self.last_usage() {
                lines.push(Line::from(vec![
                    Span::styled("last  ", dim()),
                    Span::raw(format!(
                        "in {} · out {}",
                        format_tokens(last.input_tokens),
                        format_tokens(last.output_tokens)
                    )),
                ]));
            }
            for (label, total, partial) in [
                ("in", self.usage().input, self.usage().input_partial),
                ("out", self.usage().output, self.usage().output_partial),
                (
                    "cache read",
                    self.usage().cache_read,
                    self.usage().cache_read_partial,
                ),
                (
                    "cache write",
                    self.usage().cache_write,
                    self.usage().cache_write_partial,
                ),
            ] {
                let value = match total {
                    Some(total) if partial => format!("≥ {}", format_tokens(total)),
                    Some(total) => format_tokens(total),
                    None => "—".to_owned(),
                };
                lines.push(Line::from(vec![
                    Span::styled(format!("{label:<12}"), dim()),
                    Span::raw(fit(&value, width.saturating_sub(12))),
                ]));
            }
            lines.push(Line::from(vec![
                Span::styled("run         ", dim()),
                Span::raw(format!(
                    "{} req · {} tool{} · {}",
                    self.turns(),
                    self.tool_calls(),
                    if self.tool_calls() == 1 { "" } else { "s" },
                    self.elapsed_text()
                )),
            ]));
            if let Some(stats) = &self.context() {
                lines.push(Line::from(vec![
                    Span::styled("replay      ", dim()),
                    Span::raw(format!(
                        "reasoning {} · tool args {} · results {}",
                        format_tokens(stats.reasoning_replay_tokens as u64),
                        format_tokens(stats.tool_arguments_tokens as u64),
                        format_tokens(stats.tool_result_tokens as u64)
                    )),
                ]));
            }
        } else {
            lines.push(Line::styled(
                fit(
                    &format!(
                        "in {} · out {}",
                        option_tokens(self.usage().input),
                        option_tokens(self.usage().output)
                    ),
                    width,
                ),
                Style::default(),
            ));
        }
        lines.push(Line::from(vec![
            Span::styled("est. cost   ", dim()),
            Span::raw(self.cost_text()),
        ]));
        lines
    }

    /// Wall-clock span observed in the durable stream, coarse enough to stay
    /// quiet in the sidebar.
    fn elapsed_text(&self) -> String {
        let (Some(started), Some(updated)) = (self.started_at(), self.updated_at()) else {
            return "—".to_owned();
        };
        let seconds = (updated - started).num_seconds().max(0);
        if seconds >= 3600 {
            format!("{}h{}m", seconds / 3600, (seconds % 3600) / 60)
        } else if seconds >= 60 {
            format!("{}m{}s", seconds / 60, seconds % 60)
        } else {
            format!("{seconds}s")
        }
    }

    fn cost_text(&self) -> String {
        match self.estimated_cost() {
            Some(cost) => {
                let value = format_money(cost.amount, &cost.currency);
                if cost.partial {
                    format!("≥ {value}")
                } else {
                    value
                }
            }
            None => "—".to_owned(),
        }
    }

    fn change_lines(&self, width: usize, detail: bool) -> Vec<Line<'static>> {
        let latch = self.changes().latch();
        let shell = self.changes().shell();
        let extension = self.changes().extension();
        let external = self.changes().external_files();
        let untrackable = self.changes().shell_untrackable_files();
        if latch.files.is_empty()
            && shell.files.is_empty()
            && extension.files.is_empty()
            && external.is_empty()
            && untrackable.is_empty()
        {
            return vec![section_title("CHANGES")];
        }
        let mut lines = vec![section_title("CHANGES")];
        let mut push_owner = |label: &str, stats: &OwnerStats, unknown: bool| {
            if stats.files.is_empty() && !unknown {
                return;
            }
            lines.push(change_line(label, stats, width, detail));
        };
        push_owner("Latch", &latch, false);
        let mut shell = shell;
        shell.files.extend(untrackable.iter().cloned());
        push_owner("Shell", &shell, !untrackable.is_empty());
        push_owner("Extension", &extension, false);
        if !untrackable.is_empty() {
            lines.push(Line::styled(
                fit(
                    &format!("⚠ {} shell path(s) not undoable", untrackable.len()),
                    width,
                ),
                yellow(),
            ));
        }
        if !external.is_empty() {
            lines.push(Line::from(vec![
                Span::styled(format!("{:<11}", "External"), dim()),
                Span::raw(format!(
                    "{} file{}",
                    external.len(),
                    if external.len() == 1 { "" } else { "s" }
                )),
            ]));
        }
        let externally_modified = self.changes().externally_modified_files();
        if !externally_modified.is_empty() {
            lines.push(Line::styled(
                fit(
                    &format!(
                        "⚠ {} owned file(s) changed externally",
                        externally_modified.len()
                    ),
                    width,
                ),
                yellow(),
            ));
        }
        lines
    }
}

/// Pushes the first variant that fits the remaining height, including one
/// separator blank line. Returns false when no variant fits.
fn push_section(
    lines: &mut Vec<Line<'static>>,
    height: usize,
    variants: Vec<Vec<Line<'static>>>,
) -> bool {
    for section in variants {
        let blank = !lines.is_empty();
        if lines.len() + section.len() + usize::from(blank) <= height {
            if blank {
                lines.push(Line::from(""));
            }
            lines.extend(section);
            return true;
        }
    }
    false
}

fn change_line(label: &str, stats: &OwnerStats, width: usize, detail: bool) -> Line<'static> {
    let files = stats.files.len();
    let mut spans = vec![
        Span::styled(format!("{label:<11}"), dim()),
        Span::raw(format!("{files} file{}", if files == 1 { "" } else { "s" })),
    ];
    if detail && !stats.lines_unknown && (stats.additions > 0 || stats.deletions > 0) {
        spans.push(Span::raw("  "));
        spans.push(Span::styled(
            format!("+{}", stats.additions),
            if stats.additions > 0 { green() } else { dim() },
        ));
        spans.push(Span::raw(" "));
        spans.push(Span::styled(
            format!("−{}", stats.deletions),
            if stats.deletions > 0 { red() } else { dim() },
        ));
    }
    let line = Line::from(spans);
    let line_width: usize = line
        .spans
        .iter()
        .map(|span| UnicodeWidthStr::width(span.content.as_ref()))
        .sum();
    if line_width <= width {
        line
    } else {
        let files = format!(
            "{} file{}",
            stats.files.len(),
            if stats.files.len() == 1 { "" } else { "s" }
        );
        Line::from(vec![
            Span::styled(format!("{label:<11}"), dim()),
            Span::raw(files),
        ])
    }
}

fn completion_label(completion: &CompletionState) -> (&'static str, Style) {
    match completion {
        CompletionState::Verified => ("✓ VERIFIED", green()),
        CompletionState::Blocked => ("⚠ BLOCKED", yellow()),
        CompletionState::ImplementedNotVerified => ("● IMPLEMENTED, NOT VERIFIED", cyan()),
        CompletionState::InProgress => ("● Implementing", cyan()),
    }
}

fn option_tokens(tokens: Option<u64>) -> String {
    tokens.map_or_else(|| "—".to_owned(), format_tokens)
}

#[must_use]
pub fn format_tokens(tokens: u64) -> String {
    let value = tokens as f64;
    if tokens < 1_000 {
        tokens.to_string()
    } else if tokens < 10_000_000 {
        format!("{}k", trim_decimal(value / 1_000.0))
    } else {
        format!("{}M", trim_decimal(value / 1_000_000.0))
    }
}

fn trim_decimal(value: f64) -> String {
    let text = format!("{value:.1}");
    text.strip_suffix(".0").map(str::to_owned).unwrap_or(text)
}

#[must_use]
pub fn format_duration(seconds: i64) -> String {
    if seconds >= 3600 {
        format!("{}h{}m", seconds / 3600, (seconds % 3600) / 60)
    } else if seconds >= 60 {
        format!("{}m{}s", seconds / 60, seconds % 60)
    } else {
        format!("{seconds}.0s")
    }
}

fn format_age(age: Duration) -> String {
    let seconds = age.num_seconds().max(0);
    if seconds < 60 {
        format!("{seconds}s")
    } else if seconds < 3_600 {
        format!("{}m", seconds / 60)
    } else {
        format!("{}h {}m", seconds / 3_600, (seconds % 3_600) / 60)
    }
}

#[must_use]
pub fn format_money(amount: f64, currency: &str) -> String {
    let value = format!("{amount:.2}");
    match currency {
        "USD" => format!("${value}"),
        "EUR" => format!("€{value}"),
        "GBP" => format!("£{value}"),
        "JPY" => format!("¥{value}"),
        other => format!("{value} {other}"),
    }
}

/// A restrained unicode meter: filled for the used fraction, light for the
/// remainder.
fn bar(used: usize, budget: usize, width: usize) -> String {
    let width = width.clamp(8, 32);
    if budget == 0 {
        return "░".repeat(width);
    }
    let filled = (used.saturating_mul(width) / budget).min(width);
    format!("{}{}", "█".repeat(filled), "░".repeat(width - filled))
}

/// Fits text to `width` columns, appending an ellipsis when truncated.
#[must_use]
pub fn fit(text: &str, width: usize) -> String {
    if width == 0 {
        return String::new();
    }
    if UnicodeWidthStr::width(text) <= width {
        return text.to_owned();
    }
    let mut out = String::new();
    let mut used = 0usize;
    for grapheme in text.graphemes(true) {
        let w = UnicodeWidthStr::width(grapheme).max(1);
        if used + w > width.saturating_sub(1) {
            break;
        }
        out.push_str(grapheme);
        used += w;
    }
    out.push('…');
    out
}

/// Greedy word wrap into at most `max_lines` rows, ellipsizing the remainder.
fn wrap_words(text: &str, width: usize, max_lines: usize) -> Vec<String> {
    if width == 0 || max_lines == 0 {
        return Vec::new();
    }
    let mut lines: Vec<String> = Vec::new();
    let mut current = String::new();
    for word in text.split_whitespace() {
        let candidate = if current.is_empty() {
            word.to_owned()
        } else {
            format!("{current} {word}")
        };
        if UnicodeWidthStr::width(candidate.as_str()) <= width {
            current = candidate;
            continue;
        }
        if !current.is_empty() {
            lines.push(std::mem::take(&mut current));
        }
        current = word.to_owned();
        if lines.len() == max_lines {
            break;
        }
    }
    if !current.is_empty() && lines.len() < max_lines {
        lines.push(current);
    }
    if lines.len() == max_lines
        && text.split_whitespace().count() > lines.len()
        && let Some(last) = lines.last_mut()
    {
        *last = fit(last, width);
    }
    lines.truncate(max_lines);
    lines
}

/// Estimated architecture cacheability: the share of Latch's own canonical
/// request serialization that is an exact reusable prefix. This is an
/// architecture diagnostic, not a provider tokenizer measurement.
#[must_use]
pub(crate) fn architecture_cacheability(common_prefix_tokens: usize, request_tokens: usize) -> u64 {
    let raw = common_prefix_tokens.saturating_mul(100) / request_tokens.max(1);
    raw.min(100) as u64
}

/// Provider prefix utilization: how much of Latch's estimated reusable prefix
/// the provider actually read. Clamped because the two sides are estimated
/// from slightly different shapes.
#[must_use]
pub(crate) fn provider_prefix_utilization(
    cache_read_tokens: u64,
    common_prefix_tokens: usize,
) -> u64 {
    if common_prefix_tokens == 0 {
        return 0;
    }
    (cache_read_tokens.saturating_mul(100) / common_prefix_tokens as u64).min(100)
}

/// Measured provider cache hit rate over the provider's own reported hit/miss
/// input categories. `None` means a category is unknown and must not be
/// fabricated.
#[must_use]
pub(crate) fn measured_cache_hit_rate(
    cache_hit_tokens: u64,
    cache_miss_tokens: u64,
) -> Option<u64> {
    let total = cache_hit_tokens.saturating_add(cache_miss_tokens);
    if total == 0 {
        return None;
    }
    Some(cache_hit_tokens.saturating_mul(100) / total)
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::Utc;
    use latch_protocol::*;
    use latch_protocol::{ChangeOwner, EvidenceStatus, FileVersion, Usage};
    use uuid::Uuid;

    fn event(payload: EventPayload) -> Event {
        Event {
            id: Uuid::new_v4(),
            session_id: Uuid::nil(),
            sequence: 1,
            timestamp: Utc::now(),
            parent_id: None,
            payload,
        }
    }

    fn session() -> SidebarSession {
        SidebarSession {
            model: "deepseek-flash".into(),
            mode: Mode::Work,
            branch: "main".into(),
            resumed: false,
            pricing: None,
        }
    }

    fn stats() -> ContextStats {
        ContextStats {
            instructions_tokens: 3_000,
            state_tokens: 3_300,
            recent_tokens: 37_900,
            recall_tokens: 1_500,
            tools_tokens: 2_400,
            extension_tokens: 0,
            total_tokens: 48_100,
            budget_tokens: 243_808,
            window_tokens: 256_000,
            reserve_tokens: 12_192,
            headroom_tokens: 195_708,
            durable_events: 503,
            episodes: 11,
            selected_episodes: 4,
            estimated: true,
            status: "bounded".into(),
            ..ContextStats::default()
        }
    }

    #[test]
    fn group_block_shows_counts_and_active_task_owner() {
        let mut model = SidebarModel::new(session());
        let group_id = Uuid::new_v4();
        let root = Uuid::new_v4();
        let child = Uuid::new_v4();
        model.apply_event(&event(EventPayload::AgentGroupCreated {
            identity: latch_protocol::AgentGroupIdentity {
                group_id,
                root_session_id: root,
                name: "workspace".into(),
                created_at: Utc::now(),
            },
        }));
        model.apply_event(&event(EventPayload::GroupTaskCreated {
            task: latch_protocol::GroupTask {
                task_id: Uuid::new_v4(),
                group_id,
                title: "parser".into(),
                description: String::new(),
                status: latch_protocol::GroupTaskStatus::Claimed,
                dependencies: vec![],
                assignee: Some(child),
                required: true,
                created_by: root,
                created_at: Utc::now(),
                updated_at: Utc::now(),
                summary: None,
                findings: vec![],
                expected_paths: vec![],
                touched_files: vec![],
                reason: None,
            },
        }));
        model.apply_event(&event(EventPayload::AgentNotificationDelivered {
            report: latch_protocol::AgentReport {
                report_id: Uuid::new_v4(),
                agent_id: child,
                task_name: "A-parser".into(),
                status: AgentStatus::Running,
                completion: latch_protocol::CompletionState::InProgress,
                summary: String::new(),
                findings: vec![],
                touched_files: vec![],
                evidence: vec![],
                unresolved_questions: vec![],
            },
        }));
        let rendered = render(&model, 50, 60);
        assert!(rendered.contains("GROUP"), "{rendered}");
        assert!(rendered.contains("workspace · tasks 1"), "{rendered}");
        assert!(rendered.contains("active 1"), "{rendered}");
        assert!(rendered.contains("parser"), "{rendered}");
        assert!(rendered.contains("A-parser"), "{rendered}");
        // A session without group events never grows a GROUP section.
        let plain = SidebarModel::new(session());
        assert!(!render(&plain, 50, 60).contains("GROUP"));
    }

    #[test]
    fn context_is_a_token_working_set_against_the_model_window() {
        let mut model = SidebarModel::new(session());
        model.apply_event(&event(EventPayload::ContextMaterialized { stats: stats() }));
        let rendered = render(&model, 40, 60);
        assert!(rendered.contains("Working set"));
        assert!(rendered.contains("≈48.1k / 256k tok"), "{rendered}");
        assert!(!rendered.contains("Headroom"));
        assert!(!rendered.contains("Epoch"));
        assert!(
            !rendered.contains("48.1k / 256k B"),
            "context must never render as bytes"
        );
    }

    #[test]
    fn task_completion_comes_from_kernel_state() {
        let mut model = SidebarModel::new(session());
        model.apply_event(&event(EventPayload::TaskStateUpdated {
            state: TaskState {
                goal: "TTL cache implementation".into(),
                required_validations: vec!["unit tests pass".into(), "clippy clean".into()],
                completion: CompletionState::Verified,
                ..TaskState::default()
            },
        }));
        model.apply_event(&event(EventPayload::EvidenceCreated {
            evidence: latch_protocol::Evidence {
                id: Uuid::new_v4(),
                claim: "unit tests pass".into(),
                source_event: Uuid::new_v4(),
                status: EvidenceStatus::Passed,
                detail: "ok".into(),
                created_at: Utc::now(),
                workspace_generation: None,
                supersedes: None,
            },
        }));
        assert_eq!(model.validations_passed(), 1);
        let rendered = render(&model, 40, 60);
        assert!(rendered.contains("VERIFIED"));
        assert!(rendered.contains("Validation pending"));
    }

    #[test]
    fn run_totals_reset_per_run_and_session_totals_accumulate() {
        let mut model = SidebarModel::new(session());
        let usage = |input: u64| EventPayload::ModelUsage {
            usage: Usage {
                input_tokens: input,
                output_tokens: 5,
                cache_read_tokens: None,
                cache_write_tokens: None,
                cache_miss_tokens: None,
                reasoning_tokens: None,
            },
        };
        model.apply_event(&event(EventPayload::RunStarted {
            run_id: Uuid::new_v4(),
            prompt: "first task".into(),
        }));
        model.apply_event(&event(EventPayload::ModelRequestStarted {
            provider: "deepseek".into(),
            model: "deepseek-flash".into(),
        }));
        model.apply_event(&event(EventPayload::ToolRequested {
            call: latch_protocol::ToolCall {
                id: "c1".into(),
                name: "read_file".into(),
                arguments: serde_json::json!({"path": "a"}),
            },
        }));
        model.apply_event(&event(usage(100)));
        model.apply_event(&event(EventPayload::RunCompleted {
            run_id: Uuid::new_v4(),
            outcome: "completed".into(),
        }));
        assert_eq!(model.run().requests, 1);
        assert_eq!(model.run().tool_calls, 1);
        assert_eq!(model.run().files_read, 1);
        assert_eq!(model.run().usage.input, Some(100));
        assert!(model.run().elapsed_seconds().is_some());
        assert_eq!(model.run().outcome.as_deref(), Some("completed"));

        // A second run resets run counters but not session totals.
        model.apply_event(&event(EventPayload::RunStarted {
            run_id: Uuid::new_v4(),
            prompt: "second task".into(),
        }));
        assert_eq!(model.run().requests, 0);
        assert!(model.run().usage.is_empty());
        assert_eq!(model.run().elapsed_seconds(), None);
        model.apply_event(&event(EventPayload::ModelRequestStarted {
            provider: "deepseek".into(),
            model: "deepseek-flash".into(),
        }));
        model.apply_event(&event(usage(200)));
        assert_eq!(model.run().requests, 1);
        assert_eq!(model.run().usage.input, Some(200));
        // Session totals stay cumulative across both runs.
        assert_eq!(model.usage().input, Some(300));
        assert_eq!(model.turns(), 2);
    }

    #[test]
    fn run_cost_components_accumulate_and_last_request_stays_separate() {
        let request_stats = |replay: usize| {
            let mut stats = stats();
            stats.reasoning_replay_tokens = replay;
            stats.tool_arguments_tokens = 100;
            stats.tool_result_tokens = 300;
            stats
        };
        let events = vec![
            event(EventPayload::RunStarted {
                run_id: Uuid::new_v4(),
                prompt: "task".into(),
            }),
            event(EventPayload::ContextMaterialized {
                stats: request_stats(400),
            }),
            event(EventPayload::ContextMaterialized {
                stats: request_stats(900),
            }),
        ];
        let mut model = SidebarModel::new(session());
        for event in &events {
            model.apply_event(event);
        }
        // Two requests in one run: cumulative totals are sums, the last-request
        // values describe only the final request.
        assert_eq!(model.run().reasoning_replay_tokens, 1_300);
        assert_eq!(model.run().tool_argument_tokens, 200);
        assert_eq!(model.run().tool_result_tokens, 600);
        assert_eq!(model.run().last_reasoning_replay_tokens, 900);
        assert_eq!(model.run().last_tool_argument_tokens, 100);
        assert_eq!(model.run().last_tool_result_tokens, 300);

        // Replaying the durable events once reconstructs identical totals.
        let replayed = SidebarModel::from_events(session(), &events);
        assert_eq!(replayed.run(), model.run());

        // A new run resets both cumulative and last-request values while the
        // session context view stays available.
        model.apply_event(&event(EventPayload::RunStarted {
            run_id: Uuid::new_v4(),
            prompt: "next".into(),
        }));
        assert_eq!(model.run().reasoning_replay_tokens, 0);
        assert_eq!(model.run().last_reasoning_replay_tokens, 0);
        assert!(model.context().is_some());
    }

    #[test]
    fn usage_aggregates_and_missing_cache_stays_unknown() {
        let mut model = SidebarModel::new(session());
        model.apply_event(&event(EventPayload::ModelUsage {
            usage: Usage {
                input_tokens: 100,
                output_tokens: 20,
                cache_read_tokens: Some(1_000),
                cache_write_tokens: None,
                cache_miss_tokens: None,
                reasoning_tokens: None,
            },
        }));
        model.apply_event(&event(EventPayload::ModelUsage {
            usage: Usage {
                input_tokens: 300,
                output_tokens: 30,
                cache_read_tokens: Some(2_000),
                cache_write_tokens: None,
                cache_miss_tokens: None,
                reasoning_tokens: None,
            },
        }));
        assert_eq!(model.usage().input, Some(400));
        assert_eq!(model.usage().output, Some(50));
        assert_eq!(model.usage().cache_read, Some(3_000));
        assert_eq!(model.usage().cache_write, None);
        assert_eq!(model.usage().cache_write, None, "unknown is not zero");
        assert!(render(&model, 40, 60).contains("400"));
    }

    #[test]
    fn unknown_cache_then_known_marks_partial() {
        let mut model = SidebarModel::new(session());
        model.apply_event(&event(EventPayload::ModelUsage {
            usage: Usage {
                input_tokens: 10,
                output_tokens: 1,
                cache_read_tokens: None,
                cache_write_tokens: None,
                cache_miss_tokens: None,
                reasoning_tokens: None,
            },
        }));
        model.apply_event(&event(EventPayload::ModelUsage {
            usage: Usage {
                input_tokens: 10,
                output_tokens: 1,
                cache_read_tokens: Some(5),
                cache_write_tokens: None,
                cache_miss_tokens: None,
                reasoning_tokens: None,
            },
        }));
        assert_eq!(model.usage().cache_read, Some(5));
        assert!(model.usage().cache_read_partial);
        assert!(model.usage().cache_read_partial);
    }

    #[test]
    fn estimated_cost_uses_configured_prices_and_missing_pricing_is_unavailable() {
        let mut model = SidebarModel::new(session());
        model.apply_event(&event(EventPayload::ModelUsage {
            usage: Usage {
                input_tokens: 1_000_000,
                output_tokens: 500_000,
                cache_read_tokens: None,
                cache_write_tokens: None,
                cache_miss_tokens: None,
                reasoning_tokens: None,
            },
        }));
        assert!(model.estimated_cost().is_none(), "no pricing configured");
        let mut priced = session();
        priced.pricing = Some(Pricing {
            input_per_million: Some(0.28),
            output_per_million: Some(0.42),
            cache_read_per_million: Some(0.028),
            cache_write_per_million: None,
            currency: "USD".into(),
        });
        let mut model = SidebarModel::new(priced);
        model.apply_event(&event(EventPayload::ModelUsage {
            usage: Usage {
                input_tokens: 1_000_000,
                output_tokens: 500_000,
                cache_read_tokens: Some(100_000),
                cache_write_tokens: None,
                cache_miss_tokens: None,
                reasoning_tokens: None,
            },
        }));
        let cost = model.estimated_cost().expect("cost");
        // 900k uncached input at 0.28 + 100k cache read at 0.028 + 500k output
        // at 0.42. The cached tokens are billed once, at the cache-read price.
        assert!((cost.amount - (0.9 * 0.28 + 0.21 + 0.1 * 0.028)).abs() < 1e-9);
        assert!(!cost.partial);
        assert!(render(&model, 40, 60).contains("est. cost"));
        assert!(render(&model, 40, 60).contains("$0.46"));
    }

    #[test]
    fn cached_input_is_not_double_charged() {
        let mut priced = session();
        priced.pricing = Some(Pricing {
            input_per_million: Some(1.0),
            output_per_million: Some(1.0),
            cache_read_per_million: Some(0.1),
            cache_write_per_million: None,
            currency: "USD".into(),
        });
        let mut model = SidebarModel::new(priced);
        model.apply_event(&event(EventPayload::ModelUsage {
            usage: Usage {
                input_tokens: 1_000,
                output_tokens: 0,
                cache_read_tokens: Some(600),
                cache_write_tokens: None,
                cache_miss_tokens: None,
                reasoning_tokens: None,
            },
        }));
        let cost = model.estimated_cost().expect("cost");
        // 400 miss at 1.0 + 600 read at 0.1 = 460 tokens-worth of cost.
        assert!(
            (cost.amount - 460.0 / 1_000_000.0).abs() < 1e-12,
            "{cost:?}"
        );
    }

    #[test]
    fn explicit_cache_miss_is_used_verbatim() {
        let mut priced = session();
        priced.pricing = Some(Pricing {
            input_per_million: Some(1.0),
            output_per_million: Some(1.0),
            cache_read_per_million: Some(0.1),
            cache_write_per_million: None,
            currency: "USD".into(),
        });
        let mut model = SidebarModel::new(priced);
        model.apply_event(&event(EventPayload::ModelUsage {
            usage: Usage {
                input_tokens: 1_000,
                output_tokens: 0,
                cache_read_tokens: Some(600),
                cache_write_tokens: None,
                // The adapter normalized a provider-reported miss that is not
                // simply input minus read; the explicit value wins.
                cache_miss_tokens: Some(350),
                reasoning_tokens: None,
            },
        }));
        let cost = model.estimated_cost().expect("cost");
        assert!(
            (cost.amount - (350.0 + 60.0) / 1_000_000.0).abs() < 1e-12,
            "{cost:?}"
        );
    }

    #[test]
    fn unknown_cache_categories_bill_the_full_input() {
        let mut priced = session();
        priced.pricing = Some(Pricing {
            input_per_million: Some(1.0),
            output_per_million: Some(1.0),
            cache_read_per_million: Some(0.1),
            cache_write_per_million: None,
            currency: "USD".into(),
        });
        let mut model = SidebarModel::new(priced);
        model.apply_event(&event(EventPayload::ModelUsage {
            usage: Usage {
                input_tokens: 1_000,
                output_tokens: 0,
                cache_read_tokens: None,
                cache_write_tokens: None,
                cache_miss_tokens: None,
                reasoning_tokens: None,
            },
        }));
        let cost = model.estimated_cost().expect("cost");
        assert!((cost.amount - 1_000.0 / 1_000_000.0).abs() < 1e-12);
        assert!(cost.partial, "unreported cache accounting is incomplete");
    }

    #[test]
    fn missing_price_component_makes_cost_unavailable() {
        let mut priced = session();
        priced.pricing = Some(Pricing {
            input_per_million: Some(1.0),
            output_per_million: None,
            cache_read_per_million: None,
            cache_write_per_million: None,
            currency: "USD".into(),
        });
        let mut model = SidebarModel::new(priced);
        model.apply_event(&event(EventPayload::ModelUsage {
            usage: Usage {
                input_tokens: 1_000,
                output_tokens: 500,
                cache_read_tokens: None,
                cache_write_tokens: None,
                cache_miss_tokens: None,
                reasoning_tokens: None,
            },
        }));
        assert!(model.estimated_cost().is_none());
    }

    #[test]
    fn change_owners_stay_distinct_and_external_marks_owned_files() {
        let mut model = SidebarModel::new(session());
        model.apply_event(&event(EventPayload::GitStateObserved {
            head: None,
            dirty_paths: vec!["user.txt".into()],
        }));
        model.apply_event(&event(EventPayload::FileChanged {
            before: None,
            after: FileVersion {
                path: "src/lib.rs".into(),
                content_hash: "a".into(),
                size: 1,
            },
            created: false,
            owner: ChangeOwner::Latch,
            undo_artifact: None,
            additions: 53,
            deletions: 7,
            preview: String::new(),
            call_id: None,
        }));
        model.apply_event(&event(EventPayload::FileChanged {
            before: None,
            after: FileVersion {
                path: "src/tmp.rs".into(),
                content_hash: "b".into(),
                size: 1,
            },
            created: false,
            owner: ChangeOwner::Shell,
            undo_artifact: None,
            additions: 2,
            deletions: 0,
            preview: String::new(),
            call_id: None,
        }));
        model.apply_event(&event(EventPayload::ExternalFileChangeDetected {
            path: "src/lib.rs".into(),
            expected_hash: "a".into(),
            actual_hash: "c".into(),
        }));
        let change = model.changes();
        assert_eq!(change.latch().files.len(), 1);
        assert_eq!(change.latch().additions, 53);
        assert_eq!(change.shell().files.len(), 1);
        assert!(change.external_files().contains("user.txt"));
        assert!(
            change.externally_modified_files().contains("src/lib.rs"),
            "an externally edited owned file is reported honestly"
        );
        let rendered = render(&model, 44, 60);
        assert!(rendered.contains("Latch"));
        assert!(rendered.contains("Shell"));
        assert!(rendered.contains("External"));
        assert!(rendered.contains("changed externally"));
    }

    #[test]
    fn revert_removes_the_recorded_change() {
        let hash = "abc".to_owned();
        let mut model = SidebarModel::new(session());
        model.apply_event(&event(EventPayload::FileChanged {
            before: None,
            after: FileVersion {
                path: "src/lib.rs".into(),
                content_hash: hash.clone(),
                size: 1,
            },
            created: false,
            owner: ChangeOwner::Latch,
            undo_artifact: None,
            additions: 3,
            deletions: 1,
            preview: String::new(),
            call_id: None,
        }));
        model.apply_event(&event(EventPayload::ChangeReverted {
            path: "src/lib.rs".into(),
            content_hash: hash,
        }));
        assert!(model.changes().latch().files.is_empty());
    }

    #[test]
    fn live_and_replay_sidebar_state_converge() {
        let events = vec![
            event(EventPayload::ContextMaterialized { stats: stats() }),
            event(EventPayload::ModelRequestStarted {
                provider: "openai".into(),
                model: "deepseek-flash".into(),
            }),
            event(EventPayload::ModelUsage {
                usage: Usage {
                    input_tokens: 12,
                    output_tokens: 3,
                    cache_read_tokens: Some(4),
                    cache_write_tokens: None,
                    cache_miss_tokens: None,
                    reasoning_tokens: None,
                },
            }),
            event(EventPayload::TaskStateUpdated {
                state: TaskState {
                    goal: "goal".into(),
                    completion: CompletionState::InProgress,
                    ..TaskState::default()
                },
            }),
            event(EventPayload::ProgressStagnation {
                unchanged: vec!["read_file a".into(), "git status".into()],
                redundant_turns: 2,
            }),
        ];
        let replay = SidebarModel::from_events(session(), &events);
        let mut live = SidebarModel::new(session());
        for event in &events {
            live.apply_event(event);
        }
        assert_eq!(live, replay);
        assert_eq!(replay.turns(), 1);
        assert_eq!(replay.stall().map(|stall| stall.unchanged), Some(2));
        // Progress clears the stall.
        let mut progressed = replay.clone();
        progressed.apply_event(&event(EventPayload::ValidationResult {
            command: "cargo test".into(),
            passed: true,
            detail: "ok".into(),
        }));
        assert!(progressed.stall().is_none());
    }

    #[test]
    fn session_age_and_long_content_fit() {
        let start = Utc::now();
        let mut model = SidebarModel::new(session());
        let mut first = event(EventPayload::UserMessage {
            text: "a very long goal 你好世界 ".repeat(20),
            media: vec![],
        });
        first.timestamp = start;
        model.apply_event(&first);
        model.apply_event(&event(EventPayload::ModelRequestStarted {
            provider: "p".into(),
            model: "m".into(),
        }));
        let mut later = event(EventPayload::TaskStateUpdated {
            state: TaskState {
                goal: "a very long goal 你好世界 ".repeat(20),
                ..TaskState::default()
            },
        });
        later.timestamp = start + Duration::seconds(12 * 60 + 30);
        model.apply_event(&later);
        assert_eq!(model.session_age().map(|age| age.num_seconds()), Some(750));
        let rendered = render(&model, 28, 80);
        assert!(rendered.contains("12m"), "{rendered}");
        for line in rendered.lines() {
            assert!(UnicodeWidthStr::width(line) <= 28, "line too wide: {line}");
        }
    }

    #[test]
    fn short_height_drops_lower_priority_sections() {
        let mut model = SidebarModel::new(session());
        model.apply_event(&event(EventPayload::ContextMaterialized { stats: stats() }));
        model.apply_event(&event(EventPayload::TaskStateUpdated {
            state: TaskState {
                goal: "goal".into(),
                ..TaskState::default()
            },
        }));
        let short = render(&model, 32, 6);
        assert!(!short.contains("CHANGES"));
        assert!(!short.contains("USAGE"));
        assert!(short.contains("TASK"));
    }

    #[test]
    fn durable_run_request_usage_and_stale_validation_replay_identically() {
        let run_id = Uuid::new_v4();
        let first = vec![
            event(EventPayload::RunStarted {
                run_id,
                prompt: "original request".into(),
            }),
            event(EventPayload::UserMessage {
                text: "revised request".into(),
                media: vec![],
            }),
            event(EventPayload::ModelRequestStarted {
                provider: "p".into(),
                model: "m".into(),
            }),
            event(EventPayload::ModelUsage {
                usage: Usage {
                    input_tokens: 100,
                    output_tokens: 20,
                    cache_read_tokens: Some(30),
                    cache_write_tokens: Some(5),
                    cache_miss_tokens: Some(70),
                    reasoning_tokens: None,
                },
            }),
            event(EventPayload::TaskStateUpdated {
                state: TaskState {
                    goal: "original request".into(),
                    required_validations: vec!["tests".into()],
                    implementation_done: true,
                    completion: CompletionState::Verified,
                    ..TaskState::default()
                },
            }),
            event(EventPayload::EvidenceCreated {
                evidence: latch_protocol::Evidence {
                    id: Uuid::new_v4(),
                    claim: "tests".into(),
                    source_event: Uuid::new_v4(),
                    status: EvidenceStatus::Passed,
                    detail: "ok".into(),
                    created_at: Utc::now(),
                    workspace_generation: Some(1),
                    supersedes: None,
                },
            }),
            event(EventPayload::RunCompleted {
                run_id,
                outcome: "cancelled".into(),
            }),
        ];
        let mut live = SidebarModel::new(session());
        for item in &first {
            live.apply_event(item);
        }
        let replay = SidebarModel::from_events(session(), &first);
        assert_eq!(live, replay);
        assert_eq!(live.current_request(), Some("revised request"));
        assert_eq!(live.usage().input, Some(100));
        assert_eq!(live.run().usage.cache_read, Some(30));
        let rendered = render(&live, 40, 60);
        assert!(rendered.contains("cancelled"));
        assert!(
            rendered.contains("VERIFIED"),
            "task truth is separate from cancellation"
        );

        let mutation = event(EventPayload::WorkspaceMutationPossible {
            operation: "write".into(),
        });
        live.apply_event(&mutation);
        assert_eq!(live.validation_status(), "stale");
        assert!(!render(&live, 40, 60).contains("✓ VERIFIED"));
        let mut all = first;
        all.push(mutation);
        assert_eq!(live, SidebarModel::from_events(session(), &all));

        let second_run = event(EventPayload::RunStarted {
            run_id: Uuid::new_v4(),
            prompt: "new task".into(),
        });
        live.apply_event(&second_run);
        assert_eq!(live.run().usage.input, None);
        assert_eq!(live.usage().input, Some(100));
        assert_eq!(live.current_request(), Some("new task"));
        all.push(second_run);
        assert_eq!(live, SidebarModel::from_events(session(), &all));
    }

    #[test]
    fn unrelated_evidence_cannot_clear_stale_required_validations() {
        let mut model = SidebarModel::new(session());
        model.apply_event(&event(EventPayload::TaskStateUpdated {
            state: TaskState {
                required_validations: vec!["tests".into(), "clippy".into()],
                completion: CompletionState::Verified,
                ..TaskState::default()
            },
        }));
        let evidence = |claim: &str| {
            event(EventPayload::EvidenceCreated {
                evidence: latch_protocol::Evidence {
                    id: Uuid::new_v4(),
                    claim: claim.into(),
                    source_event: Uuid::new_v4(),
                    status: EvidenceStatus::Passed,
                    detail: "ok".into(),
                    created_at: Utc::now(),
                    workspace_generation: Some(1),
                    supersedes: None,
                },
            })
        };
        model.apply_event(&evidence("tests"));
        model.apply_event(&evidence("clippy"));
        assert_eq!(model.validation_status(), "passed");
        model.apply_event(&event(EventPayload::WorkspaceMutationPossible {
            operation: "write".into(),
        }));
        model.apply_event(&evidence("unrelated"));
        model.apply_event(&evidence("tests"));
        assert_eq!(model.validation_status(), "stale");
        model.apply_event(&evidence("clippy"));
        assert_eq!(model.validation_status(), "passed");
    }

    #[test]
    fn sidebar_snapshot() {
        use chrono::TimeZone;
        let start = Utc.with_ymd_and_hms(2026, 1, 1, 0, 0, 0).unwrap();
        let mut events = Vec::new();
        let mut push = |offset_seconds: i64, payload| {
            let mut event = event(payload);
            event.timestamp = start + Duration::seconds(offset_seconds);
            events.push(event);
        };
        push(
            0,
            EventPayload::ContextMaterialized {
                stats: ContextStats {
                    instructions_tokens: 3_000,
                    state_tokens: 3_300,
                    recent_tokens: 37_900,
                    recall_tokens: 1_500,
                    tools_tokens: 2_400,
                    total_tokens: 48_100,
                    budget_tokens: 243_808,
                    window_tokens: 256_000,
                    reserve_tokens: 12_192,
                    headroom_tokens: 195_708,
                    durable_events: 503,
                    episodes: 11,
                    estimated: true,
                    status: "bounded".into(),
                    ..ContextStats::default()
                },
            },
        );
        push(
            1,
            EventPayload::TaskStateUpdated {
                state: TaskState {
                    goal: "TTL cache implementation".into(),
                    required_validations: vec!["unit tests pass".into(), "clippy clean".into()],
                    completion: CompletionState::InProgress,
                    ..TaskState::default()
                },
            },
        );
        push(
            2,
            EventPayload::ModelUsage {
                usage: Usage {
                    input_tokens: 428_600,
                    output_tokens: 31_400,
                    cache_read_tokens: Some(286_100),
                    cache_write_tokens: None,
                    cache_miss_tokens: None,
                    reasoning_tokens: None,
                },
            },
        );
        push(
            3,
            EventPayload::GitStateObserved {
                head: None,
                dirty_paths: vec!["user.txt".into()],
            },
        );
        push(
            4,
            EventPayload::FileChanged {
                before: None,
                after: FileVersion {
                    path: "src/lib.rs".into(),
                    content_hash: "a".into(),
                    size: 1,
                },
                created: false,
                owner: ChangeOwner::Latch,
                undo_artifact: None,
                additions: 53,
                deletions: 7,
                preview: String::new(),
                call_id: None,
            },
        );
        push(
            5,
            EventPayload::FileChanged {
                before: None,
                after: FileVersion {
                    path: "src/gen.rs".into(),
                    content_hash: "b".into(),
                    size: 1,
                },
                created: false,
                owner: ChangeOwner::Shell,
                undo_artifact: None,
                additions: 2,
                deletions: 0,
                preview: String::new(),
                call_id: None,
            },
        );
        push(
            750,
            EventPayload::ModelRequestStarted {
                provider: "openai".into(),
                model: "deepseek-flash".into(),
            },
        );
        let mut priced = session();
        priced.pricing = Some(Pricing {
            input_per_million: Some(0.28),
            output_per_million: Some(0.42),
            cache_read_per_million: Some(0.028),
            cache_write_per_million: None,
            currency: "USD".into(),
        });
        let model = SidebarModel::from_events(priced, &events);
        assert_eq!(
            render(&model, 42, 40).trim_end(),
            include_str!("../tests/snapshots/v31_sidebar.txt")
                .replace("\r\n", "\n")
                .trim_end()
        );
    }

    fn render(model: &SidebarModel, width: u16, height: u16) -> String {
        model
            .render_lines(width, height)
            .into_iter()
            .map(|line| {
                line.spans
                    .into_iter()
                    .map(|span| span.content.into_owned())
                    .collect::<String>()
            })
            .collect::<Vec<_>>()
            .join("\n")
    }
    #[test]
    fn cache_metric_formulas_are_distinct_and_honest() {
        // Architecture cacheability: estimated share of the request that is an
        // exact reusable prefix, clamped at 100.
        assert_eq!(architecture_cacheability(7_500, 10_000), 75);
        assert_eq!(architecture_cacheability(20_000, 10_000), 100);
        assert_eq!(architecture_cacheability(4, 0), 100);
        // Provider prefix utilization: how much of the estimated prefix the
        // provider actually read; no prefix means nothing to reuse.
        assert_eq!(provider_prefix_utilization(6_000, 7_500), 80);
        assert_eq!(provider_prefix_utilization(1_000, 0), 0);
        // Measured hit rate: provider hit / (hit + miss); unknown categories
        // stay unknown and are never fabricated as zero.
        assert_eq!(measured_cache_hit_rate(286_100, 142_500), Some(66));
        assert_eq!(measured_cache_hit_rate(0, 10), Some(0));
        assert_eq!(measured_cache_hit_rate(0, 0), None);
    }
}

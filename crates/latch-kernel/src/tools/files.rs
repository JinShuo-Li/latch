//! Bounded filesystem reads: read_file, search, artifacts, and the window
//! policy that keeps large files out of the working set.

use super::*;

impl ToolExecutor {
    /// Records the observed version and detects external drift, shared by
    /// read-only inspection tools.
    async fn observe_file(&self, path: &Path, bytes: &[u8]) -> Result<FileVersion> {
        let version = version(&self.workspace, path, bytes)?;
        let previous = self
            .observations
            .lock()
            .await
            .insert(path.to_path_buf(), version.clone());
        if let Some(previous) = previous
            && previous.content_hash != version.content_hash
            && !self
                .ledger
                .lock()
                .await
                .owned
                .iter()
                .any(|change| change.path == path && change.after_hash == version.content_hash)
        {
            self.store.append(
                self.session_id,
                EventPayload::ExternalFileChangeDetected {
                    path: version.path.clone(),
                    expected_hash: previous.content_hash,
                    actual_hash: version.content_hash.clone(),
                },
            )?;
            self.ledger
                .lock()
                .await
                .externally_changed
                .insert(path.to_path_buf());
        }
        self.store.append(
            self.session_id,
            EventPayload::FileObserved {
                version: version.clone(),
            },
        )?;
        Ok(version)
    }

    pub(super) async fn read_file(&self, call: &ToolCall) -> Result<(String, Option<String>)> {
        let _permit = self.read_slots.acquire().await?;
        let path = self.path_arg(call)?;
        let bytes = tokio::fs::read(&path)
            .await
            .with_context(|| format!("read {}", path.display()))?;
        // Binary images are never dumped as text; point the model at the
        // dedicated image tool instead.
        if let Some(format) = crate::media::detect_format(&bytes) {
            bail!("{format} image; use read_image to inspect it");
        }
        if let Some(kind) = crate::media::detected_unsupported_kind(&bytes) {
            bail!("{kind} image; read_image supports PNG, JPEG, and WebP");
        }
        let version = self.observe_file(&path, &bytes).await?;
        let text = String::from_utf8(bytes).context("file is not UTF-8")?;
        let total = text.lines().count();
        let mut window = LineWindow::from_args(call, total, READ_DEFAULT_LINES, READ_MAX_LINES)?;
        let estimator = TokenEstimator::generic();
        let token_bounded = window.token_bound(&text, READ_MAX_TOKENS, &estimator);
        let selected = window.slice(&text);
        let mut out = format!("hash: {}\n", version.content_hash);
        out.push_str(&format!("[{}: {}]\n", version.path, window.describe(total)));
        out.push_str(&selected);
        if let Some(offset) = window.continue_offset(total) {
            if token_bounded {
                out.push_str(&format!(
                    "\n[token-bounded window; continue with offset={offset}]"
                ));
            } else {
                out.push_str(&format!("\n[continue with offset={offset}]"));
            }
        }
        Ok((out, None))
    }

    /// Ingests one workspace image into the immutable artifact store and
    /// returns it as tool media so the next model request contains the actual
    /// pixels. No OCR, no textual approximation: the original bytes are
    /// preserved exactly.
    pub(super) async fn read_image(
        &self,
        call: &ToolCall,
    ) -> Result<(String, Option<String>, Vec<MediaRef>)> {
        let _permit = self.read_slots.acquire().await?;
        let path = self.path_arg(call)?;
        let metadata = tokio::fs::metadata(&path)
            .await
            .with_context(|| format!("read {}", path.display()))?;
        crate::media::ensure_size(metadata.len())
            .with_context(|| format!("read {}", path.display()))?;
        let bytes = tokio::fs::read(&path)
            .await
            .with_context(|| format!("read {}", path.display()))?;
        let version = self.observe_file(&path, &bytes).await?;
        let media =
            crate::media::ingest_image_bytes(&self.artifacts, &bytes, Some(version.path.clone()))
                .with_context(|| format!("ingest {}", version.path))?;
        let mut out = format!("image: {}\n", media.compact_label());
        out.push_str(&format!(
            "path: {}\nsha256: {}\nbytes: {}\n",
            version.path, media.sha256, media.byte_len
        ));
        out.push_str(
            "The image is now attached to this conversation; inspect the actual pixels directly.",
        );
        Ok((out, None, vec![media]))
    }
    pub(super) async fn search(&self, call: &ToolCall) -> Result<(String, Option<String>)> {
        if let Some(message) = self.search_runtime_error() {
            bail!("{message}");
        }
        let _permit = self.read_slots.acquire().await?;
        let q = str_arg(call, "query")?;
        let target = call
            .arguments
            .get("path")
            .and_then(Value::as_str)
            .unwrap_or(".");
        let path = resolve_workspace_path(&self.workspace, target)?;
        let max_results = call
            .arguments
            .get("max_results")
            .and_then(Value::as_u64)
            .map_or(SEARCH_DEFAULT_RESULTS, |value| {
                (value as usize).clamp(1, SEARCH_MAX_RESULTS)
            });
        let offset = call
            .arguments
            .get("offset")
            .and_then(Value::as_u64)
            .map_or(0, |value| value as usize);
        let out = Command::new("rg")
            .args([
                "-n",
                "--color=never",
                "--no-heading",
                "--max-columns",
                "400",
                "--max-columns-preview",
                "--max-filesize",
                "8M",
                "-m",
                "1000",
                "--",
                q,
            ])
            .arg(path)
            .current_dir(&self.workspace)
            .output()
            .await?;
        if !out.status.success() && out.status.code() != Some(1) {
            bail!(
                "search failed with {}: {}",
                out.status,
                String::from_utf8_lossy(&out.stderr)
            );
        }
        let text = String::from_utf8_lossy(&out.stdout);
        let matches = text.lines().collect::<Vec<_>>();
        let total = matches.len();
        if offset >= total {
            return Ok((
                format!("{total} match(es); offset {offset} is past the end"),
                None,
            ));
        }
        let end = (offset + max_results).min(total);
        let mut result = format!(
            "{total} match(es); showing {}-{} of {total}\n{}",
            offset + 1,
            end,
            matches[offset..end].join("\n")
        );
        if end < total {
            result.push_str(&format!("\n[continue with offset={end}]"));
        }
        Ok((result, None))
    }
    pub(super) async fn read_artifact(&self, call: &ToolCall) -> Result<(String, Option<String>)> {
        let _permit = self.read_slots.acquire().await?;
        let id = str_arg(call, "id")?;
        let path = self.artifact_path(id)?;
        let bytes = tokio::fs::read(&path)
            .await
            .with_context(|| format!("read artifact {id}"))?;
        let text = String::from_utf8_lossy(&bytes);
        let window = LineWindow::from_args(
            call,
            text.lines().count(),
            ARTIFACT_DEFAULT_LINES,
            ARTIFACT_MAX_LINES,
        )?;
        let mut out = format!(
            "[artifact {id}: {}]\n",
            window.describe(text.lines().count())
        );
        out.push_str(&window.slice(&text));
        if let Some(offset) = window.continue_offset(text.lines().count()) {
            out.push_str(&format!("\n[continue with offset={offset}]"));
        }
        Ok((out, None))
    }
    pub(super) fn artifact_path(&self, id: &str) -> Result<PathBuf> {
        if id.is_empty()
            || id.contains("..")
            || id.contains('/')
            || id.contains('\\')
            || Path::new(id).is_absolute()
        {
            bail!("invalid artifact id");
        }
        let path = self.artifacts.join(id);
        let canonical = path
            .canonicalize()
            .with_context(|| format!("unknown artifact {id}"))?;
        let root = self.artifacts.canonicalize()?;
        if !canonical.starts_with(&root) {
            bail!("artifact path escapes the artifact store");
        }
        Ok(canonical)
    }
    pub(super) fn path_arg(&self, call: &ToolCall) -> Result<PathBuf> {
        resolve_workspace_path(&self.workspace, str_arg(call, "path")?)
    }
}

/// A bounded line window with head, offset/limit, and tail modes. Used by
/// `read_file` and `read_artifact` so no tool ever injects a whole file.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct LineWindow {
    start: usize,
    end: usize,
}

impl LineWindow {
    fn from_args(
        call: &ToolCall,
        total: usize,
        default_lines: usize,
        max_lines: usize,
    ) -> Result<Self> {
        if let Some(tail) = call.arguments.get("tail").and_then(Value::as_u64) {
            let tail = (tail as usize).clamp(1, max_lines);
            return Ok(Self {
                start: total.saturating_sub(tail),
                end: total,
            });
        }
        let offset = call
            .arguments
            .get("offset")
            .and_then(Value::as_u64)
            .map_or(0, |value| value.saturating_sub(1) as usize);
        let limit = call
            .arguments
            .get("limit")
            .and_then(Value::as_u64)
            .map_or(default_lines, |value| value as usize)
            .clamp(1, max_lines);
        let start = offset.min(total);
        let end = start.saturating_add(limit).min(total);
        Ok(Self { start, end })
    }

    /// Trims the window to whole lines that fit `max_tokens`, returning true
    /// when lines were dropped. The continuation offset then points at the
    /// first dropped line.
    fn token_bound(&mut self, text: &str, max_tokens: usize, estimator: &TokenEstimator) -> bool {
        let lines: Vec<&str> = text
            .lines()
            .skip(self.start)
            .take(self.end.saturating_sub(self.start))
            .collect();
        let mut used = 0usize;
        let mut keep = 0usize;
        for (index, line) in lines.iter().enumerate() {
            let cost = estimator.estimate(line).saturating_add(1);
            if index > 0 && used + cost > max_tokens {
                break;
            }
            used += cost;
            keep = index + 1;
        }
        if keep < lines.len() {
            self.end = self.start + keep;
            true
        } else {
            false
        }
    }

    fn slice(self, text: &str) -> String {
        text.lines()
            .skip(self.start)
            .take(self.end.saturating_sub(self.start))
            .collect::<Vec<_>>()
            .join("\n")
    }

    fn describe(self, total: usize) -> String {
        if total == 0 {
            return "empty".into();
        }
        if self.start >= total {
            return format!("no lines (offset past end of {total})");
        }
        format!("lines {}-{} of {total}", self.start + 1, self.end)
    }

    fn continue_offset(self, total: usize) -> Option<usize> {
        (self.end < total).then_some(self.end + 1)
    }
}

/// `read_file` defaults to a bounded window with continuation. Files are never
/// silently injected whole into context; explicit limits may be larger than the
/// default but stay bounded per call.
const READ_DEFAULT_LINES: usize = 400;

const READ_MAX_LINES: usize = 20_000;

/// Secondary token cap for one read window. A 400-line window is already small,
/// but dense code can still be large; whole lines are trimmed until the
/// selection fits, and the continuation offset keeps the rest reachable.
const READ_MAX_TOKENS: usize = 8_000;

/// `search` returns a bounded page with an offset continuation.
const SEARCH_DEFAULT_RESULTS: usize = 50;

const SEARCH_MAX_RESULTS: usize = 500;

/// `read_artifact` defaults to the same window as a file read.
const ARTIFACT_DEFAULT_LINES: usize = 2_000;

const ARTIFACT_MAX_LINES: usize = 20_000;

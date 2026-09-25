//! Bounded filesystem reads: read_file, search, artifacts, and the window
//! policy that keeps large files out of the working set.

use super::*;
use tokio::io::AsyncSeekExt;

/// Maximum source bytes fetched by one read_file or read_artifact call.
pub(super) const READ_IO_BYTES: usize = 64 * 1024;
/// Maximum decoded text held for a read page (UTF-8 never expands here).
const READ_TEXT_BYTES: usize = READ_IO_BYTES;
/// Includes headers and continuation instructions in the durable ToolResult.
pub(super) const READ_OUTPUT_BYTES: usize = 24 * 1024;
const READ_OUTPUT_TOKENS: usize = 8_000;
const READ_FOOTER_RESERVE: usize = 256;
const ARTIFACT_ID_MAX_BYTES: usize = 255;

pub(super) struct SourcePage {
    pub(super) text: String,
    base: u64,
    size: u64,
}

pub(super) async fn source_page(path: &Path, start: u64) -> Result<SourcePage> {
    let mut file = tokio::fs::File::open(path).await?;
    let size = file.metadata().await?.len();
    let mut base = start.min(size);
    file.seek(std::io::SeekFrom::Start(base)).await?;
    let mut bytes = Vec::with_capacity(READ_IO_BYTES);
    file.take(READ_IO_BYTES as u64)
        .read_to_end(&mut bytes)
        .await?;
    // Tail seeks can start inside a multibyte scalar. Advancing at most three
    // bytes avoids treating a valid text file as malformed.
    if base > 0 {
        let skip = bytes
            .iter()
            .take(3)
            .take_while(|byte| **byte & 0b1100_0000 == 0b1000_0000)
            .count();
        bytes.drain(..skip);
        base += skip as u64;
    }
    if base == 0 {
        if let Some(format) = crate::media::detect_format(&bytes) {
            bail!("{format} image; use read_image to inspect it");
        }
        if let Some(kind) = crate::media::detected_unsupported_kind(&bytes) {
            bail!("{kind} image; read_image supports PNG, JPEG, and WebP");
        }
    }
    if bytes
        .iter()
        .any(|byte| *byte == 0x7f || (*byte < 32 && !matches!(*byte, b'\n' | b'\r' | b'\t')))
    {
        bail!("binary input; read_file and read_artifact require text");
    }
    // A page may end in the middle of a UTF-8 scalar. The next page resumes
    // at the first unread byte; malformed input inside the page is rejected.
    let valid = match std::str::from_utf8(&bytes) {
        Ok(_) => bytes.len(),
        Err(error) if error.error_len().is_none() && base + (bytes.len() as u64) < size => {
            error.valid_up_to()
        }
        Err(_) => bail!("input is not UTF-8"),
    };
    bytes.truncate(valid);
    debug_assert!(bytes.len() <= READ_TEXT_BYTES);
    Ok(SourcePage {
        text: String::from_utf8(bytes)?,
        base,
        size,
    })
}

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
        let label = relative(&self.workspace, &path)?;
        let output = self
            .read_text_window(
                call,
                &path,
                &label,
                READ_DEFAULT_LINES,
                READ_MAX_LINES,
                true,
            )
            .await
            .with_context(|| format!("read {}", path.display()))?;
        Ok((output, None))
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
        self.ensure_not_protected(&path)?;
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
        let mut command = Command::new("rg");
        command.args([
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
        ]);
        // A recursive search from a parent of the state directory must not
        // traverse it; exclude the protected tree instead of refusing the
        // search outright.
        if let Some(exclusion) = self.state_dir_exclusion_glob() {
            command.arg("--glob").arg(exclusion);
        }
        let out = command
            .args(["--", q])
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
        let output = self
            .read_text_window(
                call,
                &path,
                &format!("artifact {id}"),
                ARTIFACT_DEFAULT_LINES,
                ARTIFACT_MAX_LINES,
                false,
            )
            .await
            .with_context(|| format!("read artifact {id}"))?;
        Ok((output, None))
    }
    async fn read_text_window(
        &self,
        call: &ToolCall,
        path: &Path,
        label: &str,
        default_lines: usize,
        max_lines: usize,
        observe: bool,
    ) -> Result<String> {
        let size = tokio::fs::metadata(path).await?.len();
        let cursor = call.arguments.get("byte_offset").and_then(Value::as_u64);
        let tail = if cursor.is_some() {
            None
        } else {
            call.arguments.get("tail").and_then(Value::as_u64)
        };
        let base = if let Some(cursor) = cursor {
            cursor
        } else if tail.is_some() && size > READ_IO_BYTES as u64 {
            size - READ_IO_BYTES as u64
        } else {
            0
        };
        let page = source_page(path, base).await?;
        let complete = page.base == 0 && page.text.len() as u64 == page.size;
        let hash_header = if observe && complete {
            let version = self.observe_file(path, page.text.as_bytes()).await?;
            format!("hash: {}\n", version.content_hash)
        } else if observe {
            "hash: unavailable on bounded page (guarded edits require an exact hash)\n".to_owned()
        } else {
            String::new()
        };
        if complete && cursor.is_none() {
            let total = page.text.lines().count();
            let window = LineWindow::from_args(call, total, default_lines, max_lines)?;
            let start = line_byte(&page.text, window.start);
            return render_page(
                &page,
                start,
                window.start + 1,
                window.end - window.start,
                Some(total),
                label,
                &hash_header,
            );
        }
        let limit = tail
            .map_or_else(
                || {
                    call.arguments
                        .get("limit")
                        .and_then(Value::as_u64)
                        .map_or(default_lines, |n| n.min(max_lines as u64) as usize)
                },
                |n| n.min(max_lines as u64) as usize,
            )
            .clamp(1, max_lines);
        let requested = call
            .arguments
            .get("offset")
            .and_then(Value::as_u64)
            .unwrap_or(1)
            .max(1);
        let mut line = if cursor.is_some() {
            call.arguments
                .get("cursor_line")
                .and_then(Value::as_u64)
                .unwrap_or(requested)
                .max(1)
        } else {
            1
        };
        let mut start = 0;
        if tail.is_some() {
            if page.base > 0 {
                // The first bytes may be the middle of a line or UTF-8 scalar.
                start = page.text.find('\n').map_or(0, |pos| pos + 1);
            }
            let suffix = &page.text[start..];
            let total = suffix.lines().count();
            let skip = total.saturating_sub(limit);
            start += line_byte(suffix, skip);
            line = 1;
        } else {
            while line < requested && start < page.text.len() {
                let Some(next) = page.text[start..].find('\n') else {
                    break;
                };
                start += next + 1;
                line += 1;
            }
            if line < requested {
                if page.base + (page.text.len() as u64) < page.size {
                    return Ok(format!(
                        "[{label}: scanning to line {requested}]\n[continue with offset={requested}, byte_offset={}, cursor_line={line}]",
                        page.base + page.text.len() as u64
                    ));
                }
                return Ok(format!("[{label}: no lines (offset past end)]"));
            }
        }
        render_page(
            &page,
            start,
            line as usize,
            limit,
            None,
            label,
            &hash_header,
        )
    }
    pub(super) fn artifact_path(&self, id: &str) -> Result<PathBuf> {
        if id.is_empty()
            || id.len() > ARTIFACT_ID_MAX_BYTES
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
        let path = resolve_workspace_path(&self.workspace, str_arg(call, "path")?)?;
        self.ensure_not_protected(&path)?;
        Ok(path)
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
}

fn line_byte(text: &str, skip: usize) -> usize {
    let mut pos = 0;
    for _ in 0..skip {
        let Some(next) = text[pos..].find('\n') else {
            return text.len();
        };
        pos += next + 1;
    }
    pos
}

/// The same final budget is used for workspace files and durable artifacts.
/// It is applied before ToolResult construction and event persistence.
fn render_page(
    page: &SourcePage,
    start: usize,
    first_line: usize,
    limit: usize,
    total: Option<usize>,
    label: &str,
    hash_header: &str,
) -> Result<String> {
    let overhead = label
        .len()
        .saturating_add(hash_header.len())
        .saturating_add(READ_FOOTER_RESERVE);
    if overhead >= READ_OUTPUT_BYTES {
        bail!("read result header exceeds output budget");
    }
    let body_budget = READ_OUTPUT_BYTES - overhead;
    let estimator = TokenEstimator::generic();
    let header_tokens = estimator.estimate(label) + estimator.estimate(hash_header) + 128;
    let mut body = String::new();
    let mut pos = start;
    let mut shown = 0;
    let mut partial = false;
    let mut bounded = false;
    let mut tokens = 0;
    while shown < limit && pos < page.text.len() {
        let rest = &page.text[pos..];
        let newline = rest.find('\n');
        let raw_end = newline.unwrap_or(rest.len());
        let line = rest[..raw_end]
            .strip_suffix('\r')
            .unwrap_or(&rest[..raw_end]);
        let separator = usize::from(shown > 0);
        let capacity = body_budget.saturating_sub(body.len() + separator);
        let remaining_tokens = READ_OUTPUT_TOKENS
            .saturating_sub(header_tokens)
            .saturating_sub(tokens + separator);
        let whole = line.len() <= capacity && estimator.estimate(line) <= remaining_tokens;
        if !whole && shown > 0 {
            bounded = true;
            break;
        }
        if shown > 0 {
            body.push('\n');
            tokens += 1;
        }
        if whole {
            body.push_str(line);
            tokens += estimator.estimate(line);
            pos += raw_end + usize::from(newline.is_some());
            shown += 1;
            if newline.is_none() && page.base + (pos as u64) < page.size {
                partial = true;
                break;
            }
        } else {
            // Binary search over character boundaries so one enormous line
            // cannot bypass either the byte or estimated-token budget.
            let boundaries: Vec<usize> = line
                .char_indices()
                .map(|(i, _)| i)
                .chain(std::iter::once(line.len()))
                .collect();
            let mut lo = 0;
            let mut hi = boundaries.len();
            while lo + 1 < hi {
                let mid = (lo + hi) / 2;
                let n = boundaries[mid];
                if n <= capacity && estimator.estimate(&line[..n]) <= remaining_tokens {
                    lo = mid;
                } else {
                    hi = mid;
                }
            }
            let n = boundaries[lo];
            if n == 0 {
                bail!("read output budget cannot fit one text character");
            }
            body.push_str(&line[..n]);
            pos += n;
            shown += 1;
            partial = true;
            bounded = true;
            break;
        }
    }
    let last_line = first_line.saturating_add(shown).saturating_sub(1);
    let description = match (shown, total) {
        (0, Some(0)) => "empty".to_owned(),
        (0, Some(n)) => format!("no lines (offset past end of {n})"),
        (0, None) => "no lines".to_owned(),
        (_, Some(n)) => format!("lines {first_line}-{last_line} of {n}"),
        (_, None) => format!("lines {first_line}-{last_line} (total unknown)"),
    };
    let mut out = format!("{hash_header}[{label}: {description}]\n{body}");
    let more =
        partial || total.is_some_and(|n| last_line < n) || page.base + (pos as u64) < page.size;
    if more {
        let offset = if partial { last_line } else { last_line + 1 };
        let prefix = if bounded {
            "token-bounded window; "
        } else {
            ""
        };
        if total.is_some() && !partial {
            out.push_str(&format!("\n[{prefix}continue with offset={offset}]"));
        } else {
            out.push_str(&format!(
                "\n[{prefix}continue with offset={offset}, byte_offset={}, cursor_line={offset}]",
                page.base + pos as u64
            ));
        }
    }
    if out.len() > READ_OUTPUT_BYTES || estimator.estimate(&out) > READ_OUTPUT_TOKENS {
        bail!("read result header exceeds output budget");
    }
    Ok(out)
}

/// Final guard for both successful and failed read calls, before ToolResult is
/// created and appended to the event log. Normal pages already fit exactly.
pub(super) fn bound_final_output(output: &str) -> String {
    let estimator = TokenEstimator::generic();
    if output.len() <= READ_OUTPUT_BYTES && estimator.estimate(output) <= READ_OUTPUT_TOKENS {
        return output.to_owned();
    }
    const NOTE: &str = "\n[read output bounded]";
    let max_bytes = READ_OUTPUT_BYTES - NOTE.len();
    let boundaries: Vec<usize> = output
        .char_indices()
        .map(|(i, _)| i)
        .chain(std::iter::once(output.len()))
        .collect();
    let mut lo = 0;
    let mut hi = boundaries.len();
    while lo + 1 < hi {
        let mid = (lo + hi) / 2;
        let n = boundaries[mid];
        if n <= max_bytes
            && estimator.estimate(&output[..n]) + estimator.estimate(NOTE) <= READ_OUTPUT_TOKENS
        {
            lo = mid;
        } else {
            hi = mid;
        }
    }
    format!("{}{}", &output[..boundaries[lo]], NOTE)
}

/// `read_file` defaults to a bounded window with continuation. Files are never
/// silently injected whole into context; explicit limits may be larger than the
/// default but stay bounded per call.
const READ_DEFAULT_LINES: usize = 400;

const READ_MAX_LINES: usize = 20_000;

/// `search` returns a bounded page with an offset continuation.
const SEARCH_DEFAULT_RESULTS: usize = 50;

const SEARCH_MAX_RESULTS: usize = 500;

/// `read_artifact` defaults to the same window as a file read.
const ARTIFACT_DEFAULT_LINES: usize = 2_000;

const ARTIFACT_MAX_LINES: usize = 20_000;

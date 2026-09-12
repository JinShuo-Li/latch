//! Change ledger, drift detection, undo/checkpoint, and resume-time
//! ownership reconstruction. One module owns the ledger lock-order invariant.

use super::*;

impl ToolExecutor {
    pub async fn preexisting_change_count(&self) -> usize {
        self.ledger.lock().await.initial.len()
    }
    pub async fn latch_change_count(&self) -> usize {
        self.ledger.lock().await.owned.len()
    }
    /// Rebuilds durable change ownership from the event log after a resume.
    /// Entries whose post-change hash no longer matches the file stay in the
    /// ledger but `/undo` refuses them, exactly like an external edit in a live
    /// session.
    pub async fn restore_ownership(&self) -> Result<usize> {
        if self
            .restored
            .swap(true, std::sync::atomic::Ordering::Relaxed)
        {
            return Ok(0);
        }
        let events = self.store.events(self.session_id)?;
        let mut restored = 0;
        let mut ledger = self.ledger.lock().await;
        for event in &events {
            match &event.payload {
                EventPayload::FileChanged {
                    after,
                    owner,
                    undo_artifact,
                    ..
                } if !matches!(owner, ChangeOwner::External) => {
                    let path = self.workspace.join(&after.path);
                    let record = ChangeRecord {
                        path,
                        before: None,
                        after_hash: after.content_hash.clone(),
                        owner: owner.clone(),
                        undo_artifact: undo_artifact.clone(),
                    };
                    // A change whose result is no longer on disk was externally
                    // modified after the fact; keep it distinguishable.
                    if tokio::fs::read(&record.path)
                        .await
                        .map(|bytes| hash(&bytes) != after.content_hash)
                        .unwrap_or(true)
                    {
                        ledger.externally_changed.insert(record.path.clone());
                    }
                    self.note_self_authored(&record.path, &after.content_hash);
                    ledger.owned.push(record);
                    restored += 1;
                }
                EventPayload::ChangeReverted { content_hash, path } => {
                    // The revert tombstone removes the original entry so a
                    // resumed ledger never re-applies undone work.
                    let absolute = self.workspace.join(path);
                    if let Some(position) = ledger.owned.iter().position(|record| {
                        record.path == absolute && record.after_hash == *content_hash
                    }) {
                        ledger.owned.remove(position);
                    }
                }
                _ => {}
            }
        }
        Ok(restored)
    }
    /// Records a content hash Latch itself produced, so later guarded edits
    /// can recognize their own drift instead of demanding a re-read.
    pub(super) fn note_self_authored(&self, path: &Path, content_hash: &str) {
        if let Ok(mut authored) = self.self_authored.lock() {
            authored
                .entry(path.to_path_buf())
                .or_default()
                .insert(content_hash.to_owned());
        }
    }
    pub(super) fn is_self_authored(&self, path: &Path, content_hash: &str) -> bool {
        self.self_authored
            .lock()
            .map(|authored| {
                authored
                    .get(path)
                    .is_some_and(|hashes| hashes.contains(content_hash))
            })
            .unwrap_or(false)
    }
    pub(super) async fn commit_change(
        &self,
        path: PathBuf,
        before: Option<Vec<u8>>,
        after: Vec<u8>,
        owner: ChangeOwner,
        call_id: Option<&str>,
    ) -> Result<(String, Option<String>)> {
        if let Some(parent) = path.parent() {
            tokio::fs::create_dir_all(parent).await?;
        }
        let before_version = before
            .as_ref()
            .map(|b| version(&self.workspace, &path, b))
            .transpose()?;
        let undo_artifact = before
            .as_ref()
            .map(|bytes| self.store_undo_artifact(bytes))
            .transpose()?;
        let operation = self.store.begin_operation(
            self.session_id,
            &format!("guarded write: {}", path.display()),
        )?;
        if let Err(error) = tokio::fs::write(&path, &after).await {
            self.store.finish_operation(operation)?;
            return Err(error.into());
        }
        let after_version = version(&self.workspace, &path, &after)?;
        self.observations
            .lock()
            .await
            .insert(path.clone(), after_version.clone());
        self.note_self_authored(&path, &after_version.content_hash);
        let (additions, deletions) = line_delta(before.as_deref().unwrap_or_default(), &after);
        let preview = relative(&self.workspace, &path)
            .map(|relative_path| {
                crate::linediff::unified_diff(
                    &relative_path,
                    before.as_deref(),
                    Some(&after),
                    DIFF_PREVIEW_LINES,
                )
            })
            .unwrap_or_default();
        self.ledger.lock().await.owned.push(ChangeRecord {
            path: path.clone(),
            before,
            after_hash: after_version.content_hash.clone(),
            owner: owner.clone(),
            undo_artifact: undo_artifact.clone(),
        });
        self.store.append(
            self.session_id,
            EventPayload::FileChanged {
                before: before_version,
                after: after_version.clone(),
                owner,
                undo_artifact,
                additions,
                deletions,
                preview,
                call_id: call_id.map(str::to_owned),
            },
        )?;
        self.store.finish_operation(operation)?;
        Ok((
            format!(
                "updated {} @ {}",
                after_version.path, after_version.content_hash
            ),
            None,
        ))
    }
    /// Content-addressed artifact holding pre-change bytes so ownership and
    /// undo survive process restart without storing file bodies in event JSON.
    pub(super) fn store_undo_artifact(&self, bytes: &[u8]) -> Result<String> {
        let name = format!("changes/{}.before", hash(bytes));
        let path = self.artifacts.join(&name);
        if !path.exists() {
            if let Some(parent) = path.parent() {
                std::fs::create_dir_all(parent)?;
            }
            std::fs::write(path, bytes)?;
        }
        Ok(name)
    }
    /// Captures the pre-command state of dirty and untracked files for drift
    /// classification. `Ok(None)` means the workspace is not a Git worktree and
    /// honest drift detection is unavailable.
    pub(super) async fn snapshot_dirty(&self) -> Result<Option<Vec<(PathBuf, Vec<u8>)>>> {
        let listing = git_porcelain(&self.workspace)?;
        let Some(listing) = listing else {
            return Ok(None);
        };
        let mut captured = Vec::new();
        let mut total = 0usize;
        for path in listing.iter().take(DRIFT_MAX_PATHS) {
            let Ok(meta) = tokio::fs::metadata(path).await else {
                continue;
            };
            if !meta.is_file() || meta.len() > DRIFT_MAX_FILE_BYTES {
                continue;
            }
            let Ok(bytes) = tokio::fs::read(path).await else {
                continue;
            };
            total += bytes.len();
            if total > DRIFT_MAX_TOTAL_BYTES {
                break;
            }
            captured.push((path.clone(), bytes));
        }
        Ok(Some(captured))
    }
    /// Classifies paths a shell command mutated as shell-originated change.
    ///
    /// Git workspaces: pre-change content comes from the captured dirty-file
    /// snapshot, or from `HEAD` for files that were clean (and therefore
    /// identical to `HEAD`) before the command. Reversible paths become
    /// undoable ledger entries owned by `Shell`; anything Latch could not
    /// capture is recorded as an explicitly non-reversible mutation. Non-Git
    /// workspaces get an honest "detection unavailable" marker instead of a
    /// pretense that nothing changed.
    pub(super) async fn classify_drift(
        &self,
        command: &str,
        before: Option<&[(PathBuf, Vec<u8>)]>,
    ) {
        let mark_unavailable = |executor: &Self| {
            let _ = executor.store.append(
                executor.session_id,
                EventPayload::ShellMutationObserved {
                    command: command.into(),
                    reversible: false,
                    paths: vec![],
                },
            );
        };
        let Some(before) = before else {
            mark_unavailable(self);
            return;
        };
        let Ok(listing) = git_porcelain(&self.workspace) else {
            mark_unavailable(self);
            return;
        };
        let listing = listing.unwrap_or_default();
        let before_paths: HashSet<&Path> = before.iter().map(|(path, _)| path.as_path()).collect();
        let after_paths: HashSet<&Path> = listing.iter().map(|path| path.as_path()).collect();
        let mut candidates: Vec<PathBuf> = listing
            .iter()
            .filter(|path| !before_paths.contains(path.as_path()))
            .cloned()
            .collect();
        candidates.extend(
            before
                .iter()
                .filter(|(path, _)| !after_paths.contains(path.as_path()))
                .map(|(path, _)| path.clone()),
        );
        // Files that were already dirty before the command count as changed
        // when their captured content no longer matches disk.
        for (path, bytes) in before {
            if after_paths.contains(path.as_path())
                && before_paths.contains(path.as_path())
                && tokio::fs::read(path)
                    .await
                    .map(|current| hash(&current) != hash(bytes))
                    .unwrap_or(true)
            {
                candidates.push(path.clone());
            }
        }
        candidates.sort();
        candidates.dedup();
        let mut tracked = 0;
        let mut untrackable: Vec<String> = Vec::new();
        for path in candidates {
            let display =
                || relative(&self.workspace, &path).unwrap_or_else(|_| path.display().to_string());
            if tracked >= DRIFT_MAX_TRACKED {
                untrackable.push(display());
                continue;
            }
            let current = tokio::fs::read(&path).await.ok();
            let before_bytes = match before.iter().find(|(candidate, _)| **candidate == path) {
                Some((_, bytes)) => Some(bytes.clone()),
                None => {
                    // The file was clean before the command: its worktree
                    // content was exactly the HEAD version. A path with no
                    // HEAD blob that now exists was created by the command and
                    // is reversible by deletion (before = None).
                    match git_show_head(&self.workspace, &path) {
                        Some(bytes) => Some(bytes),
                        None if current.is_some() => None,
                        None => {
                            untrackable.push(display());
                            continue;
                        }
                    }
                }
            };
            let changed = match (&before_bytes, &current) {
                (Some(before), Some(current)) => hash(before) != hash(current),
                (Some(_), None) | (None, Some(_)) => true,
                (None, None) => false,
            };
            if !changed {
                continue;
            }
            let Some(after_bytes) = current else {
                // The command deleted a workspace path. Deletion has no
                // post-change file version to record, so it is reported
                // honestly as a non-reversible mutation.
                untrackable.push(display());
                continue;
            };
            tracked += 1;
            let Ok(after) = version(&self.workspace, &path, &after_bytes) else {
                untrackable.push(display());
                continue;
            };
            let before_version = before_bytes
                .as_ref()
                .map(|bytes| version(&self.workspace, &path, bytes))
                .transpose()
                .ok()
                .flatten();
            let undo_artifact = before_bytes
                .as_ref()
                .and_then(|bytes| self.store_undo_artifact(bytes).ok());
            let (additions, deletions) =
                line_delta(before_bytes.as_deref().unwrap_or_default(), &after_bytes);
            self.note_self_authored(&path, &after.content_hash);
            let preview = relative(&self.workspace, &path)
                .map(|relative_path| {
                    crate::linediff::unified_diff(
                        &relative_path,
                        before_bytes.as_deref(),
                        Some(&after_bytes),
                        DIFF_PREVIEW_LINES,
                    )
                })
                .unwrap_or_default();
            self.ledger.lock().await.owned.push(ChangeRecord {
                path: path.clone(),
                before: before_bytes,
                after_hash: after.content_hash.clone(),
                owner: ChangeOwner::Shell,
                undo_artifact: undo_artifact.clone(),
            });
            let _ = self.store.append(
                self.session_id,
                EventPayload::FileChanged {
                    before: before_version,
                    after,
                    owner: ChangeOwner::Shell,
                    undo_artifact,
                    additions,
                    deletions,
                    preview,
                    call_id: None,
                },
            );
        }
        if !untrackable.is_empty() {
            let _ = self.store.append(
                self.session_id,
                EventPayload::ShellMutationObserved {
                    command: command.into(),
                    reversible: false,
                    paths: untrackable,
                },
            );
        }
    }
    pub(super) async fn checkpoint(&self, _: &ToolCall) -> Result<(String, Option<String>)> {
        let mut l = self.ledger.lock().await;
        let id = Uuid::new_v4();
        let position = l.owned.len();
        l.checkpoints.push((id, position));
        self.store.append(
            self.session_id,
            EventPayload::CheckpointCreated {
                id,
                label: "manual".into(),
            },
        )?;
        Ok((id.to_string(), None))
    }
    pub(super) async fn undo(&self, _: &ToolCall) -> Result<(String, Option<String>)> {
        let _guard = self.mutation_lock.lock().await;
        // Peek before acting: a refused undo must not silently drop the change
        // from the ledger.
        let change = self
            .ledger
            .lock()
            .await
            .owned
            .last()
            .cloned()
            .ok_or_else(|| anyhow!("no Latch-owned change to undo"))?;
        let current = tokio::fs::read(&change.path).await.ok();
        if current.as_deref().map(hash).as_deref() != Some(&change.after_hash) {
            bail!(
                "cannot undo: {} changed since the {} edit",
                relative(&self.workspace, &change.path)
                    .unwrap_or_else(|_| change.path.display().to_string()),
                match change.owner {
                    ChangeOwner::Shell => "shell",
                    _ => "Latch",
                }
            );
        }
        let before = match &change.before {
            Some(bytes) => Some(bytes.clone()),
            None => match &change.undo_artifact {
                Some(artifact) => Some(
                    tokio::fs::read(self.artifacts.join(artifact))
                        .await
                        .with_context(|| format!("read undo artifact {artifact}"))?,
                ),
                None => None,
            },
        };
        match before {
            Some(bytes) => tokio::fs::write(&change.path, bytes).await?,
            None => tokio::fs::remove_file(&change.path).await?,
        }
        let relative_path = relative(&self.workspace, &change.path)?;
        // The bytes restored (or removed) are Latch-authored too.
        if let Ok(current) = tokio::fs::read(&change.path).await {
            self.note_self_authored(&change.path, &hash(&current));
        }
        self.ledger.lock().await.owned.pop();
        // Tombstone so a resumed session never restores the undone entry, plus
        // the durable audit record of the revert itself.
        self.store.append(
            self.session_id,
            EventPayload::ChangeReverted {
                path: relative_path.clone(),
                content_hash: change.after_hash,
            },
        )?;
        Ok((format!("undid {relative_path}"), None))
    }
}

#[derive(Debug, Clone)]
pub(super) struct ChangeRecord {
    pub(super) path: PathBuf,
    pub(super) before: Option<Vec<u8>>,
    pub(super) after_hash: String,
    pub(super) owner: ChangeOwner,
    /// Session artifact holding the pre-change bytes for resume-safe undo.
    pub(super) undo_artifact: Option<String>,
}

#[derive(Debug, Default)]
pub(super) struct ChangeLedger {
    initial: HashMap<PathBuf, String>,
    /// Chronological Latch- and shell-owned changes. `/undo` reverts the newest
    /// eligible entry.
    pub(super) owned: Vec<ChangeRecord>,
    pub(super) externally_changed: HashSet<PathBuf>,
    checkpoints: Vec<(Uuid, usize)>,
}

impl ChangeLedger {
    /// Builds the initial (pre-session) dirty snapshot captured at startup.
    pub(super) fn with_initial(initial: HashMap<PathBuf, String>) -> Self {
        Self {
            initial,
            ..Default::default()
        }
    }
}

/// Bounded capture limits for shell-drift detection. Drift snapshots never
/// attempt to copy a whole repository: dirty-file reads and restored pre-content
/// are capped so large trees stay safe.
const DRIFT_MAX_PATHS: usize = 256;

const DRIFT_MAX_TRACKED: usize = 64;

const DRIFT_MAX_FILE_BYTES: u64 = 1024 * 1024;

const DRIFT_MAX_TOTAL_BYTES: usize = 16 * 1024 * 1024;

/// Upper bound on the unified-diff preview stored with a change. The full
/// workspace diff remains available through `git_diff` and `/diff`.
const DIFF_PREVIEW_LINES: usize = 160;

pub(super) fn git_dirty_hashes(workspace: &Path) -> Result<HashMap<PathBuf, String>> {
    let Some(paths) = git_porcelain(workspace)? else {
        return Ok(HashMap::new());
    };
    let mut map = HashMap::new();
    for path in paths.iter().take(DRIFT_MAX_PATHS) {
        if let Ok(bytes) = std::fs::read(path) {
            map.insert(path.clone(), hash(&bytes));
        }
    }
    Ok(map)
}

/// `git status --porcelain -z --untracked-files=all` parsed into workspace
/// paths, or `None` when the workspace is not inside a Git worktree.
pub(super) fn git_porcelain(workspace: &Path) -> Result<Option<Vec<PathBuf>>> {
    let out = std::process::Command::new("git")
        .args(["status", "--porcelain", "-z", "--untracked-files=all"])
        .current_dir(workspace)
        .output()?;
    if !out.status.success() {
        return Ok(None);
    }
    let mut paths = Vec::new();
    for item in out.stdout.split(|b| *b == 0).filter(|s| !s.is_empty()) {
        if item.len() > 3 {
            let entry = String::from_utf8_lossy(&item[3..]);
            // Rename entries read "to -> from"; the worktree path is the target.
            let path = entry
                .split_once(" -> ")
                .map_or(entry.as_ref(), |(_, target)| target);
            paths.push(workspace.join(path));
        }
    }
    Ok(Some(paths))
}

/// The exact HEAD blob for a path, used as pre-change content for files that
/// were clean before a shell command. This reads Git data; it never rewrites
/// history or resets the worktree.
pub(super) fn git_show_head(workspace: &Path, path: &Path) -> Option<Vec<u8>> {
    let rel = path.strip_prefix(workspace).ok()?;
    let out = std::process::Command::new("git")
        .arg("show")
        .arg(format!("HEAD:{}", rel.to_string_lossy()))
        .current_dir(workspace)
        .output()
        .ok()?;
    out.status.success().then_some(out.stdout)
}

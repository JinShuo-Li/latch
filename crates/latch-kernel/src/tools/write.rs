//! Guarded workspace writes: exact replacement and whole-file writes with
//! base-hash freshness and durable change ownership.

use super::*;

impl ToolExecutor {
    pub(super) async fn patch(&self, call: &ToolCall) -> Result<(String, Option<String>)> {
        let _guard = self.mutation_lock.lock().await;
        let path = self.write_path_arg(call)?;
        let base = str_arg(call, "base_hash")?;
        let old = str_arg(call, "old")?;
        let new = str_arg(call, "new")?;
        let before = tokio::fs::read(&path).await?;
        self.ensure_fresh(&path, &before, base).await?;
        let text = String::from_utf8(before.clone())?;
        let occurrences = text.matches(old).count();
        if occurrences != 1 {
            bail!("expected exactly one match, found {occurrences}");
        }
        let updated = text.replacen(old, new, 1).into_bytes();
        self.commit_change(
            path,
            Some(before),
            updated,
            ChangeOwner::Latch,
            Some(&call.id),
        )
        .await
    }
    pub(super) async fn write(&self, call: &ToolCall) -> Result<(String, Option<String>)> {
        let _guard = self.mutation_lock.lock().await;
        let path = self.write_path_arg(call)?;
        let content = str_arg(call, "content")?.as_bytes().to_vec();
        let before = tokio::fs::read(&path).await.ok();
        match (
            &before,
            call.arguments.get("base_hash").and_then(Value::as_str),
        ) {
            (Some(bytes), Some(base)) => self.ensure_fresh(&path, bytes, base).await?,
            (Some(_), None) => bail!("existing file requires base_hash from read_file"),
            (None, Some(_)) => bail!("new file must not provide base_hash"),
            (None, None) => {}
        }
        self.commit_change(path, before, content, ChangeOwner::Latch, Some(&call.id))
            .await
    }
    async fn ensure_fresh(&self, path: &Path, bytes: &[u8], base: &str) -> Result<()> {
        let actual = hash(bytes);
        // Drift onto a hash Latch itself wrote is self-authored: the guarded
        // edit proceeds against current content without a forced re-read.
        // Genuine external modification still fails below.
        if actual != base && !self.is_self_authored(path, &actual) {
            self.store.append(
                self.session_id,
                EventPayload::ExternalFileChangeDetected {
                    path: relative(&self.workspace, path)?,
                    expected_hash: base.into(),
                    actual_hash: actual.clone(),
                },
            )?;
            self.ledger
                .lock()
                .await
                .externally_changed
                .insert(path.to_path_buf());
            bail!("stale observation: expected {base}, found {actual}; re-read before editing");
        }
        Ok(())
    }
    /// Write paths honor an explicit outside-workspace approval: the user saw
    /// the exact call and approved it, so the write may target an absolute path
    /// outside the workspace. Without approval the normal containment rules
    /// apply and the call never reaches this point.
    fn write_path_arg(&self, call: &ToolCall) -> Result<PathBuf> {
        let raw = str_arg(call, "path")?;
        let candidate = if Path::new(raw).is_absolute() {
            lexical_normalize(Path::new(raw))
        } else {
            lexical_normalize(&self.workspace.join(raw))
        };
        if resolve_workspace_path(&self.workspace, raw).is_ok() {
            return Ok(candidate);
        }
        // Outside the workspace: only a scoped, single-use grant allows it.
        if self.grant_allows_path(&call.id, &candidate) {
            return Ok(candidate);
        }
        bail!("path escapes workspace and no capability grant covers it")
    }
}

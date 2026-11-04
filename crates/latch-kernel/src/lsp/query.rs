//! Document snapshots and model-facing presentation. Never expose raw server
//! edits/commands, external file URIs or arbitrary result payloads.
use super::*;
use tokio::io::AsyncReadExt;

pub(super) fn checked_path(scope: &Scope, raw: &str) -> Result<PathBuf> {
    if raw.len() > 4096 || raw.contains('\0') {
        bail!("invalid or oversized LSP path");
    }
    let path = crate::workspace_path::resolve_workspace_path(&scope.workspace, raw)?;
    let alias = crate::workspace_path::file_alias_may_escape(&path);
    #[cfg(unix)]
    let alias = {
        use std::os::unix::fs::MetadataExt;
        alias || std::fs::metadata(&path).is_ok_and(|m| m.is_file() && m.nlink() > 1)
    };
    if alias {
        bail!("LSP refuses file aliases/hardlinks");
    }
    let mut protected = crate::sandbox::secret_paths(&dirs::home_dir().unwrap_or_default());
    protected.push(scope.state_dir.clone());
    if protected.iter().any(|root| {
        crate::workspace_path::resolve_real_path(root).is_some_and(|root| path.starts_with(root))
    }) {
        bail!("LSP refuses protected state/credential paths");
    }
    Ok(path)
}
async fn source(scope: &Scope, path: &Path) -> Result<String> {
    let checked = checked_path(scope, path.to_str().context("LSP path is not UTF-8")?)?;
    let mut bytes = Vec::new();
    tokio::fs::File::open(&checked)
        .await?
        .take((SOURCE_BYTES + 1) as u64)
        .read_to_end(&mut bytes)
        .await?;
    if bytes.len() > SOURCE_BYTES {
        bail!("LSP document exceeds 1 MiB; use bounded read_file/search");
    }
    if checked_path(scope, path.to_str().context("invalid path")?)? != checked {
        bail!("LSP file changed identity during read");
    }
    let text = String::from_utf8(bytes).context("LSP document must be UTF-8")?;
    if text.contains('\0') {
        bail!("LSP refuses binary documents");
    }
    Ok(text)
}
fn uri(path: &Path) -> Result<String> {
    Ok(reqwest::Url::from_file_path(path)
        .map_err(|()| anyhow::anyhow!("invalid file URI"))?
        .to_string())
}
fn uri_path(scope: &Scope, uri: &str) -> Result<PathBuf> {
    let url = reqwest::Url::parse(uri)?;
    let path = url
        .to_file_path()
        .map_err(|()| anyhow::anyhow!("LSP result is not a local file URI"))?;
    checked_path(scope, path.to_str().context("non-UTF-8 LSP location")?)
}
fn point(value: &Value) -> Result<Value> {
    let line = value["line"]
        .as_u64()
        .filter(|n| *n <= u64::from(u32::MAX))
        .context("invalid LSP line")?;
    let column = value["character"]
        .as_u64()
        .filter(|n| *n <= u64::from(u32::MAX))
        .context("invalid LSP character")?;
    Ok(json!({"line":line+1,"column":column+1}))
}
fn range(value: &Value) -> Result<Value> {
    Ok(json!({"start":point(&value["start"])?,"end":point(&value["end"])?}))
}
fn short(text: &str, limit: usize) -> String {
    text.chars().take(limit).collect()
}
pub(super) fn diagnostic_items(value: &Value) -> Result<Vec<Value>> {
    value.as_array().context("invalid LSP diagnostics")?.iter().take(MAX_RESULTS).map(|d| Ok(json!({"range":range(&d["range"])?,"severity":d["severity"].as_u64(),"message":short(d["message"].as_str().unwrap_or(""),2000),"source":short(d["source"].as_str().unwrap_or(""),128)}))).collect()
}
/// API positions are 1-based UTF-16, matching returned positions. The optional
/// unique symbol on the line avoids error-prone model column arithmetic.
fn position(text: &str, args: &Value) -> Result<Value> {
    let line = args["line"]
        .as_u64()
        .filter(|n| *n > 0 && *n <= SOURCE_BYTES as u64)
        .context("navigation requires 1-based line")?;
    let row = text
        .split('\n')
        .nth(line as usize - 1)
        .context("LSP line outside document")?
        .trim_end_matches('\r');
    let offset = if let Some(symbol) = args.get("symbol").and_then(Value::as_str) {
        if symbol.is_empty() || symbol.len() > 256 {
            bail!("symbol must be 1–256 bytes");
        }
        let mut matches = row.match_indices(symbol);
        let (offset, _) = matches
            .next()
            .context("symbol not found on this line; read_file first")?;
        if matches.next().is_some() {
            bail!("symbol is ambiguous on this line; pass an explicit 1-based UTF-16 column");
        }
        row[..offset].encode_utf16().count() as u64
    } else {
        args["column"]
            .as_u64()
            .filter(|n| *n > 0)
            .context("navigation requires symbol or 1-based UTF-16 column")?
            - 1
    };
    let mut columns = 0;
    let mut boundary = offset == 0;
    for c in row.chars() {
        columns += c.len_utf16() as u64;
        boundary |= columns == offset;
    }
    if !boundary {
        bail!("column is beyond the line or splits a UTF-16 surrogate pair");
    }
    Ok(json!({"line":line-1,"character":offset}))
}
fn location(scope: &Scope, value: &Value) -> Option<Value> {
    let raw = value
        .get("targetUri")
        .or_else(|| value.get("uri"))?
        .as_str()?;
    let path = uri_path(scope, raw).ok()?;
    let target = value
        .get("targetSelectionRange")
        .or_else(|| value.get("range"))?;
    Some(
        json!({"path":path.strip_prefix(&scope.workspace).ok()?.to_string_lossy(),"range":range(target).ok()?}),
    )
}
fn page(mut items: Vec<Value>, total: usize, omitted: usize) -> Value {
    // The final envelope leaves room for provenance/freshness metadata.
    let mut clipped = false;
    while serde_json::to_vec(&items).map_or(usize::MAX, |b| b.len()) > OUTPUT_BYTES - 2048 {
        if items.pop().is_none() {
            break;
        }
        clipped = true;
    }
    json!({"items":items,"total":total,"omitted_unavailable_or_external":omitted,"truncated":clipped || total > MAX_RESULTS,"position_encoding":"utf-16","position_base":1})
}
fn symbols(scope: &Scope, uri: &str, value: &Value) -> Result<Value> {
    let roots = value.as_array().context("invalid LSP symbols")?;
    let mut stack = roots.iter().rev().collect::<Vec<_>>();
    let mut items = Vec::new();
    let mut total = 0;
    let mut omitted = 0;
    while let Some(symbol) = stack.pop() {
        total += 1;
        if total > 2048 {
            bail!("LSP symbol tree exceeds 2048 nodes; narrow with search");
        }
        let loc = symbol.get("location").cloned().unwrap_or_else(|| json!({"uri":uri,"range":symbol.get("selectionRange").or_else(|| symbol.get("range"))}));
        if let Some(mut loc) = location(scope, &loc) {
            if items.len() < MAX_RESULTS {
                loc["name"] = json!(short(symbol["name"].as_str().unwrap_or(""), 256));
                loc["kind"] = json!(symbol["kind"].as_u64());
                items.push(loc);
            }
        } else {
            omitted += 1;
        }
        if let Some(children) = symbol["children"].as_array() {
            stack.extend(children.iter().rev());
        }
    }
    Ok(page(items, total, omitted))
}
impl Client {
    async fn sync(
        &mut self,
        config: &LspServerConfig,
        scope: &Scope,
        path: &Path,
    ) -> Result<String> {
        // Update every opened buffer, not just the query target: a prior tool,
        // formatter or external editor may have changed an imported file.
        let target = uri(path)?;
        let mut paths = self.documents.keys().cloned().collect::<Vec<_>>();
        if !self.documents.contains_key(&target) {
            if paths.len() >= MAX_DOCUMENTS {
                // Bounded index buffers; close the oldest lexicographic entry.
                let old = paths.remove(0);
                self.notify("textDocument/didClose", json!({"textDocument":{"uri":old}}))
                    .await?;
                self.documents.remove(&old);
                self.diagnostics.remove(&old);
            }
            paths.push(target.clone());
        }
        let mut changed = std::collections::BTreeSet::new();
        for uri in paths {
            let path = uri_path(scope, &uri)?;
            let text = source(scope, &path).await?;
            match self.documents.get(&uri) {
                Some(doc) if doc.text == text => {}
                Some(_) => {
                    changed.insert(uri.clone());
                    self.next_version = self
                        .next_version
                        .checked_add(1)
                        .context("LSP document version overflow")?;
                    let version = self.next_version;
                    self.documents.insert(
                        uri.clone(),
                        Document {
                            text: text.clone(),
                            version,
                        },
                    );
                    self.diagnostics.remove(&uri);
                    self.notify("textDocument/didChange", json!({"textDocument":{"uri":uri,"version":version},"contentChanges":[{"text":text}]})).await?;
                    let save = &self.capabilities["textDocumentSync"]["save"];
                    if save.is_object() || save == &Value::Bool(true) {
                        let mut params = json!({"textDocument":{"uri":uri}});
                        if save["includeText"] == true {
                            params["text"] = json!(text);
                        }
                        self.notify("textDocument/didSave", params).await?;
                    }
                    self.notify(
                        "workspace/didChangeWatchedFiles",
                        json!({"changes":[{"uri":uri,"type":2}]}),
                    )
                    .await?;
                }
                None => {
                    self.next_version = self
                        .next_version
                        .checked_add(1)
                        .context("LSP document version overflow")?;
                    let version = self.next_version;
                    self.documents.insert(
                        uri.clone(),
                        Document {
                            text: text.clone(),
                            version,
                        },
                    );
                    self.notify("textDocument/didOpen", json!({"textDocument":{"uri":uri,"languageId":config.language_id,"version":version,"text":text}})).await?;
                }
            }
        }
        if !changed.is_empty() {
            self.diagnostics.clear();
            // Imported-document changes can stale diagnostics for an unchanged
            // target. Re-version those buffers too, so queued old reports
            // cannot masquerade as current versioned diagnostics.
            let unchanged = self
                .documents
                .keys()
                .filter(|uri| !changed.contains(*uri))
                .cloned()
                .collect::<Vec<_>>();
            for uri in unchanged {
                self.next_version = self
                    .next_version
                    .checked_add(1)
                    .context("LSP document version overflow")?;
                let doc = self
                    .documents
                    .get_mut(&uri)
                    .context("missing LSP document")?;
                doc.version = self.next_version;
                let params = json!({"textDocument":{"uri":uri,"version":doc.version},"contentChanges":[{"text":doc.text}]});
                self.notify("textDocument/didChange", params).await?;
            }
        }
        Ok(target)
    }
    pub(super) async fn check_snapshots(
        &self,
        scope: &Scope,
    ) -> Result<Vec<latch_protocol::FileVersion>> {
        use sha2::{Digest, Sha256};
        let mut versions = Vec::new();
        for (uri, doc) in &self.documents {
            let path = uri_path(scope, uri)?;
            if source(scope, &path).await? != doc.text {
                bail!("LSP snapshot became stale during query; repeat against current files");
            }
            versions.push(latch_protocol::FileVersion {
                path: path
                    .strip_prefix(&scope.workspace)?
                    .to_string_lossy()
                    .into_owned(),
                content_hash: hex::encode(Sha256::digest(doc.text.as_bytes())),
                size: doc.text.len() as u64,
            });
        }
        Ok(versions)
    }
    pub(super) async fn query(
        &mut self,
        config: &LspServerConfig,
        scope: &Scope,
        path: &Path,
        args: &Value,
    ) -> Result<Value> {
        let action = args["action"].as_str().context("missing LSP action")?;
        let uri = self.sync(config, scope, path).await?;
        let feature = match action {
            "symbols" => "documentSymbolProvider",
            "definition" => "definitionProvider",
            "references" => "referencesProvider",
            "hover" => "hoverProvider",
            _ => "",
        };
        if !feature.is_empty()
            && self
                .capabilities
                .get(feature)
                .is_none_or(|v| v.is_null() || v == &Value::Bool(false))
        {
            bail!("LSP server does not advertise {action}; use search/read_file");
        }
        let mut params = json!({"textDocument":{"uri":uri}});
        if matches!(action, "definition" | "references" | "hover") {
            params["position"] = position(&self.documents[&uri].text, args)?;
        }
        let value = match action {
            "diagnostics" => return self.diagnostics_for(&uri).await,
            "symbols" => self.rpc("textDocument/documentSymbol", params).await?,
            "definition" => self.rpc("textDocument/definition", params).await?,
            "references" => {
                params["context"] = json!({"includeDeclaration":true});
                self.rpc("textDocument/references", params).await?
            }
            "hover" => self.rpc("textDocument/hover", params).await?,
            _ => bail!("unsupported LSP query"),
        };
        if value.is_null() {
            return Ok(page(vec![], 0, 0));
        }
        if action == "symbols" {
            return symbols(scope, &uri, &value);
        }
        if action == "hover" {
            let contents = &value["contents"];
            let text = if let Some(text) = contents.as_str() {
                short(text, 4000)
            } else if let Some(text) = contents["value"].as_str() {
                short(text, 4000)
            } else if let Some(parts) = contents.as_array() {
                parts
                    .iter()
                    .take(8)
                    .map(|part| {
                        short(
                            part.as_str()
                                .or_else(|| part["value"].as_str())
                                .unwrap_or(""),
                            500,
                        )
                    })
                    .collect::<Vec<_>>()
                    .join("\n")
            } else {
                bail!("invalid LSP hover contents");
            };
            return Ok(json!({"text":text,"position_encoding":"utf-16","position_base":1}));
        }
        let locations = if let Some(list) = value.as_array() {
            list.clone()
        } else {
            vec![value]
        };
        let total = locations.len();
        let mut omitted = 0;
        let items = locations
            .iter()
            .filter_map(|item| match location(scope, item) {
                Some(loc) => Some(loc),
                None => {
                    omitted += 1;
                    None
                }
            })
            .take(MAX_RESULTS)
            .collect();
        Ok(page(items, total, omitted))
    }
    async fn diagnostics_for(&mut self, uri: &str) -> Result<Value> {
        if self
            .capabilities
            .get("diagnosticProvider")
            .is_some_and(|v| !v.is_null() && v != &Value::Bool(false))
        {
            let result = self
                .rpc(
                    "textDocument/diagnostic",
                    json!({"textDocument":{"uri":uri}}),
                )
                .await?;
            if result["kind"] != "full" {
                bail!("LSP returned unchanged diagnostics without a prior result; retry manually");
            }
            let items = diagnostic_items(&result["items"])?;
            let total = result["items"].as_array().map_or(0, Vec::len);
            let mut output = page(items, total, 0);
            output["diagnostic_version"] = json!(self.documents[uri].version);
            output["diagnostic_source"] = json!("pull");
            return Ok(output);
        }
        for _ in 0..2048 {
            if let Some(diagnostic) = self.diagnostics.get(uri).cloned() {
                let version = diagnostic["version"].as_i64();
                if version.is_none() || version == Some(i64::from(self.documents[uri].version)) {
                    let items = diagnostic["items"].as_array().cloned().unwrap_or_default();
                    let mut output =
                        page(items, diagnostic["total"].as_u64().unwrap_or(0) as usize, 0);
                    output["diagnostic_version"] = json!(version);
                    output["diagnostic_source"] = json!(if version.is_some() {
                        "push-versioned"
                    } else {
                        "push-unversioned-freshness-not-proven"
                    });
                    return Ok(output);
                }
                self.diagnostics.remove(uri);
            }
            self.message().await?;
        }
        bail!("LSP diagnostic notification flood; no current diagnostic report")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn state_paths_and_aliases_are_never_navigation_targets() {
        let dir = tempfile::tempdir().unwrap();
        let state = dir.path().join("state");
        std::fs::create_dir(&state).unwrap();
        std::fs::write(state.join("secret.rs"), "private").unwrap();
        let scope = Scope {
            workspace: dir.path().canonicalize().unwrap(),
            state_dir: state,
        };
        assert!(checked_path(&scope, "state/secret.rs").is_err());
        assert!(checked_path(&scope, "../outside.rs").is_err());
        std::fs::write(dir.path().join("normal.rs"), "public").unwrap();
        std::fs::hard_link(dir.path().join("normal.rs"), dir.path().join("alias.rs")).unwrap();
        assert!(checked_path(&scope, "alias.rs").is_err());
        #[cfg(unix)]
        {
            std::os::unix::fs::symlink(&scope.state_dir, dir.path().join("state-alias")).unwrap();
            assert!(checked_path(&scope, "state-alias/secret.rs").is_err());
        }
    }
    #[test]
    fn unicode_positions_and_ambiguous_symbols_are_checked() {
        let text = "// 😀测试\nlet x = foo(); foo();\n";
        assert_eq!(
            position(text, &json!({"line":1,"symbol":"测试"})).unwrap(),
            json!({"line":0,"character":5})
        );
        assert!(position(text, &json!({"line":1,"column":5})).is_err());
        assert!(position(text, &json!({"line":2,"symbol":"foo"})).is_err());
        assert_eq!(
            position(text, &json!({"line":2,"column":9})).unwrap()["character"],
            8
        );
        assert!(position(text, &json!({"line":0,"column":1})).is_err());
    }
}

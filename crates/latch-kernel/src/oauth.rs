//! ChatGPT/Codex device authorization and refresh-token lifecycle.
//!
//! Tokens live in a dedicated restricted credential file, never in configuration
//! or durable events. A cross-process lock serializes refresh, login and logout;
//! every request reloads credentials so child sessions cannot retain stale tokens.
use anyhow::{Result, anyhow, bail};
use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use tokio_util::sync::CancellationToken;

use crate::credentials::CredentialStore;

// Public native-client identifier used by the Codex device authorization service.
// This is not a client secret. Latch identifies itself separately as originator.
const CLIENT_ID: &str = "app_EMoamEEZ73f0CkXaXp7hrann";
const ISSUER: &str = "https://auth.openai.com";
const ENTRY: &str = "openai-codex";
pub const VERIFICATION_URL: &str = "https://auth.openai.com/codex/device";
pub const BASE_URL: &str = "https://chatgpt.com/backend-api/codex";
pub const DEFAULT_MODEL: &str = "gpt-6.1-sol";
// Offline fallback; interactive discovery resolves the official stable release.
// Model minimal_client_version does not fully describe service listing gates.
pub const CATALOG_CLIENT_VERSION: &str = "0.166.0";

#[derive(Clone, Serialize, Deserialize)]
pub struct OAuthTokens {
    pub access_token: String,
    refresh_token: String,
    pub account_id: String,
    expires_at: u64,
}
impl std::fmt::Debug for OAuthTokens {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("OAuthTokens([redacted])")
    }
}

#[derive(Clone, Debug)]
pub struct ChatGptAuth {
    path: PathBuf,
    issuer: String,
    client: reqwest::Client,
}

/// Public fields are safe to show only to the human who initiated sign-in.
/// The device authorization id is deliberately not Debug/Serialize.
pub struct DeviceLogin {
    pub user_code: String,
    device_auth_id: String,
    interval: Duration,
}

fn now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

impl ChatGptAuth {
    #[must_use]
    pub fn new(state_dir: &Path) -> Self {
        Self {
            path: state_dir.join("chatgpt-auth.toml"),
            issuer: ISSUER.to_owned(),
            client: reqwest::Client::builder()
                .redirect(reqwest::redirect::Policy::none())
                .timeout(Duration::from_secs(30))
                .build()
                .expect("valid OAuth HTTP client"),
        }
    }

    fn load(&self) -> Result<Option<OAuthTokens>> {
        reject_alias(&self.path)?;
        let store = self.store()?;
        store
            .resolve(&crate::CredentialRef::File(ENTRY.into()))?
            .map(|value| {
                serde_json::from_str(&value)
                    .map_err(|_| anyhow!("invalid ChatGPT credentials; run /login again"))
            })
            .transpose()
    }

    /// Offline presence check for setup and doctor; never refreshes or contacts OpenAI.
    pub fn is_available(&self) -> Result<bool> {
        Ok(self.load()?.is_some_and(|t| {
            !t.refresh_token.is_empty() && !t.access_token.is_empty() && !t.account_id.is_empty()
        }))
    }

    async fn lock(&self) -> Result<std::fs::File> {
        let root = self
            .path
            .parent()
            .ok_or_else(|| anyhow!("missing OAuth state directory"))?;
        crate::paths::ResolvedPaths::ensure_private_root(root)?;
        let path = root.join("chatgpt-auth.lock");
        reject_alias(&path)?;
        let mut options = std::fs::OpenOptions::new();
        options.create(true).read(true).write(true).truncate(false);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        let file = options.open(path)?;
        tokio::time::timeout(Duration::from_secs(45), async {
            loop {
                match file.try_lock() {
                    Ok(()) => return Ok(file),
                    Err(std::fs::TryLockError::WouldBlock) => {
                        tokio::time::sleep(Duration::from_millis(50)).await
                    }
                    Err(error) => return Err(anyhow!("cannot lock ChatGPT credentials: {error}")),
                }
            }
        })
        .await
        .map_err(|_| anyhow!("ChatGPT credentials are busy; retry shortly"))?
    }

    fn store(&self) -> Result<CredentialStore> {
        // TOML parse errors can quote source lines containing tokens. Never
        // propagate those diagnostics into notices, doctor output or logs.
        CredentialStore::open(&self.path).map_err(|_| anyhow!(
            "cannot read ChatGPT credentials; ensure chatgpt-auth.toml is private and valid, or remove it and run /login again"
        ))
    }

    fn save(&self, tokens: &OAuthTokens) -> Result<()> {
        reject_alias(&self.path)?;
        self.store()?.set(ENTRY, &serde_json::to_string(tokens)?)
    }

    pub async fn logout(&self) -> Result<()> {
        let _lock = self.lock().await?;
        reject_alias(&self.path)?;
        self.store()?.remove(ENTRY)?;
        Ok(())
    }

    /// Loads/refreshes under one cross-process lock. No automatic retry of a
    /// rotating refresh token: ambiguous transport failures require re-login.
    pub async fn access(&self) -> Result<OAuthTokens> {
        let _lock = self.lock().await?;
        let tokens = self
            .load()?
            .ok_or_else(|| anyhow!("ChatGPT is not signed in; run /login or latch login"))?;
        if tokens.expires_at > now().saturating_add(120) {
            return Ok(tokens);
        }
        let response = self
            .client
            .post(format!("{}/oauth/token", self.issuer))
            .form(&[
                ("grant_type", "refresh_token"),
                ("client_id", CLIENT_ID),
                ("refresh_token", &tokens.refresh_token),
            ])
            .send()
            .await
            .map_err(|_| anyhow!("ChatGPT token refresh transport failed; retry or run /login"))?;
        let refreshed = read_tokens(response).await?;
        if refreshed.account_id != tokens.account_id {
            bail!("ChatGPT account changed during refresh; run /login again");
        }
        self.save(&refreshed)?;
        Ok(refreshed)
    }

    /// Read-only account catalog, not a model inference request. The version
    /// parameter describes the supported Codex catalog protocol, not our UA.
    pub async fn models(&self) -> Result<Value> {
        self.models_for_version(CATALOG_CLIENT_VERSION).await
    }

    /// The version controls catalog visibility, not application identity.
    pub async fn models_for_version(&self, client_version: &str) -> Result<Value> {
        self.models_at(BASE_URL, client_version).await
    }

    async fn models_at(&self, base_url: &str, client_version: &str) -> Result<Value> {
        let tokens = self.access().await?;
        let mut response = self
            .client
            .get(format!("{base_url}/models"))
            .query(&[("client_version", client_version)])
            .bearer_auth(&tokens.access_token)
            .header("chatgpt-account-id", &tokens.account_id)
            .header("originator", "latch")
            .header("user-agent", concat!("latch/", env!("CARGO_PKG_VERSION")))
            .send()
            .await
            .map_err(|_| anyhow!("ChatGPT model catalog request failed"))?;
        if !response.status().is_success() {
            bail!(
                "ChatGPT model catalog unavailable (HTTP {}); retry /model refresh or /login",
                response.status()
            );
        }
        let mut bytes = Vec::new();
        while let Some(chunk) = response
            .chunk()
            .await
            .map_err(|_| anyhow!("ChatGPT model catalog read failed"))?
        {
            if bytes.len() + chunk.len() > 4 * 1024 * 1024 {
                bail!("ChatGPT model catalog exceeds 4 MiB");
            }
            bytes.extend_from_slice(&chunk);
        }
        serde_json::from_slice(&bytes).map_err(|_| anyhow!("invalid ChatGPT model catalog"))
    }

    pub async fn begin(&self) -> Result<DeviceLogin> {
        let response = self
            .client
            .post(format!("{}/api/accounts/deviceauth/usercode", self.issuer))
            .json(&json!({"client_id": CLIENT_ID}))
            .send()
            .await
            .map_err(|_| anyhow!("could not contact OpenAI for ChatGPT sign-in"))?;
        if !response.status().is_success() {
            bail!(
                "ChatGPT device sign-in unavailable (HTTP {}); enable device code authorization in ChatGPT security settings and retry",
                response.status()
            );
        }
        let value: Value = response
            .json()
            .await
            .map_err(|_| anyhow!("invalid ChatGPT device sign-in response"))?;
        let interval = value["interval"]
            .as_u64()
            .or_else(|| value["interval"].as_str()?.trim().parse().ok())
            .unwrap_or(5)
            .clamp(1, 60);
        Ok(DeviceLogin {
            user_code: required(&value, "user_code").or_else(|_| required(&value, "usercode"))?,
            device_auth_id: required(&value, "device_auth_id")?,
            interval: Duration::from_secs(interval),
        })
    }

    /// Bounded and cancellable, including network requests. Nothing is stored
    /// until authorization and token validation both succeed.
    pub async fn complete(&self, device: DeviceLogin, cancel: CancellationToken) -> Result<()> {
        tokio::select! {
            () = cancel.cancelled() => bail!("ChatGPT sign-in cancelled"),
            result = tokio::time::timeout(Duration::from_secs(15 * 60), self.poll(device)) => {
                result.map_err(|_| anyhow!("ChatGPT sign-in expired; run /login again"))?
            }
        }
    }

    async fn poll(&self, device: DeviceLogin) -> Result<()> {
        let mut interval = device.interval;
        loop {
            tokio::time::sleep(interval).await;
            let response = self.client.post(format!("{}/api/accounts/deviceauth/token", self.issuer))
                .json(&json!({"device_auth_id": device.device_auth_id, "user_code": device.user_code}))
                .send().await.map_err(|_| anyhow!("ChatGPT sign-in polling failed; run /login again"))?;
            if response.status().is_success() {
                let value: Value = response
                    .json()
                    .await
                    .map_err(|_| anyhow!("invalid ChatGPT authorization response"))?;
                let code = required(&value, "authorization_code")?;
                let verifier = required(&value, "code_verifier")?;
                let redirect = format!("{}/deviceauth/callback", self.issuer);
                let response = self
                    .client
                    .post(format!("{}/oauth/token", self.issuer))
                    .form(&[
                        ("grant_type", "authorization_code"),
                        ("client_id", CLIENT_ID),
                        ("code", &code),
                        ("code_verifier", &verifier),
                        ("redirect_uri", &redirect),
                    ])
                    .send()
                    .await
                    .map_err(|_| anyhow!("ChatGPT token exchange failed; run /login again"))?;
                let tokens = read_tokens(response).await?;
                let _lock = self.lock().await?;
                return self.save(&tokens);
            }
            if matches!(response.status().as_u16(), 403 | 404) {
                continue;
            }
            let status = response.status();
            let value: Value = response.json().await.unwrap_or(Value::Null);
            let error = value["error"]
                .as_str()
                .or_else(|| value["error"]["code"].as_str());
            match error {
                Some("authorization_pending" | "deviceauth_authorization_pending") => continue,
                Some("slow_down") => {
                    interval = (interval + Duration::from_secs(5)).min(Duration::from_secs(60))
                }
                _ => bail!("ChatGPT authorization failed (HTTP {status}); run /login again"),
            }
        }
    }
}

fn reject_alias(path: &Path) -> Result<()> {
    match std::fs::symlink_metadata(path) {
        Ok(metadata) => {
            let mut aliased = metadata.file_type().is_symlink()
                || !metadata.is_file()
                || crate::workspace_path::file_alias_may_escape(path);
            #[cfg(unix)]
            {
                use std::os::unix::fs::MetadataExt;
                aliased |= metadata.nlink() != 1;
            }
            #[cfg(windows)]
            {
                use std::os::windows::fs::MetadataExt;
                aliased |= metadata.file_attributes() & 0x400 != 0;
            }
            if aliased {
                bail!("ChatGPT credential files must be ordinary, unaliased files");
            }
            Ok(())
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error.into()),
    }
}

fn required(value: &Value, key: &str) -> Result<String> {
    value
        .get(key)
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
        .map(str::to_owned)
        .ok_or_else(|| anyhow!("ChatGPT authorization response missing {key}"))
}

async fn read_tokens(response: reqwest::Response) -> Result<OAuthTokens> {
    if !response.status().is_success() {
        // Never echo response bodies: auth servers may include submitted tokens.
        bail!(
            "ChatGPT token exchange/refresh failed (HTTP {}); run /login again",
            response.status()
        );
    }
    let value: Value = response
        .json()
        .await
        .map_err(|_| anyhow!("invalid ChatGPT token response"))?;
    parse_tokens(&value)
}

fn parse_tokens(value: &Value) -> Result<OAuthTokens> {
    let access_token = required(value, "access_token")?;
    let refresh_token = required(value, "refresh_token")?;
    let expires_in = value["expires_in"]
        .as_u64()
        .filter(|s| *s > 120 && *s <= 365 * 24 * 3600)
        .ok_or_else(|| anyhow!("invalid ChatGPT token expiry"))?;
    // Claims are routing metadata from an HTTPS token response, not an
    // independently authenticated identity or local authorization grant.
    let payload = access_token
        .split('.')
        .nth(1)
        .and_then(|p| URL_SAFE_NO_PAD.decode(p).ok())
        .and_then(|p| serde_json::from_slice::<Value>(&p).ok())
        .ok_or_else(|| anyhow!("invalid ChatGPT access token"))?;
    let account_id = payload
        .pointer("/https:~1~1api.openai.com~1auth/chatgpt_account_id")
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty() && s.is_ascii() && !s.chars().any(char::is_control))
        .ok_or_else(|| anyhow!("ChatGPT access token has no valid account id"))?
        .to_owned();
    Ok(OAuthTokens {
        access_token,
        refresh_token,
        account_id,
        expires_at: now().saturating_add(expires_in),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn token_value(expires_in: u64) -> Value {
        let payload = URL_SAFE_NO_PAD.encode(
            serde_json::to_vec(
                &json!({"https://api.openai.com/auth": {"chatgpt_account_id":"account-test"}}),
            )
            .unwrap(),
        );
        json!({"access_token":format!("header.{payload}.signature"),"refresh_token":"refresh-secret","expires_in":expires_in})
    }

    #[tokio::test]
    async fn storage_is_private_offline_and_logout_invalidates_existing_handles() {
        let root = tempfile::tempdir().unwrap();
        let auth = ChatGptAuth::new(root.path());
        assert!(!auth.is_available().unwrap());
        let tokens = parse_tokens(&token_value(3600)).unwrap();
        auth.save(&tokens).unwrap();
        assert!(auth.is_available().unwrap());
        assert_eq!(
            auth.clone().access().await.unwrap().account_id,
            "account-test"
        );
        assert!(!format!("{tokens:?}").contains("refresh-secret"));
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(
                std::fs::metadata(&auth.path).unwrap().permissions().mode() & 0o777,
                0o600
            );
        }
        auth.logout().await.unwrap();
        assert!(!auth.is_available().unwrap());
        assert!(
            auth.access()
                .await
                .unwrap_err()
                .to_string()
                .contains("/login")
        );
    }

    /// Tiny loopback fixture: no real accounts, credentials or provider calls.
    async fn mock(responses: Vec<(u16, Value)>) -> (String, tokio::task::JoinHandle<Vec<String>>) {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        let task = tokio::spawn(async move {
            let mut requests = Vec::new();
            for (status, value) in responses {
                let (mut socket, _) =
                    tokio::time::timeout(Duration::from_secs(5), listener.accept())
                        .await
                        .unwrap()
                        .unwrap();
                let mut bytes = Vec::new();
                loop {
                    let mut chunk = [0; 2048];
                    let n = socket.read(&mut chunk).await.unwrap();
                    assert_ne!(n, 0);
                    bytes.extend_from_slice(&chunk[..n]);
                    assert!(bytes.len() < 65536);
                    if let Some(end) = bytes.windows(4).position(|w| w == b"\r\n\r\n") {
                        let headers = String::from_utf8_lossy(&bytes[..end]).to_lowercase();
                        let length: usize = headers
                            .lines()
                            .find_map(|line| line.strip_prefix("content-length: "))
                            .unwrap_or("0")
                            .parse()
                            .unwrap();
                        if bytes.len() >= end + 4 + length {
                            break;
                        }
                    }
                }
                requests.push(String::from_utf8(bytes).unwrap());
                let body = value
                    .as_str()
                    .map(str::to_owned)
                    .unwrap_or_else(|| value.to_string());
                socket.write_all(format!("HTTP/1.1 {status} Fixture\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len()).as_bytes()).await.unwrap();
            }
            requests
        });
        (url, task)
    }

    #[tokio::test]
    async fn subscription_transport_streams_tools_replays_reasoning_and_honors_logout() {
        use crate::provider::{ModelProvider, OpenAiResponsesProvider};
        use latch_protocol::{ModelMessage, ModelRequest};
        let root = tempfile::tempdir().unwrap();
        let auth = ChatGptAuth::new(root.path());
        auth.save(&parse_tokens(&token_value(3600)).unwrap())
            .unwrap();
        let first = [
            json!({"type":"response.output_text.delta","delta":"Checking"}),
            json!({"type":"response.done","response":{"status":"completed","output":[
                {"type":"reasoning","id":"reason1","encrypted_content":"opaque"},
                {"type":"function_call","id":"fc1","call_id":"call1","name":"read","arguments":"{\"path\":\"README.md\"}"}
            ],"usage":{"input_tokens":10,"output_tokens":3}}}),
        ].iter().map(|v| format!("data: {v}\n\n")).collect::<String>();
        let second = format!(
            "data: {}\n\ndata: {}\n\n",
            json!({"type":"response.output_text.delta","delta":"Done"}),
            json!({"type":"response.completed","response":{"status":"completed","output":[]}})
        );
        let (url, requests) = mock(vec![(200, json!(first)), (200, json!(second))]).await;
        let session = uuid::Uuid::new_v4();
        let provider = OpenAiResponsesProvider::new(url, String::new(), DEFAULT_MODEL.into())
            .with_chatgpt_auth(auth.clone())
            .with_session(session);
        let request = ModelRequest {
            system: "stable instructions".into(),
            messages: vec![ModelMessage::text("user", "Read README")],
            tools: vec![],
        };
        let sink: crate::provider::StreamSink = std::sync::Arc::new(|_| {});
        let response = provider
            .stream(request.clone(), CancellationToken::new(), sink.clone())
            .await
            .unwrap();
        assert_eq!(response.text, "Checking");
        assert_eq!(response.tool_calls[0].id, "call1");
        assert_eq!(response.usage.unwrap().input_tokens, 10);
        let mut assistant = ModelMessage::text("assistant", response.text);
        assistant.tool_calls = response.tool_calls;
        assistant.reasoning = response.reasoning;
        let mut next = request;
        next.messages.extend([
            assistant,
            ModelMessage::tool_result("call1", "contents", false, vec![]),
        ]);
        let child = provider.for_session(uuid::Uuid::new_v4()).unwrap();
        assert_eq!(
            child
                .stream(next, CancellationToken::new(), sink.clone())
                .await
                .unwrap()
                .text,
            "Done"
        );
        let requests = requests.await.unwrap();
        let first_headers = requests[0].to_lowercase();
        assert!(first_headers.contains("chatgpt-account-id: account-test"));
        assert!(first_headers.contains("originator: latch"));
        assert!(!first_headers.contains("x-opencode-session"));
        let body: Value =
            serde_json::from_str(requests[1].split("\r\n\r\n").nth(1).unwrap()).unwrap();
        assert_eq!(body["store"], false);
        assert_eq!(body["instructions"], "stable instructions");
        assert!(
            body["input"]
                .as_array()
                .unwrap()
                .iter()
                .any(|i| i["type"] == "reasoning" && i["encrypted_content"] == "opaque")
        );
        assert!(
            body["input"]
                .as_array()
                .unwrap()
                .iter()
                .any(|i| i["type"] == "function_call_output" && i["call_id"] == "call1")
        );
        auth.logout().await.unwrap();
        assert!(
            provider
                .stream(
                    ModelRequest {
                        system: String::new(),
                        messages: vec![],
                        tools: vec![]
                    },
                    CancellationToken::new(),
                    sink
                )
                .await
                .unwrap_err()
                .to_string()
                .contains("/login")
        );
    }

    #[tokio::test]
    async fn subscription_transport_rejects_truncated_stream() {
        use crate::provider::{ModelProvider, OpenAiResponsesProvider};
        let root = tempfile::tempdir().unwrap();
        let auth = ChatGptAuth::new(root.path());
        auth.save(&parse_tokens(&token_value(3600)).unwrap())
            .unwrap();
        let (url, task) = mock(vec![(
            200,
            json!("data: {\"type\":\"response.output_text.delta\",\"delta\":\"partial\"}\n\n"),
        )])
        .await;
        let provider = OpenAiResponsesProvider::new(url, String::new(), DEFAULT_MODEL.into())
            .with_chatgpt_auth(auth);
        let error = provider
            .stream(
                latch_protocol::ModelRequest {
                    system: String::new(),
                    messages: vec![],
                    tools: vec![],
                },
                CancellationToken::new(),
                std::sync::Arc::new(|_| {}),
            )
            .await
            .unwrap_err();
        assert!(error.to_string().contains("before completion"));
        task.await.unwrap();
    }

    #[tokio::test]
    async fn account_catalog_uses_managed_auth_and_does_not_make_inference_calls() {
        let root = tempfile::tempdir().unwrap();
        let auth = ChatGptAuth::new(root.path());
        auth.save(&parse_tokens(&token_value(3600)).unwrap())
            .unwrap();
        let (url, requests) = mock(vec![(
            200,
            json!({"models":[{"slug":"new-model","visibility":"list"}]}),
        )])
        .await;
        assert_eq!(
            auth.models_at(&url, CATALOG_CLIENT_VERSION).await.unwrap()["models"][0]["slug"],
            "new-model"
        );
        let requests = requests.await.unwrap();
        assert_eq!(requests.len(), 1);
        assert!(requests[0].starts_with("GET /models?client_version=0.166.0 "));
        assert!(requests[0].to_lowercase().contains("originator: latch"));
        assert!(
            requests[0]
                .to_lowercase()
                .contains("chatgpt-account-id: account-test")
        );
        let (url, requests) = mock(vec![(401, json!({"error":"refresh-secret"}))]).await;
        assert!(
            !auth
                .models_at(&url, CATALOG_CLIENT_VERSION)
                .await
                .unwrap_err()
                .to_string()
                .contains("refresh-secret")
        );
        requests.await.unwrap();
    }

    #[tokio::test]
    async fn device_login_polls_pending_exchanges_pkce_and_commits() {
        let root = tempfile::tempdir().unwrap();
        let (issuer, requests) = mock(vec![
            (
                200,
                json!({"user_code":"ABCD-EFGH","device_auth_id":"device-private","interval":"1"}),
            ),
            (403, json!({})),
            (
                200,
                json!({"authorization_code":"code-private","code_verifier":"verifier-private"}),
            ),
            (200, token_value(3600)),
        ])
        .await;
        let mut auth = ChatGptAuth::new(root.path());
        auth.issuer = issuer;
        let mut device = auth.begin().await.unwrap();
        assert_eq!(device.user_code, "ABCD-EFGH");
        device.interval = Duration::from_millis(1);
        assert!(!auth.path.exists());
        auth.complete(device, CancellationToken::new())
            .await
            .unwrap();
        assert!(auth.is_available().unwrap());
        let requests = requests.await.unwrap();
        assert!(requests[0].starts_with("POST /api/accounts/deviceauth/usercode "));
        assert!(requests[1].contains("device-private"));
        assert!(requests[3].starts_with("POST /oauth/token "));
        assert!(requests[3].contains("code_verifier=verifier-private"));
        assert!(requests[3].contains("grant_type=authorization_code"));
    }

    #[tokio::test]
    async fn concurrent_refreshes_reload_and_rotate_only_once() {
        let root = tempfile::tempdir().unwrap();
        let mut fresh = token_value(3600);
        fresh["refresh_token"] = json!("rotated-secret");
        let (issuer, requests) = mock(vec![(200, fresh)]).await;
        let mut auth = ChatGptAuth::new(root.path());
        auth.issuer = issuer;
        let mut expired = parse_tokens(&token_value(3600)).unwrap();
        expired.expires_at = 0;
        auth.save(&expired).unwrap();
        let other = auth.clone();
        let (a, b) = tokio::join!(auth.access(), other.access());
        assert_eq!(a.unwrap().refresh_token, "rotated-secret");
        assert_eq!(b.unwrap().refresh_token, "rotated-secret");
        assert_eq!(requests.await.unwrap().len(), 1);
        assert_eq!(
            auth.load().unwrap().unwrap().refresh_token,
            "rotated-secret"
        );
    }

    #[tokio::test]
    async fn rejected_refresh_is_redacted_and_preserves_credentials() {
        let root = tempfile::tempdir().unwrap();
        let (issuer, requests) = mock(vec![(
            401,
            json!({"error":"refresh-secret access-private"}),
        )])
        .await;
        let mut auth = ChatGptAuth::new(root.path());
        auth.issuer = issuer;
        let mut tokens = parse_tokens(&token_value(3600)).unwrap();
        tokens.expires_at = 0;
        auth.save(&tokens).unwrap();
        let error = auth.access().await.unwrap_err().to_string();
        assert!(!error.contains("refresh-secret"));
        assert!(error.contains("/login"));
        assert_eq!(
            auth.load().unwrap().unwrap().refresh_token,
            tokens.refresh_token
        );
        requests.await.unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn credential_aliases_are_refused() {
        let root = tempfile::tempdir().unwrap();
        let auth = ChatGptAuth::new(root.path());
        let target = root.path().join("outside");
        std::fs::write(&target, "secret").unwrap();
        std::os::unix::fs::symlink(&target, &auth.path).unwrap();
        assert!(auth.is_available().is_err());
        std::fs::remove_file(&auth.path).unwrap();
        std::fs::hard_link(&target, &auth.path).unwrap();
        assert!(auth.is_available().is_err());
    }

    #[test]
    fn corrupt_credential_file_diagnostics_do_not_echo_source() {
        use std::io::Write;
        let root = tempfile::tempdir().unwrap();
        let auth = ChatGptAuth::new(root.path());
        let mut file = tempfile::NamedTempFile::new_in(root.path()).unwrap();
        file.write_all(b"[secrets]\nopenai-codex = leaked-refresh-secret\n")
            .unwrap();
        file.persist(&auth.path).unwrap();
        let error = auth.is_available().unwrap_err();
        assert!(!format!("{error:#}").contains("leaked-refresh-secret"));
    }

    #[test]
    fn malformed_tokens_never_echo_secrets() {
        for value in [
            json!({"access_token":"secret"}),
            token_value(0),
            json!({"access_token":"secret","refresh_token":"refresh-secret","expires_in":3600}),
        ] {
            let error = parse_tokens(&value).unwrap_err().to_string();
            assert!(!error.contains("secret"));
        }
    }

    #[tokio::test]
    async fn cancellation_does_not_persist_tokens() {
        let root = tempfile::tempdir().unwrap();
        let auth = ChatGptAuth::new(root.path());
        let cancel = CancellationToken::new();
        cancel.cancel();
        assert!(
            auth.complete(
                DeviceLogin {
                    user_code: "code".into(),
                    device_auth_id: "private".into(),
                    interval: Duration::from_secs(60)
                },
                cancel
            )
            .await
            .is_err()
        );
        assert!(!auth.path.exists());
    }
}

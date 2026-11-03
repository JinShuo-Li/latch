# ChatGPT subscriptions

Latch can connect to a ChatGPT account through the OpenAI Codex device
authorization service. This is **separate from OpenAI API-key billing**. Your
account must be eligible for Codex and remains subject to its subscription usage
limits and organization policy. A ChatGPT subscription does not provide general
OpenAI API credits. Latch never switches to API billing when a subscription
request fails.

## Sign in

In the TUI or Web composer, run:

```text
/login
```

You can also select **ChatGPT subscription** in `/setup`, or sign in before
starting an interactive session:

```sh
latch login
# For a non-default installation:
latch --config /path/to/config.toml login
```

1. Open `https://auth.openai.com/codex/device` in your browser.
2. Enter the one-time code displayed by Latch and complete OpenAI sign-in.
3. Return to Latch. It waits for authorization, saves credentials, and makes the
   subscription provider available as `openai-codex`.

**Only enter a code for a login you initiated yourself.** Never paste access
or refresh tokens into a chat, configuration file, issue, or API-key field.
You may need to enable **device code authorization** in ChatGPT's security
settings. If your organization disables it, this implementation does not bypass
that policy. There is no browser-cookie or external credential-file import.

The Web interface shows a sign-in dialog with a link, code and Cancel button.
In the TUI, Ctrl+C cancels while the code is displayed; `/login cancel` also
cancels. The standalone CLI uses Ctrl+C. Authorization expires after 15 minutes.
The device flow works over SSH without a local callback listener or port forward.

Interactive login selects the provider for the current session. Existing
configured providers and explicit new-session defaults are kept. On a new
installation, ChatGPT becomes the new-session default. Use `/setup` to change
that default, and `/model` to select a model or reasoning effort. Standalone
`latch login` saves the provider but does not change an existing session.

The initial catalog includes `gpt-5.4` and `gpt-5.3-codex`. Availability depends
on the account and service. These are subscription deployment entries, not the
public OpenAI API catalog. The default conservative input budget is 272,000
tokens; no API dollar prices are inferred for subscription usage. Additional
model IDs can be configured through Advanced with the Responses transport when
supported by your account.

## Sign out

```text
/logout
```

Or:

```sh
latch logout
```

This removes **local** saved ChatGPT tokens. It keeps provider configuration,
session history and other providers' API keys. Existing provider objects reload
credentials before their next request, including child sessions. Already
in-flight requests are not revoked. To revoke remote authorization, use OpenAI's
account controls. Select another provider with `/model` or sign in again.

## Storage and security

- Configuration contains only `credential = "oauth:openai-codex"` with provider
  kind `openai-codex`. OAuth cannot be attached to arbitrary compatible endpoints.
- Tokens are stored in `chatgpt-auth.toml` under the configured Latch state
  directory, normally `~/.latch/`. Storage uses the existing private-root and
  atomic credential writer (0600 on Unix); aliases/hardlinks are refused.
- `chatgpt-auth.lock` serializes token rotation across Latch processes. The file
  may remain after logout; OS lock ownership ends when its file handle closes.
- Tokens refresh near expiry at request time, not during `latch doctor`. Setup
  and doctor can establish local credential presence, not remote validity.
- Tokens never enter durable events, model context, transcripts or browser
  metadata. The one-time device challenge is human-only, transient UI state.
- Subscription requests use the fixed Codex endpoint. Authentication and token
  HTTP clients do not follow redirects. Provider serialization remains in
  `provider.rs`; tool execution and approval rules are unchanged.
- One locally signed-in ChatGPT account is shared by subscription provider
  instances within a state directory. Separate state directories isolate accounts.
- The existing legacy-XDG migration does not transfer this new OAuth file;
  sign in again after migrating rather than copying rotating refresh tokens.

## Troubleshooting and qualification

| Message | Action |
| --- | --- |
| Device sign-in unavailable | Enable device code authorization in ChatGPT security settings and check organization policy. |
| Sign-in expired/cancelled | Start `/login` again and use the newly displayed code. |
| Token refresh failed / access revoked | Run `/login` again. Do not substitute an API key into the subscription provider. |
| Subscription access denied | Check Codex entitlement and account/organization permissions. |
| Usage limit reached | Wait for your subscription allowance to reset. There is no automatic paid fallback. |
| Credential files are aliased or too permissive | Use ordinary private files in the protected state directory; re-login if necessary. |

This is an independent implementation of the Codex device/Responses wire
protocol, not a general-purpose OpenAI OAuth developer platform integration.
The service uses the public Codex native-client identifier (not a client secret)
and Latch identifies requests with `originator: latch`. OpenAI controls protocol
availability, policy and entitlement; this is not an OpenAI endorsement or a
promise that the endpoint will remain unchanged.

Deterministic loopback tests cover authorization polling, PKCE token exchange,
refresh rotation/concurrency, failure redaction, storage restrictions,
cancellation/logout, streaming/tool/reasoning replay and truncated streams:

```sh
cargo test -p latch-kernel --lib oauth --locked
cargo test -p latch-cli --bin latch login --locked
```

They make no external requests and require no account. Live account sign-in and
plan/model entitlement require human verification; passing mock tests alone does
not establish live-service or native Windows/macOS qualification.

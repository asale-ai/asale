# Antigravity integration

Antigravity is an independent OAuth credential family (`antigravity`). Legacy
Gemini CLI accounts remain under `gemini`; their tokens are not migrated or
silently reused.

## Sell

Set `ASALE_OAUTH_CLIENT_ID_ANTIGRAVITY` and
`ASALE_OAUTH_CLIENT_SECRET_ANTIGRAVITY` in the private `.env.package` before
building, or in the daemon environment at runtime. The public client ID defaults
to the one used by CLIProxyAPI; the client secret is never committed. Optionally
set `ASALE_ANTIGRAVITY_VERSION` when updating the upstream client version.

Connect **Antigravity** from the Sell page. Login resolves the Google profile,
checks `loadCodeAssist` eligibility, performs `onboardUser` when required, and
fetches `fetchAvailableModels` before saving the account. Publication is limited
to discovered, priced text models from Google's and Anthropic's catalog entries.
A missing model list publishes no supply. Entitlements refresh in the background
and model aliases retain the upstream's spelling. Transient discovery failures
keep the last catalog; an explicit `UNSUPPORTED_CLIENT` clears it.

Inference uses the daily Antigravity endpoint and its project/session envelope.
Responses and SSE are unwrapped into the Gemini wire format before normal
market protocol conversion, usage metering, and settlement.

An `UNSUPPORTED_CLIENT` rejection pauses every model of that account. It does
not retry on a daily timer or recover merely because an access token refreshed.
An explicit resume, or a successful Antigravity login/eligibility check, can
clear that pause.

Both the client and server must be updated for the new provider to be available.
No new database migration is required.

## Buy in Antigravity CLI

Use a CLI version that supports Gemini API-key mode, then enable
**Antigravity CLI** on the Buy page and select models. Asale configures:

- `~/.gemini/antigravity-cli/settings.json`: `modelProvider: "gemini"`.
- A private local key/base URL configuration and shell wrapper for `agy`.
- `GEMINI_API_KEY` and `GOOGLE_GEMINI_BASE_URL` for the CLI process only.
- The first selected model as the CLI's startup `--model`; all selected models
  remain available through the normal buy gate.

Open a new terminal after enabling the switch. POSIX shells are configured via
zsh, Bash, and fish startup files; Windows uses PowerShell profile functions.
The CLI does **not** load `.env` files. Keys are read at invocation, so rotation
is picked up without embedding credentials in shell startup files. Disabling
buying restores the previous provider and removes Asale's wrapper while
preserving unrelated settings and shell edits made since enabling it. Invalid
JSON or a failed configuration write rolls the changes back.

The CLI can run in the Antigravity IDE's integrated terminal. The IDE's built-in
chat does not have a verified equivalent custom endpoint configuration and is
not redirected by this switch.

Gemini model-list requests and `countTokens` preflights are supported. Token
counts are heuristic estimates, consistent with the gateway's existing count
endpoint, and do not trigger inference or settlement.

Protocol reference: `../CLIProxyAPI/internal/auth/antigravity/`,
`../CLIProxyAPI/internal/runtime/executor/antigravity_executor*.go`, and
`../CLIProxyAPI/sdk/cliproxy/antigravity_models.go`.

Official CLI configuration:
<https://antigravity.google/docs/cli/install/>.

## Offline validation and remaining limits

Regression coverage includes OAuth offline consent, onboarding tier selection,
account rejection, model discovery, integer session IDs, API-key shell scoping,
configuration rollback, preservation of user edits, model-path substitution,
and native Gemini tool-call signature/ID replay through streaming and buffered
responses. These are local tests with synthetic credentials, not a successful
Google subscription test.

Antigravity tool schemas are normalized only within function declarations:
local references are resolved, nullable types and missing array items handled,
and unsupported constraints retained as description hints. Claude tool unions
are simplified for the bridge's VALIDATED mode; this is not exact JSON Schema
validation. Recursive or external references are rejected locally. Conversation
arguments are never schema-cleaned. Cross-dialect signed reasoning, account
eligibility, quota behavior, and actual upstream inference still require a real
account integration test. No paid-account or live CLI test has been completed.

# Agent channel protocol (v1)

How an AI agent (first client: Muse) talks to a NevoFlux head over the
remote relay. Protocol version **1**. The machine-checkable half of this
document is `crates/daemon/tests/fixtures/muse/`; when the two disagree, the
fixtures win and this document has a bug.

## 1. Pairing

The person runs `/pair-agent` in the NevoFlux sidebar and pastes the block it
shows into the agent:

```
NEVOFLUX_AGENT_PAIRING
relay: wss://relay.nevoflux.app
channel: 2f1c4a90-7b3e-4d1a-9c58-0e6a2b7d4f31
code: A-BCDE-FGHJ-KMNP
```

Read the three values by prefix. Normalize the code before anything else
(`fixtures/muse/pairing_code.json`): drop every character outside
`[0-9A-Za-z]`, uppercase, map `I`/`L` → `1` and `O` → `0`, require exactly 13
characters of `0123456789ABCDEFGHJKMNPQRSTVWXYZ` (reject otherwise, never
guess), regroup as `X-XXXX-XXXX-XXXX`.

Channel key = Argon2id(v0x13, m = 65536 KiB, t = 3, p = 4, 32 bytes) with
password = the normalized code and salt = `"{code}|{channel}"`, both UTF-8
(`fixtures/muse/kdf.json`). Derive it once and store the key, not the code.

## 2. Relay admission

`{relay}/?c={channel}&t={jwt}`, where `relay` is the `relay:` value from the
block (it already starts with `wss://`). The JWT comes from the person's
NevoFlux account via the device authorization grant (`client_id =
nevoflux-muse`): the account must be the same one the head is signed in to,
or the relay answers 403. Always dial out; the head never connects to you.

The relay sends plaintext text messages `{"k":"peers","n":N}`, where N counts
the *other* sockets on the channel, never the recipient itself. Everything
else is binary and sealed. The head never reads any other text message: an
envelope sent as text is dropped unanswered.

## 3. Sealing

Every binary message = `nonce(12) || AES-256-GCM(ciphertext || tag(16))`
under the channel key, fresh random nonce per message
(`fixtures/muse/seal.json`). The plaintext is JSON:

```json
{"k": "frame", "frame": <envelope>}
```

There is no `seq`, `resume` or `resync` on this channel. A message that does
not open is dropped silently.

## 4. Envelope

```json
{"d": "c2h", "n": 17, "ch": "<challenge>", "m": { ...JSON-RPC 2.0... }}
```

| field | rule |
|---|---|
| `d` | `"c2h"` agent → head, `"h2c"` head → agent; accept only the opposite of what you send |
| `ch` | this connection's challenge; must equal it on every frame, both ways |
| `n` | per direction, strictly increasing (gaps allowed); starts at 0 |
| `m` | one JSON-RPC message (a JSON object); `null` only in the head's challenge frame |

On every connection the head speaks first: when it sees you arrive it sends
`{"d":"h2c","n":0,"ch":<new 16-byte challenge, base64url, no padding>,"m":null}`.
Wait for it before sending `initialize`; if it has not arrived within 10
seconds, disconnect and dial again. Every reconnect gets a new challenge and
both counters restart. The head decides a connection has arrived when the
relay's `peers` count rises, so a second socket from you on the same channel
also starts a new session and ends the first.

A challenge frame (`d=h2c`, `n=0`, `m=null`) may arrive at any time, not only
right after you connect: when the head itself reconnects or restarts, your
socket stays up but the head greets you again. On receiving one: drop the
current MCP session, reset both counters, and run `initialize` again. Ignore
any `h2c` frame whose `ch` is not the most recent challenge.

The head drops frames that fail these checks without answering. It checks, in
order: shape (an object with `d`, `n` as a non-negative integer, `ch` and `m`
all present), direction, challenge, counter, and finally that `m` is a JSON
object — so a frame whose `m` is `null` or not an object is a shape failure
reported only after its counter has been checked
(`fixtures/muse/envelope.json` lists the cases). After 8 refused frames the
head drops the MCP session and stops answering on that connection until you
redial.

## 5. MCP

Standard MCP over the envelope's `m`: `initialize` → `notifications/initialized`
→ requests. One MCP session per connection; a disconnect ends it, and a call
in flight when it ends gets no answer.

`initialize` result:

- `serverInfo.name = "nevoflux-head"`, `serverInfo.version` = head version
- `capabilities.experimental.nevoflux.protocol = 1` — refuse to proceed on a
  version you do not support

Tools: `tools/list` shows the head's whitelist with their schemas:
`browser_snapshot`, `browser_get_markdown`, `browser_screenshot`,
`browser_get_tabs`, `browser_navigate`, `browser_activate_tab`,
`browser_click_by_id`, `browser_fill_by_id`, `browser_type_by_id`,
`browser_click`, `browser_fill`, `browser_type`, `browser_key_press`,
`browser_scroll`, `browser_wait_for`, `browser_upload_file`. Calls run in the
person's desktop NevoFlux browser.

Example requests: `fixtures/muse/mcp_messages.json`.

### 5.1 Tiers

Every agent pairing has its own execution tier, `read-only` when paired. The
person changes it in the NevoFlux sidebar with `/agent-tier <name> <tier>`;
nothing on this channel can. A tier change applies from the next call. The tier is a ceiling: a call above it fails at
once with `tier_denied` and nobody is asked.

| tools | first tier that runs them |
|---|---|
| snapshot, get_markdown, screenshot, get_tabs, navigate, wait_for | `read-only` |
| click_by_id, fill_by_id, type_by_id, click, fill, type, key_press, scroll, activate_tab | `browser-auto` |
| upload_file | `full-auto` |

(`browser-auto-local-read` adds nothing on this channel.)

### 5.2 Tabs

You work in tabs of your own. Without `tab_id`, `browser_navigate` opens a tab
for you in the background the first time and reuses it afterwards;
`new_tab: true` opens another. Every other tool without `tab_id` (except `browser_get_tabs`, which
lists all tabs) acts in your most recently opened tab, or fails with `no_agent_tab` if you have none. With
`tab_id` any tab can be read or acted on, within your tier.
`browser_get_tabs` marks each tab `owned: true` (yours) or `owned: false` (the
person's). `browser_activate_tab` requires `tab_id` and changes what the
person sees; without one it fails with `tool_failed` (`tab_id is required`),
not `no_agent_tab`. A `tab_id` that is not a positive integer, on any tool,
also fails with `tool_failed`.

Your tabs are forgotten when the browser disconnects or restarts (tab ids are
reused after a restart); navigate again to get a new one. A tab that was
closed answers `tab_closed` once and is then forgotten.

### 5.3 Errors

A call that was understood but refused or failed is a normal JSON-RPC result
with `isError: true`:

```json
{"isError": true,
 "content": [{"type": "text", "text": "<the structuredContent object as JSON>"}],
 "structuredContent": {"error": {"code": "tier_denied", "message": "…", "tier": "read-only", "required": "browser-auto"}}}
```

| `code` | meaning | extra fields |
|---|---|---|
| `unknown_tool` | not on the whitelist | |
| `tier_denied` | above this pairing's tier | `tier`, `required` |
| `approval_denied` | refused | `reason`: `declined` (the person said no), `client_cannot_approve` (your client did not declare elicitation), `rule` (a site rule, a pack hook, or the allowlist refused the call outright), `approval_failed` (the question could not be put) |
| `approval_timeout` | nobody answered within 60 s | |
| `browser_unavailable` | the desktop browser is not connected | |
| `no_agent_tab` | no tab of yours and no `tab_id` | |
| `tab_closed` | the tab is gone | `tab_id` |
| `tool_failed` | the call was malformed (for example a bad `tab_id`), or the browser failed it or did not answer within 150 s; `detail` says which | `detail` |

Branch on `code` and the extra fields; `message` is for people. Every case is
in `fixtures/muse/tool_errors.json` exactly as the head sends it.
JSON-RPC errors still mean a malformed request. A head before M2 answered
off-whitelist calls with JSON-RPC errors carrying `data.code` `not_allowed` or
`unavailable`; an M2 head sends neither.

### 5.4 Approvals

When a rule wants a person to confirm a call within your tier, the head sends
`elicitation/create` (form mode) while that call is still open, and answers
the call only after the person decides:

```json
{"jsonrpc": "2.0", "id": 7, "method": "elicitation/create",
 "params": {"mode": "form",
            "message": "Click on element 'e1'\nTab: tab 1\nWhy: …",
            "requestedSchema": {"type": "object", "properties": {"approve": {"type": "boolean"}}, "required": ["approve"]},
            "_meta": {"io.nevoflux/call": "0badc0de", "progressToken": 7}}}
```

- Declare `capabilities.elicitation` in `initialize`, or every such call fails
  with `approval_denied` / `client_cannot_approve`. Only form mode is used.
- Optionally stamp a `tools/call` with `_meta: {"io.nevoflux/call": "<id>"}`;
  the head copies it into the elicitation's `_meta` so you know which call is
  asking. Without the stamp the elicitation carries no such key.
- The elicitation's `_meta` may also carry `progressToken`, added by the MCP
  library; it always equals the request `id`. Accept extra `_meta` keys, and
  do not pin the request id or the token to any particular value.
- Answer `{"action": "accept", "content": {"approve": true}}` to run the call.
  Anything else — `approve: false`, `decline`, `cancel` — refuses it
  (`approval_denied`, `reason: declined`).
- Answer within 60 s. After that the head sends `notifications/cancelled` for
  the elicitation and the call fails with `approval_timeout`.
- Every question is asked separately; there is no "always allow".

Request, answers and errors: `fixtures/muse/tool_errors.json`.

### Limitations

The head buffers at most 32 inbound messages per session. If you send faster
than the head processes (more than 32 unprocessed), further requests are
dropped without a response, and their counters are spent, so sending the same
frame again is refused as a replay. Do not pipeline more than a handful of
requests, and use your own per-request timeout.

## 6. Versioning

`fixtures/muse/PROTOCOL_VERSION` holds the protocol number and always equals
`capabilities.experimental.nevoflux.protocol`. A client pins a copy of the
fixtures and checks `PROTOCOL_VERSION` first.

## 7. Testing against a stub head

`cargo run -p nevoflux-daemon --example agent_stub_head` runs a head that
dials a relay and serves canned answers, so a client can be tested without a
browser. Environment:

| variable | meaning |
|---|---|
| `NF_STUB_RELAY` | relay base URL, default `ws://127.0.0.1:8765` |
| `NF_STUB_CHANNEL` | channel id (required) |
| `NF_STUB_CODE` | the normalized pairing code (required) |
| `NF_STUB_TOKEN` | sent as the relay `t` parameter instead of a minted JWT, default `test` |

It answers `browser_snapshot` and `browser_navigate` with fixed results and
lists only whitelisted tools; a whitelisted tool it does not stub answers `unknown_tool` (section 5.3).

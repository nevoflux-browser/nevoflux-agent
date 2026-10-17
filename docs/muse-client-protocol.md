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
person's desktop NevoFlux browser. A head that offers delegation (§8) also lists
`nevoflux_task_start`, `nevoflux_task_wait`, `nevoflux_task_approve` and
`nevoflux_task_cancel`.

Example requests: `fixtures/muse/mcp_messages.json`.

### 5.1 Tiers

Every agent pairing has its own execution tier, `read-only` when paired. The
person changes it in the NevoFlux sidebar with `/agent-tier <id> <tier>`, where `<id>` is the agent's short id shown by `/devices`;
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
`tab_id` any tab can be read or acted on, within your tier — but the id must
come from your own `browser_navigate` or from a `browser_get_tabs` result in
the current browser session. Any other id (one from before a browser restart
or reconnect, or one you guessed) fails with `tab_closed` and nothing is sent;
call `browser_get_tabs` first to act in one of the person's tabs.
`browser_get_tabs` marks each tab `owned: true` (yours) or `owned: false` (the
person's). `browser_activate_tab` requires `tab_id` and changes what the
person sees; without one it fails with `tool_failed` (`tab_id is required`),
not `no_agent_tab`. A `tab_id` that is not a positive integer, on any tool,
also fails with `tool_failed`.

`browser_navigate` only opens `http://` and `https://` URLs (any letter case)
and `about:blank`. Anything else — `file:`, `about:` pages, `nevoflux:`,
`javascript:`, `data:`, or a URL with leading whitespace — fails with
`tool_failed` (`only http and https URLs can be opened`) and nothing is sent.

Your tabs, and the ids `browser_get_tabs` showed you, are forgotten when the
browser disconnects or restarts (tab ids are reused after a restart); navigate
again to get a new one. A tab that was closed answers `tab_closed` and is then
forgotten.

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
| `tab_closed` | the tab is gone, or the `tab_id` is not one you opened or were shown by `browser_get_tabs` in the current browser session | `tab_id` |
| `tool_failed` | the call was malformed (for example a bad `tab_id`, or a URL that is not http/https/`about:blank`), or the browser failed it, or did not answer within 150 s — in that last case the action may still have happened (`detail`: `the browser did not answer within 150 s; the action may still have happened`); `detail` says which | `detail` |

Branch on `code` and the extra fields; `message` is for people. Every case is
in `fixtures/muse/tool_errors.json` exactly as the head sends it.
JSON-RPC errors still mean a malformed request. A head before M2 answered
off-whitelist calls with JSON-RPC errors carrying `data.code` `not_allowed` or
`unavailable`; an M2 head sends neither.

The task tools of §8 answer the same way, with the codes in §8.5 as well.

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

Known limits of this version:

- Site rules match the URL a `browser_navigate` goes to, not the page a
  click, fill or type lands on. A rule that asks before acting on a site
  guards your navigates there, not your later clicks in a tab already on it.
- If you cancel a `tools/call` (`notifications/cancelled`), the person's audit
  shows the call but no result for it.
- If a headless NevoFlux browser is registered with the same daemon, it
  answers agent calls instead of the person's desktop browser.

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

## 8. Delegation

A head that lists `nevoflux_task_start` in `tools/list` lets you hand the
person's desktop NevoFlux a whole task: one full agent turn, run in a
background session of its sidebar, in Agent mode, with skills, Canvas and
every tool **this pairing's tier** allows. The tier (§5.1) is the boundary,
not the browser whitelist of §5: above it a tool is refused at once and
nobody is asked, and the person lowering the tier stops the next tool call.
Within the tier, ten sensitive tools always need an approval, whatever was
allowed before: by default only the person at the desktop can give it, and
the person can let you answer them too; see §8.3. Whatever the tier, the
task's file-writing tools cannot write into NevoFlux's own configuration or
data directory (its settings, pairings and database): such a write fails with
`A task delegated by a paired agent cannot change NevoFlux's own settings or pairings.`
Shell commands are not restricted — full-auto is full access. The four task
tools themselves work at every tier. The protocol number stays `1`; detect
delegation by the tool.

Schemas: `fixtures/muse/task_tools.json`. Results: `fixtures/muse/task_results.json`.

### 8.1 Calls

| tool | arguments | result (`structuredContent`) |
|---|---|---|
| `nevoflux_task_start` | `prompt` (1–8000 characters after trimming); `session` (optional) | `{"task_id", "session", "state": "running"}` |
| `nevoflux_task_wait` | `task_id`; `timeout_secs` (integer 1–60, default 30) | the task (§8.2) |
| `nevoflux_task_approve` | `task_id`, `approval_id`, `choice` (`allow` or `deny`); `answer` (≤ 4000 characters, for a question) | `{"ok": true, "state"}` |
| `nevoflux_task_cancel` | `task_id` | `{"task_id", "state"}`: `cancelled`, or the final state of a task that already ended |

- One task per pairing at a time; at most 20 starts in any hour.
- `session` continues one of your earlier tasks (for example "now make the
  buttons blue"): pass the `session` an earlier start returned. Any other
  session is `session_not_found`.
- Malformed arguments answer `tool_failed` with `detail` one of:
  `prompt is required`, `prompt must be 1 to 8000 characters`,
  `session must be a non-empty string`,
  `timeout_secs must be an integer from 1 to 60`, `choice must be allow or deny`
  (also for `allow_always`, which only the person can choose),
  `answer is required for a question`, `answer must be a string`,
  `answer must be at most 4000 characters`, `answer is only allowed with allow`,
  `<field> is required`.
- Task ids, session ids and approval ids are UUIDs (36 characters, lowercase,
  hyphenated); match them case-insensitively. Your own
  elicitation ids from §5.4 are 8 hex characters, so the two never collide.

### 8.2 The task

```json
{"task_id": "…", "session": "…", "state": "running | awaiting_approval | done | failed | cancelled",
 "steps": [{"id": "toolu_01", "at": "2026-10-14T13:05:56Z", "tool": "skill_load", "status": "started | ok | failed"}],
 "steps_truncated": true,
 "approvals": [ … §8.3 … ],
 "reply": "…", "reply_truncated": true,
 "screenshot_error": "no_tab",
 "error": {"code": "approval_timeout | llm_error | browser_unavailable | interrupted", "message": "…"}}
```

- `steps` are events: a tool call gives a `started` event and later one `ok`
  or `failed` event with the same `id`. Each wait returns the events since
  your previous wait **in this MCP session**; after a reconnect the first
  wait replays from the oldest kept event. At most 500 events are kept;
  `steps_truncated: true` says some you had not seen were dropped. `at` is UTC.
- `approvals` only while `awaiting_approval`, oldest first. `reply` only on
  `done`: the assistant's text after its last tool call, at most 32 000
  characters (`reply_truncated: true` when cut). `error` only on `failed`.
  `steps_truncated`, `reply_truncated` and `screenshot_error` appear only
  when they apply. Accept keys you do not know.
- A whole result is kept within 900 KiB (921 600 bytes, the relay's message
  budget): over it, the picture is dropped first (`screenshot_error:
  "too_large"`), then `reply` is halved until it fits, so `reply_truncated:
  true` can appear on a reply shorter than 32 000 characters.
- The **first** `done` result in an MCP session carries one picture of what
  the task produced (the Canvas it generated, else the tab it opened, else
  the tab it last acted on) as `content[1]`:
  `{"type": "image", "data": "<base64>", "mimeType": "image/jpeg"}` — at most
  1280 px wide, 2400 px tall, 300 KB. Later waits in that session omit it.
  With no picture, `done` carries `screenshot_error`: `no_tab` (nothing was
  opened or touched), `tab_discarded`, `tab_closed`, `too_large`, `timeout`,
  `capture_failed`, `browser_unavailable`.
- A wait returns at once when an approval is open that no earlier wait in
  this MCP session returned, when the task's state changes while you wait
  (an answered approval puts it back to `running`), or when the task has
  ended; otherwise at `timeout_secs`. An approval a wait already returned
  does not end the next wait in that MCP session early, so waiting again on
  an approval only the person can answer blocks until something changes.
  After a reconnect the first wait returns the open approvals at once. Steps
  do not end a wait early.
- `failed` messages: `approval_timeout` — `nobody answered an approval within
  30 minutes; it was denied`; `llm_error` — `the model call failed: <detail>`;
  `browser_unavailable` — `the NevoFlux desktop browser disconnected`;
  `interrupted` — `the task was interrupted`. Cancelling, pausing or
  unpairing ends a task as `cancelled`, without `error`.
- A finished task can be waited on for an hour. After that, or after the
  desktop's NevoFlux restarts, it is `task_not_found` (its session remains).
- Your MCP connection dropping does not stop a task; wait again after you
  reconnect.

### 8.3 Approvals

```json
{"approval_id": "…", "kind": "tool | question | plan", "summary": "Create a Canvas artifact: \"Calculator\"",
 "tool": "create_artifact", "target": null, "question": "…", "options": ["…"], "plan_steps": ["…"],
 "choices": ["allow", "deny"], "at": "2026-10-14T13:06:10Z", "expires_in": 1740}
```

- `tool`: a permission prompt (a site or pack rule, a risk flag, an ACP
  agent's own permission request, one of the ten sensitive tools below).
  `question`: the agent asks you something; `question` and `options`
  (possibly empty) are present, answer with `choice: "allow"` and the text
  in `answer`, or `deny` to cancel the question. `plan`: a plan to confirm;
  `plan_steps` is present; `allow` runs it, `deny` cancels it.
- `tool` is the running tool, or `null`. `target` is always `null` in this
  version. `expires_in` is whole seconds left of 30 minutes; unanswered by
  then, the approval is denied and the task ends `failed`
  (`approval_timeout`).
- The person at the desktop can answer the same approval. The first answer
  wins; the other side's is `approval_not_found` — wait again to see the
  task's state.
- Ten sensitive tools always ask, even at full-auto and whatever was allowed
  before: when the task wants to schedule work, run or resume a schedule,
  start a loop, switch a pack or the model, set a goal, replace the system
  prompt or share the screen. The must-approve question's `summary` is
  `A task delegated by a paired agent wants to run <tool>. This action always needs approval.`;
  other questions about the same call (a site, pack or policy rule) can also
  carry `desktop_only`.
  By default only the person at the desktop may answer it: it carries
  `"desktop_only": true` with `"choices": []`, and answering it is
  `approval_desktop_only`; ask the person to approve it at the desktop, or to
  turn on "Let Muse approve sensitive actions" for this agent there, and
  wait. With that setting on, the same approvals arrive as ordinary ones
  (`choices: ["allow", "deny"]`, no `desktop_only`; first answer wins as
  above). The setting can change while an approval waits: read
  `desktop_only` from each wait result, never from an earlier one.
  Unanswered after 30 minutes it is denied like any other approval.

### 8.4 Progress

Put `_meta: {"progressToken": <token>}` on a `nevoflux_task_wait` call and,
while that wait is open, the head sends `notifications/progress`
`{progressToken, progress, message}` (no `total`), one per step event and one
per new approval; `progress` counts 1, 2, … within the wait. Messages:
`Running <tool>`, `<tool> finished`, `<tool> failed`,
`Waiting for approval: <summary>`.

Progress is optional: without a `progressToken` nothing is sent, and the same
events arrive as `steps` in the wait result (nevoflux-muse 0.4.0 sends none).

### 8.5 Errors

| `code` | when | extra fields |
|---|---|---|
| `task_busy` | a task of yours has not ended; or the desktop's ACP model provider is busy with another conversation | `task_id`, or `provider` |
| `rate_limited` | 20 starts in the last hour | `retry_after` (seconds, ≥ 1) |
| `session_not_found` | `session` is not one of your task sessions | `session` |
| `task_not_found` | unknown, someone else's, ended over an hour ago, or the head restarted | `task_id` |
| `approval_not_found` | not waiting for an answer (answered on the desktop, expired, unknown) | `approval_id` |
| `approval_desktop_only` | only the person at the desktop may answer this approval (`desktop_only`; "Let Muse approve sensitive actions" is off) | `approval_id` |
| `paused` | the person paused you | |
| `browser_unavailable` | the desktop sidebar is not connected | |
| `tool_failed` | malformed arguments (§8.1) | `detail` |

Every case: `fixtures/muse/tool_errors.json` (the `provider` form of
`task_busy` is its `task_busy_provider` entry).

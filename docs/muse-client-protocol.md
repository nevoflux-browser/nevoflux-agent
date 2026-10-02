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
else is binary and sealed.

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

The head drops frames that fail these checks without answering, and after 8
refusals drops the MCP session (`fixtures/muse/envelope.json` lists the cases
and the order they are checked in: shape, direction, challenge, counter).
A frame whose `m` is missing, `null` or not an object is a shape failure.

## 5. MCP

Standard MCP over the envelope's `m`: `initialize` → `notifications/initialized`
→ requests. One MCP session per connection; a disconnect ends it, and a call
in flight when it ends gets no answer.

`initialize` result:

- `serverInfo.name = "nevoflux-head"`, `serverInfo.version` = head version
- `capabilities.experimental.nevoflux.protocol = 1` — refuse to proceed on a
  version you do not support

Tools: `tools/list` shows only tools from the head's whitelist; `tools/call`
of anything else fails. In protocol v1 / head M1 the list is empty and every
call fails with `unavailable`; real browser tools arrive with head M2. The
whitelist is browser tools only: `browser_snapshot`, `browser_get_markdown`,
`browser_screenshot`, `browser_get_tabs`, `browser_navigate`,
`browser_activate_tab`, `browser_click_by_id`, `browser_fill_by_id`,
`browser_type_by_id`, `browser_click`, `browser_fill`, `browser_type`,
`browser_key_press`, `browser_scroll`, `browser_wait_for`,
`browser_upload_file`.

Errors are JSON-RPC errors with `code = -32602` and a machine-readable
`data.code`:

| `data.code` | meaning |
|---|---|
| `not_allowed` | the tool is not on the agent-channel whitelist |
| `unavailable` | the head has no tools to run yet |
| `unknown_tool` | the backend has no such tool |

Example requests: `fixtures/muse/mcp_messages.json`.

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
lists only whitelisted tools; anything else fails as in section 5.

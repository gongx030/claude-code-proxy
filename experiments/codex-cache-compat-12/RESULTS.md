# Codex prompt-caching compatibility (#12): results

**Verdict: NO-GO** for translating Anthropic `cache_control` into OpenAI
explicit cache breakpoints, **for the proxy's default Codex model
`gpt-6.1-sol`**. The subscription-backed Codex endpoint rejects
`prompt_cache_options` on both request lanes, and `prompt_cache_breakpoint` on
the Lite lane, with HTTP 400 on both accounts tested. The other models the proxy
routes to were not tested; their support is UNKNOWN (see *Limitations*).

No proxy behavior changes. The default remains implicit-only caching with the
existing `prompt_cache_key` mode, so no rollback is needed.

The offline audit found three prefix perturbations that the proxy itself
causes, filed separately: #13, #14 and #15.

Date of the live measurements: 2026-11-12. Endpoint behavior can change; the
endpoint-support answer is a dated observation, not a permanent property.

## Terms

- **Implicit caching**: the upstream reuses the longest previously seen prefix
  of a request automatically, with no request fields beyond `prompt_cache_key`.
- **Explicit breakpoint**: a `prompt_cache_breakpoint` object on an input
  content block, plus request-level `prompt_cache_options`. OpenAI documents
  these for GPT-5.6 and later on the public Responses API
  ([prompt-caching guide](https://developers.openai.com/api/docs/guides/prompt-caching),
  accessed 2026-11-12). The guide also states that top-level `instructions` and
  `additional_tools` items cannot carry a breakpoint.
- **Item prefix**: every `input` item of turn N, serialized as canonical JSON,
  equals the item at the same index in turn N+1, and N+1 has more items. The
  tests compare canonical JSON per item. Wire bytes are the same today because
  `serde_json` is built without `preserve_order` and struct field order is fixed.
- **Full lane / Lite lane**: the two request shapes the proxy sends to the same
  Codex URL.
  - The full lane uses top-level `instructions` and `tools`.
  - The Lite lane sends `input[0]` as an `additional_tools` developer item and
    `input[1]` as the system text in a developer message, then the history.
  - `gpt-6.1-sol` (`CLAUDE_CODEX_MODEL`'s default in unified-agent-sdk) uses
    the Lite lane unless `CCP_CODEX_FULL_LANE` is set.
- **Same thread**: successive requests of one conversation (main agent, or one
  subagent), as opposed to a main/subagent switch.

## 1. Offline baseline (synthetic requests)

The inputs were synthetic Claude Code-shaped Messages bodies for `gpt-6.1-sol`,
translated by the real `translate_request` on each lane. There were no network
calls and no credentials were read. Every row marked **T** is asserted in
`tests/codex_cache_prefix.rs`. Rows marked **S** are read from source and not
tested.

### cache_control disposition

- **T:** Anthropic `cache_control` and the billing-header system block are
  absent from every translated body, and removing every `cache_control` marker
  leaves the output unchanged.
- **T:** no `prompt_cache_options` or `prompt_cache_breakpoint` field is ever
  emitted.
- **S:** the cause is implicit. The parsers read only the fields they need
  (`flatten_system_text` and `parse_content_block` in
  `src/providers/translate_shared.rs`, and tool translation in
  `src/providers/codex/translate/request.rs`).
- **S:** `prompt_cache_key` is the only cache control. Its mode defaults to
  `session` (`config.rs`). The test checks only that both lanes pass through
  the same key.
- **S:** `CCP_CODEX_PREVIOUS_RESPONSE_ID` defaults to off, so the full input is
  sent on every turn.

### Full lane: pairwise structure (T)

| previous → current | `instructions` equal | `tools` equal | item prefix |
|---|---|---|---|
| main N → N+1 | yes | yes | yes |
| main → subagent | no | no | no; first diff at item 0 (inherent: different system prompt and tools) |
| subagent → later subagent turn | yes | yes | yes |
| main → after dynamic (ToolSearch) tool load | yes | **no** | yes |
| → compaction, message form | yes | yes | yes |
| → compaction, system-prompt form | no (inherent: summarizer prompt) | yes | yes |

Main N → N+1 stays an item prefix even though the billing nonce and the
position of the message-level `cache_control` marker change between turns
(**T**).

### Lite lane: pairwise structure (T)

| previous → current | `input[0]` and `input[1]` equal | item prefix | first differing item |
|---|---|---|---|
| main N → N+1 | yes | yes | none |
| main → subagent | no | no | 0 (inherent) |
| subagent → later subagent turn | yes | yes | none |
| main → ToolSearch load → next turn | yes | yes | none |
| → compaction, message form | yes | yes | none |
| → compaction, system-prompt form | `input[0]` yes, `input[1]` no | no | 1 (inherent: summarizer prompt) |
| compaction → replay, no prior deferred load | yes | no | 2 (inherent: history rewritten) |
| compaction → replay, deferred tool loaded before | **`input[0]` no**, `input[1]` yes | no | **0**: proxy-induced, #13 |

The proxy recognizes compaction in two forms: a summarizer system prompt
(`is_compact_request`), or a compaction user message under the main system
prompt (`is_compact_message_text`). Both forms are tested, and the tests assert
that each is detected (reasoning effort capped to `low`).

Translation is deterministic (**T**): the same request translates to identical
output twice, and a permuted schema key order changes nothing.

### Proxy-induced prefix perturbations

1. **#13 (Lite, T).** After compaction, a deferred tool loaded earlier is
   merged back into `input[0]`. Item 0 changes, so the reusable prefix ends
   before item 0. The system message item (`input[1]`) is unchanged, but its
   position in the byte stream moves, so it can no longer be reused.
2. **#14 (T on Lite; S for the full lane, shared code).** The Read-offset note
   on an earlier `function_call_output` comes from an in-memory store of 4,096
   entries (S). Once the entry is evicted, that earlier item translates
   differently (T). A proxy restart empties the store as well (S, not tested).
3. **#15 (S, diagnostics only).** `request_summary` reports 0 tools and 0
   instruction bytes on the Lite lane.

### Candidate breakpoint location (if support ever appears)

Under the guide's rules above, the only eligible stable developer-message
content boundary is Lite `input[1]`: the single `input_text` that holds the
joined system text. It follows the tools and is unchanged within a thread
(**T**), except across the system-prompt form of compaction.

## 2. Endpoint feasibility canary (live, bounded)

Scope, approved by the operator: at most 8 upstream requests per account, two
accounts (A and B), synthetic prompts only. The script sent HTTPS directly,
with no proxy listener involved. Tokens were read only and never refreshed.

The request body was about 2,100 input tokens and followed the proxy's
`gpt-6.1-sol` request shape for the lane. Each account had a fresh
`prompt_cache_key`.

| request | lane | added fields | account A | account B |
|---|---|---|---|---|
| R1 control | Lite | none | 200; input 2,117, cached 0 | 200; input 2,117, cached 0 |
| R2 repeat of R1 | Lite | none | 200; cached 1,920 (90.7%), uncached 197 | 200; cached 1,920 (90.7%), uncached 197 |
| R3 | Lite | `prompt_cache_options {mode: implicit}` | 400 | 400 |
| R4 | Lite | options + breakpoint on the developer `input_text` | 400 | 400 |
| R5 repeat of R4 | Lite | same as R4 | 400 | 400 |
| R6 unknown-field comparison | Lite | an invented top-level field | 400 | 400 |
| R7 | full | options + breakpoint | 400 | 400 |
| R8 | Lite | breakpoint only, no options | 400 | 400 |

Error texts, identical on both accounts:

- R3, R4, R5 and R7: `invalid_request_error` / `invalid_parameter`,
  "prompt_cache_options is not supported on this model".
- R8: `invalid_parameter`, "prompt_cache_breakpoint is not supported on this
  model".
- R6: "Unsupported parameter", with empty type and code.

The R6 comparison shows the two caching fields are not treated as unknown. The
endpoint recognizes both fields and refuses them for this model, with an error
different from the one for an unknown field.

`cache_write_tokens` was present in `input_tokens_details` and was 0 on every
successful request. Implicit caching already works without the new fields
(R2).

**Endpoint support, by field:**

| field | `gpt-6.1-sol`, Lite lane | `gpt-6.1-sol`, full lane | other models |
|---|---|---|---|
| `prompt_cache_options` | **NO** (both accounts) | **NO** (both accounts) | UNKNOWN (not tested) |
| `prompt_cache_breakpoint` | **NO** (both accounts) | not tested alone | UNKNOWN (not tested) |

## 3. Accepted-task A/B

**Not run.** An accepted task is a task whose output passed its own acceptance
check. The approved per-account A/B (implicit-only vs. opt-in breakpoint)
depended on the endpoint accepting breakpoints for the default model, and it
does not.

No per-accepted-task cache-read or uncached-input measurement exists from this
issue. The R2 figures are single-request numbers.

## Verdict and rationale

**NO-GO for `gpt-6.1-sol`.** No explicit-cache translation was implemented, not
even an opt-in one, because on the default model it can only produce HTTP 400.

The proxy's Claude-alias mappings (`gpt-6-sol`, `gpt-5.6-terra`, `gpt-6-luna`)
were outside the canary's approved budget. A future flag for those models would
first need its own bounded canary showing YES.

Implicit caching and the `prompt_cache_key` defaults are unchanged. The prefix
improvements the proxy controls are #13 and #14. Each changes the default
request output, so each needs its own before/after measurement.

## Limitations

- Only `gpt-6.1-sol` was tested live. Every other model the proxy routes to is
  UNKNOWN.
- The breakpoint without options was tested only on the Lite lane.
- The canary measured implicit caching on one repeated request per account. It
  says nothing about cross-turn or cross-subagent hit rates in real sessions.
- The offline facts are structural (canonical-JSON item prefixes). They are not
  observed cache hits.
- Receipts and the account mapping are kept in a private results store. This
  report holds anonymized aggregates only.

## Reproduce

```bash
cargo test --test codex_cache_prefix
```

The live canary is not reproducible from this repository by design: it needs
operator-approved account credentials.

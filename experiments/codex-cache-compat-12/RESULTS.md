# Codex prompt-caching compatibility (#12): results

**Verdict: NO-GO** for translating Anthropic `cache_control` into OpenAI
explicit cache breakpoints. The subscription-backed Codex endpoint rejects both
`prompt_cache_options` and `prompt_cache_breakpoint` for the production default
model (`gpt-6.1-sol`), with HTTP 400, on both accounts tested. No proxy behavior
changes. The default stays implicit-only caching with the existing
`prompt_cache_key` mode, so no rollback is needed.

The offline audit found three prefix perturbations that the proxy itself
causes, filed separately: #13, #14 and #15.

Date of the live measurements: 2026-11-12. Endpoint behavior can change; the
endpoint-support answer is a dated observation, not a permanent property.

## Terms

- **Implicit caching**: the upstream reuses the longest previously seen prefix
  of a request automatically, with no request fields beyond `prompt_cache_key`.
- **Explicit breakpoint**: a `prompt_cache_breakpoint` object on an input
  content block, plus request-level `prompt_cache_options`. OpenAI documents
  these for GPT-5.6+ on the public Responses API.
- **Strict prefix**: the serialized `input` of turn N is a byte prefix of turn
  N+1's (only the closing `]` differs).
- **Full lane / Lite lane**: the two request shapes the proxy sends to the same
  Codex URL. The full lane uses top-level `instructions` and `tools`. The Lite
  lane (`uses_responses_lite`, which includes the default `gpt-6.1-sol`) sends
  `input[0]` as an `additional_tools` developer item and `input[1]` as the
  system text in a developer message, followed by the history.

## 1. Offline baseline (synthetic requests)

All requests were synthetic Claude Code-shaped Messages bodies, translated by
the real `translate_request`. There were no network calls and no credentials
were read. The regression tests live in `tests/codex_cache_prefix.rs`.

### cache_control disposition

- Anthropic `cache_control` is dropped implicitly on system blocks, message
  content blocks and tools, because the parsers read only the fields they need
  (`flatten_system_text`, `parse_content_block`, tool translation).
- The billing-header system block is removed.
- `ResponsesRequest` has no explicit-cache fields. `prompt_cache_key` is the
  only cache control. Its default mode is `session`, and it is set the same way
  on both lanes.
- `CCP_CODEX_PREVIOUS_RESPONSE_ID` defaults to off, so the full input is sent on
  every turn.

### Full lane: pairwise structure

| previous → current | `instructions` equal | `tools` equal | strict input prefix |
|---|---|---|---|
| main N → N+1 | yes | yes | yes |
| main → subagent | no | no | no (inherent: different system prompt and tools) |
| subagent → later subagent turn | yes | yes | yes |
| main → after dynamic tool load | yes | **no** | yes |
| tool load → compaction request | **no** (inherent: summarizer prompt) | yes | yes (and `reasoning.effort` is capped to `low` by `CCP_COMPACT_EFFORT`) |

Notes:

- Main N → N+1 stays a strict prefix even though the billing nonce,
  `metadata.user_id` and the moving message-level `cache_control` marker all
  change between turns.
- Translation is byte-deterministic. A permuted schema key order gives
  identical bytes, but the tool array keeps the client's order.

### Lite lane: pairwise structure

| previous → current | `input[0]` equal | strict input prefix | first differing item |
|---|---|---|---|
| main N → N+1 | yes | yes | none |
| main → subagent | no | no | 0 (inherent) |
| subagent → later subagent turn | yes | yes | none |
| main → ToolSearch load | yes | yes | none (the loaded tool is appended as its own item) |
| → compaction request | yes | yes | none |
| compaction → replay, no prior deferred load | yes | no | 2 (inherent: history rewritten) |
| compaction → replay, deferred tool loaded before | **no** | no | **0**: proxy-induced, #13 |

### Proxy-induced prefix perturbations (both lanes unless noted)

1. **#13 (Lite).** A deferred tool loaded before compaction is merged back into
   `input[0]` after compaction. That changes every following byte, including
   the system message.
2. **#14.** The Read-offset note on an earlier `function_call_output` comes from
   an in-memory store of 4,096 entries. A proxy restart or an eviction changes
   that earlier item (89 → 465 → 89 bytes in the test).
3. **#15 (diagnostics only).** `request_summary` reports 0 tools and 0
   instruction bytes on the Lite lane.

### Candidate breakpoint location (if support ever appears)

The only stable developer-message content boundary eligible under OpenAI's
rules is Lite `input[1]`, the single `input_text` that holds the joined system
text. It covers the tools and the system text, and it is byte-identical in
every same-thread pair. Top-level `instructions` and `additional_tools` are not
eligible.

## 2. Endpoint feasibility canary (live, bounded)

Scope, approved by the operator: at most 8 upstream requests per account, two
accounts (A and B), synthetic prompts only. The script sent HTTPS directly,
with no proxy listener involved. Tokens were read only and never refreshed.

The request body was about 2,100 input tokens and mirrored the proxy's own
`gpt-6.1-sol` request. Each account had a fresh `prompt_cache_key`.

| request | lane | added fields | account A | account B |
|---|---|---|---|---|
| R1 control | Lite | none | 200; input 2,117, cached 0 | 200; input 2,117, cached 0 |
| R2 repeat of R1 | Lite | none | 200; cached 1,920 (90.7%), uncached 197 | 200; cached 1,920 (90.7%), uncached 197 |
| R3 | Lite | `prompt_cache_options {mode: implicit}` | 400 | 400 |
| R4 | Lite | options + breakpoint on the developer `input_text` | 400 | 400 |
| R5 repeat of R4 | Lite | same as R4 | 400 | 400 |
| R6 negative control | Lite | an unknown top-level field | 400 (unsupported parameter) | 400 |
| R7 | full | options + breakpoint | 400 | 400 |
| R8 | Lite | breakpoint only, no options | 400 | 400 |

Error texts:

- R3, R4, R5 and R7: `invalid_parameter`, "prompt_cache_options is not supported on this model".
- R8: `invalid_parameter`, "prompt_cache_breakpoint is not supported on this model".
- `cache_write_tokens` was present in `input_tokens_details` and was 0 on every
  successful request.

**Endpoint support, by field:**

| field | `gpt-6.1-sol`, Lite lane | `gpt-6.1-sol`, full lane | other models |
|---|---|---|---|
| `prompt_cache_options` | **NO** (both accounts) | **NO** (both accounts) | UNKNOWN (not tested) |
| `prompt_cache_breakpoint` | **NO** (both accounts) | not tested alone | UNKNOWN (not tested) |

R6 shows the endpoint rejects unknown fields rather than ignoring them. So the
model-scoped rejections are real refusals, not a parser that ignores the fields.
Implicit caching already works without these fields (R2).

## 3. Accepted-task A/B

**Not run.** The approved A/B (implicit-only vs. opt-in breakpoint, per
account) depended on the endpoint accepting breakpoints. It does not, so there
is no treatment arm to compare.

No per-accepted-task cache-read or uncached-input measurement exists from this
issue. The R2 figures above are single-request numbers, not per-task numbers.

## Verdict and rationale

**NO-GO.** No explicit-cache translation was implemented, not even an opt-in
one. The production model rejects the request with HTTP 400, so an opt-in flag
could only produce failures. Implicit caching and the `prompt_cache_key`
defaults are unchanged.

The prefix improvements the proxy controls are #13 and #14. Each changes the
default request bytes, so each needs its own before/after measurement. They are
not bundled here.

## Limitations

- Only `gpt-6.1-sol` was tested live. The other Codex models the proxy routes,
  including GPT-5.6 Lite models, remain UNKNOWN.
- The breakpoint without options was tested only on the Lite lane.
- The canary used two requests' worth of implicit-cache evidence per account,
  which says nothing about cross-turn or cross-subagent hit rates in real
  sessions.
- The offline facts are structural (byte prefixes). They are not observed
  cache hits.
- The receipts and the account mapping are kept in a private results store.
  This report has anonymized aggregates only.

## Reproduce

```bash
cargo test --test codex_cache_prefix
```

The live canary is not reproducible from this repository by design: it needs
operator-approved account credentials.

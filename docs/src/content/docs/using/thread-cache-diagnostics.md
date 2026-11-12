---
title: Thread cache diagnostics
description: Optional, bounded observations of Codex cache counts and inter-turn timing.
---

Codex thread diagnostics observe requests to `/v1/messages` without changing upstream request bodies, headers, cache-key routing, continuation, compaction, retries, or execution policy. They are disabled by default. The default cache-key mode remains `session`. This instrumentation does not establish a cache-efficiency improvement.

## Enable for an isolated run

Set these environment variables before sending requests:

```sh
export CCP_CODEX_THREAD_DIAGNOSTICS=1
export CCP_CODEX_THREAD_DIAGNOSTICS_DIR=/tmp/new-private-codex-diagnostics
```

`1`, `true`, and `on` enable diagnostics (case-sensitive, surrounding whitespace ignored). All other values disable them. The directory must be **new**, with existing ancestors; do not create it beforehand. No path component may be a symlink, and `..` components are refused. Existing directories, files, and symlinks are refused, not reused or truncated. Choose a fresh directory for each run. Configuration is read at request entry; the sink is initialized lazily on its dedicated writer thread and shared across provider registries for the current configured path. Filesystem initialization never runs under the shared observer lock or on the asynchronous request worker. Banner and monitor setup do not claim the sink. Initialization or writer failure is retained for the current path: later requests do not retry it. Changing the path replaces the one cached observer, not an unbounded registry of sinks. Do not change configuration or restart or replace a proxy serving other sessions merely to collect observations.

The secure sink is available on Unix only. A missing path, creation error, or unavailable secure sink disables output without failing inference. A later write error stops output. Missing output is not evidence of zero misses. This sink is separate from general logs and traffic capture; enabling it does not enable either, and its privacy restrictions do not sanitize independently enabled traffic logs.

## Output and retention

The new directory is created with mode `0700`; files use `0600` (a restrictive umask may reduce permissions further). Newline-delimited JSON is written to `turns.jsonl`, with at most one `turns.previous.jsonl` backup. Each file is bounded to 1 MiB. Rotation discards the oldest backup. Creation walks directory components without following symlinks; subsequent file checks and rotation operate relative to the retained private directory handle. Renaming or substituting an ancestor cannot redirect writes or rotation to another directory. A renamed sink may continue writing to its original directory. This protects against other users modifying writable ancestors, not a hostile process with the same user privileges. No prompts, credentials, headers, cache keys, paths, session/agent identifiers, hashes, or thread ordinals are emitted.

A bounded queue holds at most 128 records. A full queue drops observations rather than waiting for disk. Process termination can lose queued records. There is no durable completeness guarantee or drop counter. Treat missing, rotated, or interrupted samples as incomplete, not as negative observations. Protect and delete the output according to your own retention requirements; the proxy does not delete the directory on exit.

In-memory tracking holds at most 1,024 thread entries with a 30-minute idle time-to-live, checked lazily at request entry. Active entries are pinned: only inactive entries expire or are evicted by least-recent touch. If all entries are active, new identities are not retained. A bounded global count and ambiguity flag suppress known gaps and predecessor creation until all outstanding identified requests, including refused admissions, finish. This conservatively prevents an untracked active request from becoming a false predecessor gap. Validated identities exist only in this bounded tracking map and guards for admitted active requests; active guards scale with concurrent requests. Request entry scans at most 1,024 entries. Locks cover bookkeeping only, never asynchronous operations or sink output. These are implementation bounds, not measured latency or throughput claims.

## Schema 1

Each logical request produces at most one record, including across existing retries:

| Field | Values / meaning |
| --- | --- |
| `schema` | `1` |
| `kind` | `main`, `agent`, `unknown` |
| `cache_key_mode` | `session`, `thread`, `omit`, `unknown` |
| `model` | Allow-listed family `sol`, `terra`, `luna`, otherwise `other`; never a raw model name |
| `lane` | `full`, `lite`, `unknown` |
| `gap` | Previous successful accepted upstream terminal to this request's entry, on the same validated thread |
| `duration` | Request entry to accepted terminal, error, cancellation, or missing-terminal detection |
| `outcome` | `success`, `failure`, `cancelled`, `missing_terminal`, `excluded` |
| `input_tokens` | Nonnegative integer reported by the accepted successful upstream terminal, or `null` |
| `cache_read_tokens` | Nonnegative upstream cached-input count, no greater than valid input count, or `null` |
| `cache_hit` | `full_miss` (positive input, zero cached), `full_hit` (positive input, all cached), `partial`, or `unknown` |

Both timing fields use `unknown`, `under1s` ([0,1) seconds), `s1_to10` ([1,10)), `s10_to60` ([10,60)), `m1_to5` ([60,300)), `m5_to30` ([300,1800)), or `m30_plus` ([1800,infinity)). Monotonic checked subtraction prevents a reversed timestamp from becoming a large positive interval.

A first observation, restart, expiration, eviction, invalid/missing identity, overlapping requests, or stale generation yields an unknown gap. Concurrent requests on one thread invalidate the ambiguous predecessor; neither completion establishes a successor gap. Sibling agents have distinct ownership. A nested agent's parent header is validated but does not change the direct agent's identity. No emitted identifier permits reconstructing individual thread histories.

Only accepted successful terminal events establish a predecessor. HTTP streaming response readiness is not completion; completion is observed before downstream terminal delivery, so downstream draining is excluded. Failure, cancellation, and missing terminal events clear the predecessor. An accepted upstream success remains success if downstream delivery subsequently fails. Standalone search is `excluded`; native Responses requests, token counting, and other routes are not instrumented. Early failures can have unknown cache mode/lane. Missing, malformed, negative, or inconsistent usage counts do not become zero; cache classification is unknown. Counts describe the accepted final response, not the sum of work across retries or auxiliary compaction requests.

## Descriptive comparison, not causal inference

Within a fixed `kind`, cache-key mode, model category, and lane, group successful records by **known** gap bucket. Define the eligible denominator as records with positive valid input counts and valid cached counts (`full_miss`, `partial`, or `full_hit`). The descriptive full-miss proportion is:

```text
number of eligible full_miss records / number of eligible records
```

Exclude unknown gaps and unknown cache classifications; separately report their counts, all non-success outcomes, and any known collection interruptions. Never silently place unknown observations in the full-miss numerator or the hit denominator. Preserve sample sizes and uncertainty; empty denominators are unknown, not zero. Model categories combine multiple model names, so they are coarse strata rather than proof of identical workloads.

Gap and cache status can share causes such as workload changes, scheduling, model selection, and upstream service behavior. Queue loss and rotation can also select observations. This comparison is **descriptive, not causal**: it does not identify a retention threshold, diagnose a cache mechanism, or justify adding delays or changing routing. No controlled-intervention study is launched by enabling this mode.

Keep four quantities separate: raw input-token counts, model-weighted cost, subscription-quota utilization, and task quality/acceptance. This output measures only raw upstream counts and timing categories. Weighted cost, quota effects, and task quality remain unknown without independent measurements; do not infer subscription savings from token counts.

## Offline synthetic replay

The public fixture `tests/fixtures/thread-cache-diagnostics.json` contains only synthetic identifiers and timings. Replay it without an account or upstream access:

```sh
cargo test --offline --lib providers::codex::diagnostics::tests::synthetic_replay_is_thread_local_and_contains_no_canaries
```

The replay checks interleaved main, sibling, nested, follow-up, and malformed identities and asserts that fixture canaries never appear in emitted records. Other focused tests cover overlap, eviction, lifecycle, usage validation, and private bounded output. HTTP mock tests compare actual upstream body bytes and headers with diagnostics absent, explicitly disabled, and enabled.

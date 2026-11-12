//! Prompt-prefix stability of the Anthropic -> Codex Responses translation, both
//! lanes (gongx030/claude-code-proxy#12). Offline: synthetic fake text, no
//! network, hermetic config dir. Structural byte facts, not cache-hit claims.

use std::collections::BTreeSet;
use std::ffi::OsString;
use std::sync::{Mutex, MutexGuard, PoisonError};

use claude_code_proxy::MessagesRequest;
use claude_code_proxy::providers::codex::compaction::{
    activate_compaction, apply_compaction_replay, begin_compaction, store_compaction,
};
use claude_code_proxy::providers::codex::translate::model_allowlist::uses_responses_lite_with_full_lane;
use claude_code_proxy::providers::codex::translate::read_rewrite::sanitize_read_args;
use claude_code_proxy::providers::codex::translate::reasoning_signature::{
    ReasoningReplay, encode_reasoning_signature,
};
use claude_code_proxy::providers::codex::translate::request::{
    ResponsesContentPart, ResponsesInputItem, ResponsesRequest, TranslateOptions, translate_request,
};
use serde_json::{Value, json};
use tempfile::TempDir;

const MODEL: &str = "gpt-6-luna";
const CACHE_KEY: &str = "sess-fake-0001";
const MAIN_PROMPT: &str = "You are a fake CLI agent. Fake main system prompt.";
const SUMMARIZER_PROMPT: &str = "You are a fake assistant that summarizes conversations.";
const COMPACT_INSTRUCTION: &str = "CRITICAL: Respond with TEXT ONLY. Do NOT call any tools. \
Your task is to create a detailed summary of the conversation so far";
const SUMMARY: &str = "Fake portable summary of the earlier conversation, long enough to anchor \
replay. Fake portable summary of the earlier conversation, long enough to anchor replay.";
const PNG: &str = "iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAYAAAAfFcSJAAAADUlEQVR42mNk+M9QDwADhgGAWjR9awAAAABJRU5ErkJggg==";
const BASH_TOOL: &str = r#"{"name":"Bash","description":"fake Bash tool","input_schema":{"type":"object","properties":{"command":{"type":"string"},"timeout":{"type":"number"}}}}"#;
const BASH_TOOL_PERMUTED: &str = r#"{"input_schema":{"properties":{"timeout":{"type":"number"},"command":{"type":"string"}},"type":"object"},"description":"fake Bash tool","name":"Bash"}"#;

static ENV_LOCK: Mutex<()> = Mutex::new(());

/// Serializes env access; points CCP_CONFIG_DIR at an empty tempdir and clears
/// CCP_CODEX_* and CCP_COMPACT_EFFORT, restoring all on drop.
struct Hermetic {
    saved: Vec<(OsString, Option<OsString>)>,
    _config: TempDir,
    _lock: MutexGuard<'static, ()>,
}

impl Hermetic {
    fn new() -> Self {
        let lock = ENV_LOCK.lock().unwrap_or_else(PoisonError::into_inner);
        let config = TempDir::new().unwrap();
        let mut keys: Vec<OsString> = std::env::vars_os()
            .map(|(k, _)| k)
            .filter(|k| k.to_str().is_some_and(|k| k.starts_with("CCP_CODEX_")))
            .collect();
        keys.extend(["CCP_COMPACT_EFFORT".into(), "CCP_CONFIG_DIR".into()]);
        let saved: Vec<_> = keys
            .into_iter()
            .map(|k| {
                let v = std::env::var_os(&k);
                (k, v)
            })
            .collect();
        // SAFETY: every env mutation in this binary holds ENV_LOCK.
        unsafe {
            for (k, _) in &saved {
                std::env::remove_var(k);
            }
            std::env::set_var("CCP_CONFIG_DIR", config.path());
        }
        Self {
            saved,
            _config: config,
            _lock: lock,
        }
    }
}

impl Drop for Hermetic {
    fn drop(&mut self) {
        // SAFETY: ENV_LOCK is still held; `_lock` drops after this body.
        unsafe {
            for (k, v) in self.saved.drain(..) {
                match v {
                    Some(v) => std::env::set_var(&k, v),
                    None => std::env::remove_var(&k),
                }
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Synthetic Claude Code-shaped requests
// ---------------------------------------------------------------------------

fn tool(name: &str, props: Value) -> Value {
    json!({"name": name, "description": format!("fake {name} tool"),
           "input_schema": {"type": "object", "properties": props}})
}

fn lookup_tool() -> Value {
    json!({"name": "mcp__fake__lookup", "description": "fake lookup", "defer_loading": true,
           "input_schema": {"type": "object", "properties": {"q": {"type": "string"}}}})
}

fn main_tools(loaded: bool) -> Vec<Value> {
    let mut tools = vec![
        serde_json::from_str(BASH_TOOL).unwrap(),
        tool(
            "Read",
            json!({"file_path": {"type": "string"}, "offset": {"type": "number"},
                   "limit": {"type": "number"}}),
        ),
        tool(
            "Edit",
            json!({"file_path": {"type": "string", "pattern": "^/"}}),
        ),
        tool("ToolSearch", json!({"query": {"type": "string"}})),
        json!({"name": "DeferredToolPlaceholder", "description": "never call", "defer_loading": true,
               "input_schema": {"type": "object", "properties": {}}}),
        tool("Agent", json!({"prompt": {"type": "string"}})),
    ];
    if loaded {
        tools.push(lookup_tool());
    }
    let last = tools.len() - 1;
    tools[last]["cache_control"] = json!({"type": "ephemeral"});
    tools
}

fn system(nonce: u32, prompt: &str) -> Value {
    json!([
        {"type": "text", "text": format!("x-anthropic-billing-header: cc_version=9.9.9.{nonce}; cch=fake{nonce};")},
        {"type": "text", "text": prompt, "cache_control": {"type": "ephemeral"}},
        {"type": "text", "text": "Fake long instructions block. ".repeat(20),
         "cache_control": {"type": "ephemeral", "ttl": "1h"}}
    ])
}

fn sig(id: &str) -> String {
    encode_reasoning_signature(&ReasoningReplay {
        id: id.to_string(),
        encrypted_content: format!("ENC-{id}"),
    })
    .unwrap()
}

fn user_text(text: &str) -> Value {
    json!({"role": "user", "content": [
        {"type": "text", "text": "<system-reminder>fake reminder</system-reminder>"},
        {"type": "text", "text": text}]})
}

fn tool_use(id: &str, name: &str, input: Value) -> Value {
    json!({"role": "assistant", "content": [
        {"type": "thinking", "thinking": "", "signature": sig(&format!("rs_{id}"))},
        {"type": "text", "text": "fake assistant text"},
        {"type": "tool_use", "id": id, "name": name, "input": input}]})
}

fn tool_result(id: &str, content: Value) -> Value {
    json!({"role": "user", "content": [{"type": "tool_result", "tool_use_id": id, "content": content}]})
}

fn main_history(stage: usize) -> Vec<Value> {
    let mut m = vec![
        json!({"role": "user", "content": [
            {"type": "text", "text": "<system-reminder>fake reminder</system-reminder>"},
            {"type": "text", "text": "fake task one"},
            {"type": "image", "source": {"type": "base64", "media_type": "image/png", "data": PNG}}]}),
        tool_use("tu_bash1", "Bash", json!({"command": "ls"})),
        tool_result("tu_bash1", json!("fake.txt")),
    ];
    if stage >= 1 {
        m.push(json!({"role": "assistant", "content": [{"type": "text", "text": "fake done"}]}));
        m.push(user_text("fake task two"));
    }
    if stage >= 2 {
        m.push(tool_use(
            "tu_search",
            "ToolSearch",
            json!({"query": "select:mcp__fake__lookup"}),
        ));
        m.push(json!({"role": "user", "content": [
            {"type": "tool_result", "tool_use_id": "tu_search",
             "content": [{"type": "tool_reference", "tool_name": "mcp__fake__lookup"}]},
            {"type": "text", "text": "Tool loaded."}]}));
    }
    if stage >= 3 {
        m.push(tool_use(
            "tu_lookup",
            "mcp__fake__lookup",
            json!({"q": "x"}),
        ));
        m.push(tool_result(
            "tu_lookup",
            json!([{"type": "text", "text": "fake lookup result"}]),
        ));
    }
    m
}

fn strip_cache_control(v: &mut Value) {
    match v {
        Value::Object(o) => {
            o.remove("cache_control");
            o.values_mut().for_each(strip_cache_control);
        }
        Value::Array(a) => a.iter_mut().for_each(strip_cache_control),
        _ => {}
    }
}

/// Claude Code moves the message cache_control marker to the newest block each turn.
fn request(system: Value, tools: Vec<Value>, mut messages: Vec<Value>) -> Value {
    messages.iter_mut().for_each(strip_cache_control);
    if let Some(block) = messages
        .last_mut()
        .and_then(|m| m["content"].as_array_mut())
        .and_then(|b| b.last_mut())
    {
        block["cache_control"] = json!({"type": "ephemeral"});
    }
    json!({"model": MODEL, "max_tokens": 32000, "stream": true, "system": system,
           "tools": tools, "messages": messages,
           "metadata": {"user_id": "fake-user-id-0001"}, "output_config": {"effort": "high"}})
}

fn main_request(stage: usize) -> Value {
    request(
        system(stage as u32 + 1, MAIN_PROMPT),
        main_tools(stage >= 2),
        main_history(stage),
    )
}

fn subagent_request(later: bool) -> Value {
    let mut m = vec![user_text("fake subagent prompt")];
    if later {
        m.push(tool_use("tu_sub1", "Read", json!({"file_path": "/fake"})));
        m.push(tool_result("tu_sub1", json!("1\tfake line")));
    }
    let tools = vec![
        serde_json::from_str(BASH_TOOL).unwrap(),
        tool("Read", json!({"file_path": {"type": "string"}})),
    ];
    request(
        json!([{"type": "text", "text": "x-anthropic-billing-header: cch=fake9;"},
               {"type": "text", "text": "You are a fake subagent.", "cache_control": {"type": "ephemeral"}}]),
        tools,
        m,
    )
}

fn compaction_request(stage: usize, prompt: &str) -> Value {
    let mut m = main_history(stage);
    m.push(json!({"role": "user", "content": [{"type": "text", "text": COMPACT_INSTRUCTION}]}));
    request(system(99, prompt), main_tools(stage >= 2), m)
}

fn post_compaction_request(loaded_before: bool) -> Value {
    let m = vec![
        user_text(&format!(
            "This session is being continued. Summary: {SUMMARY}"
        )),
        json!({"role": "assistant", "content": [{"type": "text", "text": "fake ack"}]}),
        user_text("fake task three"),
    ];
    request(system(100, MAIN_PROMPT), main_tools(loaded_before), m)
}

// ---------------------------------------------------------------------------
// Translation and structural comparison
// ---------------------------------------------------------------------------

fn translate_req(req: &MessagesRequest, lite: bool) -> ResponsesRequest {
    translate_request(
        req,
        TranslateOptions {
            prompt_cache_key: Some(CACHE_KEY.to_string()),
            service_tier: None,
            model: MODEL.to_string(),
            use_responses_lite: lite,
        },
    )
    .unwrap()
}

fn translate_raw(body: &Value, lite: bool) -> ResponsesRequest {
    translate_req(&serde_json::from_value(body.clone()).unwrap(), lite)
}

fn wire(r: &ResponsesRequest) -> Value {
    serde_json::to_value(r).unwrap()
}

fn translate(body: &Value, lite: bool) -> Value {
    wire(&translate_raw(body, lite))
}

fn items(w: &Value) -> Vec<String> {
    w["input"]
        .as_array()
        .unwrap()
        .iter()
        .map(Value::to_string)
        .collect()
}

/// First input item of `prev` not reproduced in `cur`; `None` iff prev is an item prefix.
fn first_diff(prev: &Value, cur: &Value) -> Option<usize> {
    let (p, c) = (items(prev), items(cur));
    p.iter()
        .zip(&c)
        .position(|(a, b)| a != b)
        .or((p.len() > c.len()).then_some(c.len()))
}

fn assert_strict_prefix(prev: &Value, cur: &Value, label: &str) {
    assert_eq!(first_diff(prev, cur), None, "{label}");
    assert!(items(cur).len() > items(prev).len(), "{label}");
}

fn changed_fields(a: &Value, b: &Value) -> BTreeSet<String> {
    let (a, b) = (a.as_object().unwrap(), b.as_object().unwrap());
    a.keys()
        .chain(b.keys())
        .filter(|k| a.get(*k) != b.get(*k))
        .cloned()
        .collect()
}

fn set(keys: &[&str]) -> BTreeSet<String> {
    keys.iter().map(|k| k.to_string()).collect()
}

fn tool_names(tools: &Value) -> Vec<String> {
    tools
        .as_array()
        .unwrap()
        .iter()
        .map(|t| t["name"].as_str().unwrap().to_string())
        .collect()
}

#[test]
fn full_lane_pairwise_prefix_facts() {
    let _env = Hermetic::new();
    let t = |b: Value| translate(&b, false);
    let n = t(main_request(0));
    let n1 = t(main_request(1));
    let sub = t(subagent_request(false));
    let load = t(main_request(2));
    let comp = t(compaction_request(2, SUMMARIZER_PROMPT));

    assert_strict_prefix(&n, &n1, "main N -> N+1");
    assert_eq!(changed_fields(&n, &n1), set(&["input"]));

    assert_eq!(first_diff(&n1, &sub), Some(0));
    assert_ne!(n1["instructions"], sub["instructions"]);
    assert_ne!(n1["tools"], sub["tools"]);

    assert_strict_prefix(&n1, &load, "tool load");
    assert_eq!(changed_fields(&n1, &load), set(&["input", "tools"]));

    assert_strict_prefix(&load, &comp, "compaction");
    // `reasoning` is the compaction effort cap, not prompt content.
    assert_eq!(
        changed_fields(&load, &comp),
        set(&["input", "instructions", "reasoning"])
    );
}

#[test]
fn lite_lane_pairwise_prefix_facts() {
    let _env = Hermetic::new();
    assert!(uses_responses_lite_with_full_lane(MODEL, false));
    let t = |b: Value| translate(&b, true);
    let n = t(main_request(0));
    let n1 = t(main_request(1));
    let sub = t(subagent_request(false));
    let sub_later = t(subagent_request(true));
    let load = t(main_request(2));
    let after_load = t(main_request(3));
    let comp = t(compaction_request(3, MAIN_PROMPT));

    assert!(n.get("instructions").is_none() && n.get("tools").is_none());
    assert_eq!(n["input"][0]["type"], "additional_tools");
    assert_eq!(n["input"][0]["role"], "developer");
    assert_eq!(n["input"][1]["type"], "message");
    assert_eq!(n["input"][1]["role"], "developer");
    assert_eq!(n["input"][2]["role"], "user");

    assert_strict_prefix(&n, &n1, "main N -> N+1");
    assert_eq!(changed_fields(&n, &n1), set(&["input"]));
    assert_strict_prefix(&sub, &sub_later, "subagent later turn");
    assert_strict_prefix(&n1, &load, "ToolSearch load");
    assert_strict_prefix(&load, &after_load, "after load");
    assert_strict_prefix(&after_load, &comp, "compaction request");

    assert_eq!(first_diff(&n1, &sub), Some(0));
}

#[test]
fn translation_is_deterministic_and_drops_anthropic_cache_fields() {
    let _env = Hermetic::new();
    let fixtures = [
        main_request(0),
        main_request(1),
        main_request(2),
        main_request(3),
        subagent_request(false),
        subagent_request(true),
        compaction_request(3, MAIN_PROMPT),
        compaction_request(2, SUMMARIZER_PROMPT),
        post_compaction_request(true),
    ];
    for lite in [false, true] {
        for (i, body) in fixtures.iter().enumerate() {
            let label = format!("lite={lite} fixture={i}");
            let a = serde_json::to_string(&translate(body, lite)).unwrap();
            assert_eq!(
                a,
                serde_json::to_string(&translate(body, lite)).unwrap(),
                "{label}"
            );
            for needle in [
                "cache_control",
                "billing-header",
                "prompt_cache_options",
                "prompt_cache_breakpoint",
                "fake-user-id",
            ] {
                assert!(!a.contains(needle), "{label}: {needle}");
            }
            let mut bare = body.clone();
            strip_cache_control(&mut bare);
            assert_eq!(
                a,
                serde_json::to_string(&translate(&bare, lite)).unwrap(),
                "{label}"
            );
        }

        // Client key order is exercised at the text level, independent of serde_json's Map.
        let mut templ = main_request(1);
        templ["tools"][0] = json!("__TOOL0__");
        let templ = serde_json::to_string(&templ).unwrap();
        let from_text = |tool: &str| {
            let req: MessagesRequest =
                serde_json::from_str(&templ.replace("\"__TOOL0__\"", tool)).unwrap();
            serde_json::to_string(&translate_req(&req, lite)).unwrap()
        };
        assert_eq!(
            from_text(BASH_TOOL),
            from_text(BASH_TOOL_PERMUTED),
            "lite={lite}"
        );
    }

    let full = translate(&main_request(1), false);
    let lite = translate(&main_request(1), true);
    assert_eq!(full["prompt_cache_key"], CACHE_KEY);
    assert_eq!(full["prompt_cache_key"], lite["prompt_cache_key"]);
}

#[test]
fn known_proxy_induced_prefix_perturbations() {
    let _env = Hermetic::new();

    for (loaded, session) in [
        (false, "fake-session-replay-ctl"),
        (true, "fake-session-replay-load"),
    ] {
        let pre = translate_raw(
            &compaction_request(if loaded { 3 } else { 1 }, MAIN_PROMPT),
            true,
        );
        let attempt = begin_compaction(session, MODEL);
        let native = vec![
            ResponsesInputItem::Message {
                role: "user".into(),
                content: vec![ResponsesContentPart::InputText {
                    text: "fake task one".into(),
                }],
            },
            ResponsesInputItem::Compaction {
                encrypted_content: "fake-opaque".into(),
            },
        ];
        assert!(store_compaction(session, attempt, native));
        let output = vec![ResponsesInputItem::Message {
            role: "assistant".into(),
            content: vec![ResponsesContentPart::OutputText {
                text: format!("<summary>{SUMMARY}</summary>"),
            }],
        }];
        assert!(activate_compaction(
            Some(session),
            Some(attempt),
            MODEL,
            &output
        ));
        let post = translate_raw(&post_compaction_request(loaded), true);
        let replay = apply_compaction_replay(Some(session), &post)
            .expect("replay applies")
            .request;
        let (pw, rw) = (wire(&pre), wire(&replay));

        assert_eq!(pw["input"][1], rw["input"][1], "loaded={loaded}");
        if loaded {
            // Pins current behavior; see gongx030/claude-code-proxy#13. Flip when fixed.
            assert_eq!(first_diff(&pw, &rw), Some(0));
            let mut expected = tool_names(&pw["input"][0]["tools"]);
            expected.push("mcp__fake__lookup".into());
            assert_eq!(tool_names(&rw["input"][0]["tools"]), expected);
        } else {
            assert_eq!(first_diff(&pw, &rw), Some(2));
        }
    }

    // Pins current behavior; see gongx030/claude-code-proxy#14. Flip when fixed.
    let call = "tu_read_big_offset_fake";
    let args = r#"{"file_path":"/fake","offset":5000000}"#;
    let _ = sanitize_read_args("Read", args, Some(call));
    let mut m = main_history(1);
    m.push(tool_use(call, "Read", json!({"file_path": "/fake"})));
    m.push(tool_result(call, json!("1\tfake line")));
    m.push(json!({"role": "assistant", "content": [{"type": "text", "text": "fake"}]}));
    m.push(user_text("fake next"));
    let body = request(system(1, MAIN_PROMPT), main_tools(false), m);

    let before = translate(&body, true);
    for i in 0..4_100 {
        let _ = sanitize_read_args("Read", args, Some(&format!("evict-{i}")));
    }
    let after = translate(&body, true);
    assert!(
        before["input"]
            .to_string()
            .contains("Proxy Read offset note:")
    );
    assert!(
        !after["input"]
            .to_string()
            .contains("Proxy Read offset note:")
    );
    let i = first_diff(&before, &after).expect("eviction changes the input");
    assert_eq!(before["input"][i]["type"], "function_call_output");
    assert!(
        i + 1 < items(&before).len(),
        "an earlier item, not the tail"
    );
}

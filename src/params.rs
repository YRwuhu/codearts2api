//! 请求参数归一化与上游错误映射。
//!
//! 这两块是 Python 版踩过坑之后补上的，Rust 版必须保持同样语义：
//! 1. 各客户端会用不同字段名表达输出长度（max_tokens / max_completion_tokens /
//!    max_output_tokens），上游只认 max_tokens 且超限时返回 HTTP 200 + 业务错误；
//! 2. 上游业务错误可能出现在 200 的 JSON 或 SSE 流里，必须翻译成可识别的形态。

use std::collections::{BTreeMap, VecDeque};

use serde_json::{json, Map, Value};

/// 上游各模型真正接受的 max_tokens 硬上限。
///
/// 这个值**不是**取自 `model_parameters.max_tokens`（那里恒为 65536/65535），
/// 而是上游参数校验器报出来的真实边界：超出时它会回
/// `RANGE_VALIDATOR: max_tokens -> max_tokens should be between 1 and 131072`。
/// 实测 10 个模型（deepseek-v4.1 / glm-5.3 / kimi-k3 / openpangu-2.0 ...）全部
/// 接受 131072、拒绝 131073，因此统一按这个上限裁剪。
///
/// 历史 bug：旧版直接信 `model_parameters.max_tokens`（65536），把客户端合法的
/// 128k 请求硬砍到 64k，于是"官方支持 384k 输出"和"反代只给 60k"的观感就出现了。
/// 注意 131072 是**输出**上限；模型的总上下文另见 `context_window`（1M）。
pub const UPSTREAM_MAX_OUTPUT_TOKENS: i64 = 131_072;

/// 把 max_* 系列字段统一成 max_tokens，并按模型上限裁剪。
///
/// `cap` 是调用方拿到的模型上限；它只会在**低于**上游硬上限时生效
/// （例如某个模型确实比别的模型小）。高于硬上限一律收敛到
/// [`UPSTREAM_MAX_OUTPUT_TOKENS`]，否则上游会回 400。
///
/// 返回 (是否发生裁剪, 原始值, 裁剪后值)。
pub fn normalize_params(body: &mut Map<String, Value>, cap: i64) -> Option<(i64, i64)> {
    let mut original: Option<i64> = None;
    for key in ["max_tokens", "max_completion_tokens", "max_output_tokens"] {
        if let Some(v) = body.get(key).and_then(|v| v.as_i64()) {
            if v > 0 {
                original = Some(original.map_or(v, |cur: i64| cur.max(v)));
            }
        }
    }

    let effective_cap = cap.clamp(1, UPSTREAM_MAX_OUTPUT_TOKENS);

    let mut capped_now = None;
    if let Some(orig) = original {
        let capped = orig.min(effective_cap);
        // 上游只认 max_tokens；其余两个字段会被当成未知参数拒绝。
        body.remove("max_completion_tokens");
        body.remove("max_output_tokens");
        body.insert("max_tokens".to_string(), Value::from(capped));
        if capped != orig {
            capped_now = Some((orig, capped));
        }
    }

    // reasoning 是 Responses API 字段，Chat Completions 上游不认。
    body.remove("reasoning");

    normalize_roles(body);
    repair_tool_pairs(body);

    capped_now
}

/// 上游参数校验的通用错误码：`InferHub.001001005.400 The request param is invalid`。
///
/// 这个码是**兜底**：上游参数校验器只回一句话，不说是哪个字段。实测能触发它的
/// 字段全部记在 [`repair_params`] 的注释里；这里只用于日志与归类。
pub const UPSTREAM_PARAM_INVALID: &str = "001001005";

/// 把上游会以 `InferHub.001001005.400` 拒绝的参数修成它能接受的形状。
///
/// 上游（`/api/v2/chat/completions`）对参数范围极严，而且**不告诉是哪个字段**：
/// 超出范围只回 `The request param is invalid, Please check it`。2026-11 实测
/// 出以下边界，逐个在这里收敛，避免整单失败：
///
/// | 字段 | 上游要求 | 触发值 |
/// |---|---|---|
/// | `max_tokens` | >= 1 | `0`、负数 |
/// | `n` | 只能 1 | `0`、`2`、负数 |
/// | `temperature` | `[0, 2]` | `2.1`、`3`、负数 |
/// | `top_p` | `(0, 1]` | `0`（另回 001001028）、`>1`、负数 |
/// | `presence_penalty` / `frequency_penalty` | `[-2, 2]` | `±3` |
/// | `stop` | 数组，最多 5 项 | 字符串、`""` |
/// | `seed` | `[0, 2^31-1]` | 负数、超出 |
/// | `top_k` | >= 1 | `0`、负数 |
/// | `top_logprobs` | `[0, 20]` | `-1`、`21` |
/// | `tool_choice` | 字符串 `none`/`auto`/`required` | 任何对象、`any`、其它字符串 |
///
/// `n` 无法"收敛"——客户端要求 2 条候选，改写成 1 条会静默改变语义——所以它
/// 以 `Err` 返回可读原因，由调用方转成 400。其余字段按"取最近的合法值"处理，
/// 并把改动记进返回值，随请求打进日志。
pub fn repair_params(body: &mut Map<String, Value>) -> Result<Vec<String>, String> {
    let mut notes = Vec::new();

    // n：上游只支持单条候选。留成 2 会让整个请求 400，不如直接告诉调用方。
    if let Some(n) = body.get("n").and_then(Value::as_i64) {
        if n == 1 {
            body.remove("n");
        } else {
            return Err(format!(
                "上游只支持单条候选（n=1），收到 n={n}；请去掉 n 或改成一"
            ));
        }
    }

    // max_tokens 已经过 normalize_params 裁剪；这里只兜底 <=0（未给出的情况
    // 用上游默认值即可，不要替它补 0）。
    if let Some(v) = body.get("max_tokens").and_then(Value::as_i64) {
        if v < 1 {
            body.remove("max_tokens");
            notes.push(format!("max_tokens {v} < 1，已移除"));
        }
    }

    // stop：上游要么是数组（最多 5 项），要么不出现。字符串会让校验器报通用错。
    match body.get("stop").cloned() {
        Some(Value::String(s)) if s.is_empty() => {
            body.remove("stop");
            notes.push("stop 为空字符串，已移除".to_string());
        }
        Some(Value::String(s)) => {
            body.insert("stop".to_string(), json!([s]));
            notes.push("stop 字符串已改写为单元素数组".to_string());
        }
        Some(Value::Array(items)) if items.len() > 5 => {
            body.insert("stop".to_string(), Value::Array(items[..5].to_vec()));
            notes.push("stop 超过 5 项，已截断".to_string());
        }
        _ => {}
    }

    // 采样参数的合法区间；超出取最近边界（Clippy 关心的是"总有值"）。
    clamp_number(body, "temperature", 0.0, 2.0, &mut notes);
    clamp_top_p(body, &mut notes);
    clamp_number(body, "presence_penalty", -2.0, 2.0, &mut notes);
    clamp_number(body, "frequency_penalty", -2.0, 2.0, &mut notes);

    // seed：只接受非负 32 位整数。它只影响可复现性，超出就丢掉。
    if let Some(seed) = body.get("seed").and_then(Value::as_i64) {
        if !(0..=i32::MAX as i64).contains(&seed) {
            body.remove("seed");
            notes.push(format!("seed {seed} 超出 [0, {}]，已移除", i32::MAX));
        }
    }

    // top_k / top_logprobs：非 OpenAI 标准字段，但上游认，且有下界/上界。
    if let Some(k) = body.get("top_k").and_then(Value::as_i64) {
        if k < 1 {
            body.remove("top_k");
            notes.push(format!("top_k {k} < 1，已移除"));
        }
    }
    if let Some(v) = body.get("top_logprobs").and_then(Value::as_i64) {
        if !(0..=20).contains(&v) {
            let fixed = v.clamp(0, 20);
            body.insert("top_logprobs".to_string(), json!(fixed));
            notes.push(format!("top_logprobs {v} 已收敛到 {fixed}"));
        }
    }

    // logit_bias：上游明确不支持（001001022.406），留着就是整单失败。
    if body.remove("logit_bias").is_some() {
        notes.push("上游不支持 logit_bias，已移除".to_string());
    }

    repair_tool_choice(body, &mut notes);
    repair_tools(body, &mut notes);
    repair_message_content(body, &mut notes);

    Ok(notes)
}

/// 把数值字段收敛到 `[lo, hi]`。非数值（字符串、null）原样留着，交给上游。
fn clamp_number(
    body: &mut Map<String, Value>,
    key: &str,
    lo: f64,
    hi: f64,
    notes: &mut Vec<String>,
) {
    let Some(v) = body.get(key).and_then(Value::as_f64) else {
        return;
    };
    if v < lo || v > hi {
        let fixed = v.clamp(lo, hi);
        body.insert(key.to_string(), json!(fixed));
        notes.push(format!("{key} {v} 已收敛到 {fixed}"));
    }
}

/// `top_p` 的下界是**开区间**：上游对 `0` 回 `001001028.406 top_p cannot be zero`。
///
/// 一律收敛到 `1.0`（不做核采样）：客户端写 `top_p: 0` 通常想表达"关掉 top_p"，
/// 而 `1.0` 正是这个语义；`>1` 的越界值也只能往下压到 1.0。
fn clamp_top_p(body: &mut Map<String, Value>, notes: &mut Vec<String>) {
    let Some(v) = body.get("top_p").and_then(Value::as_f64) else {
        return;
    };
    if !(v > 0.0 && v <= 1.0) {
        body.insert("top_p".to_string(), json!(1.0));
        notes.push(format!("top_p {v} 超出 (0, 1]，已收敛到 1"));
    }
}

/// `tool_choice` 只接受三个字符串。
///
/// OpenAI 的"指定某个函数"是对象形状（`{"type":"function","function":{...}}`），
/// 上游校验器不认，回的就是通用参数错。降级成 `auto` 是保语义最近的选择：
/// 工具仍然可用，只是不能强制指定。
fn repair_tool_choice(body: &mut Map<String, Value>, notes: &mut Vec<String>) {
    let tools_present = body
        .get("tools")
        .and_then(Value::as_array)
        .map(|t| !t.is_empty())
        .unwrap_or(false);

    let Some(choice) = body.get("tool_choice").cloned() else {
        return;
    };

    // 没有任何工具定义时，tool_choice 本身就是多余字段，去掉最安全。
    if !tools_present {
        body.remove("tool_choice");
        notes.push("无 tools 定义，已移除 tool_choice".to_string());
        return;
    }

    let replacement = match &choice {
        Value::String(s) if matches!(s.as_str(), "none" | "auto" | "required") => return,
        Value::String(other) => {
            notes.push(format!("tool_choice \"{other}\" 不受支持，已改为 auto"));
            json!("auto")
        }
        Value::Object(_) => {
            notes.push("上游不接受 tool_choice 对象，已改为 auto".to_string());
            json!("auto")
        }
        _ => {
            body.remove("tool_choice");
            notes.push("tool_choice 类型无效，已移除".to_string());
            return;
        }
    };
    body.insert("tool_choice".to_string(), replacement);
}

/// 工具定义的形状兜底：`type` 缺失、`parameters.type` 缺失都会被上游拒绝。
fn repair_tools(body: &mut Map<String, Value>, notes: &mut Vec<String>) {
    let Some(tools) = body.get_mut("tools").and_then(Value::as_array_mut) else {
        return;
    };
    tools.retain_mut(|tool| {
        let Some(obj) = tool.as_object_mut() else {
            return false;
        };
        if obj.get("type").and_then(Value::as_str).is_none() {
            obj.insert("type".to_string(), json!("function"));
        }
        let Some(function) = obj.get_mut("function").and_then(Value::as_object_mut) else {
            // 不是 function 工具（上游只认这一种），丢弃比整单失败好。
            return false;
        };
        // 名字必须非空；非法字符交给上游报它自己的 001001018（那条消息可读）。
        if function
            .get("name")
            .and_then(Value::as_str)
            .map(str::is_empty)
            .unwrap_or(true)
        {
            return false;
        }
        if let Some(params) = function
            .get_mut("parameters")
            .and_then(Value::as_object_mut)
        {
            params
                .entry("type".to_string())
                .or_insert_with(|| json!("object"));
        } else if function.get("parameters").is_none() {
            function.insert("parameters".to_string(), json!({"type": "object"}));
        }
        true
    });
    if tools.is_empty() {
        body.remove("tools");
        notes.push("工具定义全部无效，已移除 tools".to_string());
    }
}

/// 消息形状兜底：空 text part / 非字符串 content / tool_calls 缺字段。
///
/// 上游对 `{"type":"text","text":""}` 这类空块回 `ModelArts.81001 text content
/// is empty`；对空 content 数组回 `the message ... must not be empty`。两者都会
/// 让整段对话**永久**不可用——这正是长会话突然全线失败的一个来源。
fn repair_message_content(body: &mut Map<String, Value>, notes: &mut Vec<String>) {
    let Some(messages) = body.get_mut("messages").and_then(Value::as_array_mut) else {
        return;
    };
    let mut dropped_parts = 0usize;
    for msg in messages.iter_mut() {
        let Some(obj) = msg.as_object_mut() else {
            continue;
        };

        match obj.get_mut("content") {
            Some(Value::Array(parts)) => {
                let before = parts.len();
                parts.retain(|part| !is_empty_text_part(part));
                dropped_parts += before - parts.len();
                // 空数组同样被拒；空字符串 content 是上游接受的形状。
                if parts.is_empty() {
                    obj.insert("content".to_string(), json!(""));
                }
            }
            // object / number 之类上游回 001001021；转成 JSON 文本保住信息。
            Some(Value::Object(_)) | Some(Value::Number(_)) => {
                let text = obj
                    .get("content")
                    .cloned()
                    .unwrap_or(Value::Null)
                    .to_string();
                obj.insert("content".to_string(), json!(text));
                notes.push("content 不是字符串/数组，已转成文本".to_string());
            }
            _ => {}
        }

        // tool_calls：缺 type、arguments 不是字符串都会被上游拒。
        if let Some(calls) = obj.get_mut("tool_calls").and_then(Value::as_array_mut) {
            for call in calls.iter_mut() {
                let Some(call) = call.as_object_mut() else {
                    continue;
                };
                call.entry("type".to_string())
                    .or_insert_with(|| json!("function"));
                if let Some(function) = call.get_mut("function").and_then(Value::as_object_mut) {
                    if let Some(args) = function.get("arguments") {
                        if !args.is_string() && !args.is_null() {
                            let text = args.to_string();
                            function.insert("arguments".to_string(), json!(text));
                        }
                    }
                }
            }
        }
    }
    if dropped_parts > 0 {
        notes.push(format!("移除 {dropped_parts} 个空文本块"));
    }
}

fn is_empty_text_part(part: &Value) -> bool {
    part.get("type").and_then(Value::as_str) == Some("text")
        && part
            .get("text")
            .map(|t| t.as_str().map(str::is_empty).unwrap_or(true))
            .unwrap_or(true)
}

/// 只记录参数**形状**的指纹，用来给 `001001005` 定位真凶。
///
/// 上游对参数错误只回一句通用文案，不带字段名。真出问题时，日志里必须有
/// "这个请求带了哪些采样字段、各是什么类型/范围"才可能复现；但绝不能把
/// 提示词或 base64 写进日志，所以消息部分只统计条数与角色分布。
pub fn param_fingerprint(body: &Map<String, Value>) -> String {
    const SCALARS: [&str; 12] = [
        "temperature",
        "top_p",
        "presence_penalty",
        "frequency_penalty",
        "top_k",
        "top_logprobs",
        "max_tokens",
        "n",
        "seed",
        "reasoning_effort",
        "logprobs",
        "response_format",
    ];
    let mut fields: Vec<String> = Vec::new();
    for key in SCALARS {
        if let Some(v) = body.get(key) {
            fields.push(format!("{key}={}", short_value(v)));
        }
    }
    if let Some(stop) = body.get("stop") {
        fields.push(format!(
            "stop={}",
            match stop {
                Value::Array(a) => format!("array({})", a.len()),
                Value::String(_) => "string".to_string(),
                Value::Null => "null".to_string(),
                _ => "other".to_string(),
            }
        ));
    }
    if let Some(choice) = body.get("tool_choice") {
        fields.push(format!(
            "tool_choice={}",
            match choice {
                Value::String(s) => s.clone(),
                Value::Object(_) => "object".to_string(),
                Value::Null => "null".to_string(),
                _ => "other".to_string(),
            }
        ));
    }
    if let Some(tools) = body.get("tools").and_then(Value::as_array) {
        fields.push(format!("tools={}", tools.len()));
    }
    if let Some(messages) = body.get("messages").and_then(Value::as_array) {
        let mut counts: BTreeMap<&str, usize> = BTreeMap::new();
        for msg in messages {
            let role = msg.get("role").and_then(Value::as_str).unwrap_or("?");
            *counts.entry(role).or_default() += 1;
        }
        let roles = counts
            .into_iter()
            .map(|(role, n)| format!("{role}:{n}"))
            .collect::<Vec<_>>()
            .join(",");
        fields.push(format!("messages={}({roles})", messages.len()));
    }
    fields.join(" ")
}

fn short_value(v: &Value) -> String {
    match v {
        Value::String(s) if s.len() <= 24 => s.clone(),
        Value::String(s) => format!("\"<{} chars>\"", s.len()),
        other => other.to_string(),
    }
}

/// 修复 `tool_calls` / `tool` 消息配对，避免上游 400。
///
/// 上游对工具链的要求是：assistant 的每条 `tool_calls` 必须被**紧随其后、
/// 连续排列**的 `role: "tool"` 消息逐条应答。任何消息插进这段连续区间都会
/// 让后半段全部变成孤儿，报成：
///
/// ```text
/// Invalid request: an assistant message with 'tool_calls' must be followed by
/// tool messages responding to each 'tool_call_id'. The following tool_call_ids
/// did not have response messages: Read:9, Read:10, Read:11, Bash:12
/// ```
///
/// 实测来源是 `anthropic::to_chat_request`：某个 `tool_result` 带截图时，它会
/// 在工具链中间补一条 `role: "user"` 的图片消息（上游不接受 tool 带图），把
/// 后面几个 tool 结果全隔断了。适配层已改为把图片排到最后，这里再做一次
/// 兜底归一化——直通 `/v1/chat/completions` 的客户端也可能发出同样的形状。
///
/// 处理方式：
/// 1. 属于某条 assistant `tool_calls` 的 tool 消息提前到紧邻位置（保持原序）；
/// 2. 完全没有对应 `tool_calls` 的孤儿 tool 消息降级为 user 文本，否则上游会
///    因未知 `tool_call_id` 直接拒绝；
/// 3. 缺失结果的 `tool_call_id` 补一条占位 tool 消息，说明该调用被中断。
pub fn repair_tool_pairs(body: &mut Map<String, Value>) {
    let Some(messages) = body.get_mut("messages").and_then(|m| m.as_array_mut()) else {
        return;
    };

    let len = messages.len();
    let has_tools = messages
        .iter()
        .any(|m| is_tool_message(m) || has_tool_calls(m));
    if !has_tools {
        return;
    }

    // 第一遍：按 id 把 tool 消息排成队列，再给每条 tool_calls 分配具体下标。
    // 不在这里搬运消息，避免复制带 base64 的大 payload。
    let mut available: BTreeMap<String, VecDeque<usize>> = BTreeMap::new();
    for (idx, msg) in messages.iter().enumerate() {
        if is_tool_message(msg) {
            available
                .entry(tool_call_id(msg))
                .or_default()
                .push_back(idx);
        }
    }

    // tool_calls 各 id → 对应的 tool 消息下标（缺失为 None），以及该 tool 消息
    // 已被哪条 assistant 认领。
    let mut plan: BTreeMap<usize, Vec<(String, Option<usize>)>> = BTreeMap::new();
    let mut claimed = vec![false; len];
    for (idx, msg) in messages.iter().enumerate() {
        if !has_tool_calls(msg) {
            continue;
        }
        let slots = tool_call_ids(msg)
            .into_iter()
            .map(|id| {
                let origin = available.get_mut(&id).and_then(|queue| queue.pop_front());
                if let Some(origin) = origin {
                    claimed[origin] = true;
                }
                (id, origin)
            })
            .collect();
        plan.insert(idx, slots);
    }

    // 第二遍：按原顺序重建；被认领的 tool 消息在对应 assistant 处落地，
    // 未认领的孤儿降级为 user，缺失的结果补占位。
    let mut slots: Vec<Option<Value>> = std::mem::take(messages).into_iter().map(Some).collect();
    let mut rebuilt: Vec<Value> = Vec::with_capacity(len + 8);

    for idx in 0..len {
        if slots[idx].as_ref().is_some_and(is_tool_message) {
            if claimed[idx] {
                // 已被认领：若排在 assistant 之前，先留在原位，等 assistant
                // 处理时再取走；若排在后面，则已被取走（slots 为 None），
                // 上面的 is_some_and 已经不成立。两种情况都不在这里落地。
                continue;
            }
            if let Some(orphan) = slots[idx].take() {
                rebuilt.push(relabel_as_user(orphan));
            }
            continue;
        }

        if let Some(msg) = slots[idx].take() {
            rebuilt.push(msg);
        }
        let Some(calls) = plan.remove(&idx) else {
            continue;
        };
        for (id, origin) in calls {
            match origin.and_then(|origin| slots[origin].take()) {
                Some(result) => rebuilt.push(result),
                None => rebuilt.push(json!({
                    "role": "tool",
                    "tool_call_id": id,
                    "content": TOOL_RESULT_LOST,
                })),
            }
        }
    }

    *messages = rebuilt;
}

fn is_tool_message(msg: &Value) -> bool {
    msg.get("role").and_then(|r| r.as_str()) == Some("tool")
}

/// 上游对「工具调用后没等到结果」的兜底文案。
const TOOL_RESULT_LOST: &str = "该工具调用未返回结果（对话被中断或客户端未回传），请重新发起。";

fn has_tool_calls(msg: &Value) -> bool {
    msg.get("tool_calls")
        .and_then(|v| v.as_array())
        .map(|calls| !calls.is_empty())
        .unwrap_or(false)
}

fn tool_call_ids(msg: &Value) -> Vec<String> {
    msg.get("tool_calls")
        .and_then(|v| v.as_array())
        .map(|calls| {
            calls
                .iter()
                .filter_map(|c| c.get("id").and_then(|v| v.as_str()))
                .filter(|id| !id.is_empty())
                .map(str::to_string)
                .collect()
        })
        .unwrap_or_default()
}

fn tool_call_id(msg: &Value) -> String {
    msg.get("tool_call_id")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string()
}

/// 孤儿 tool 消息 → 用户消息。内容原样保留（文本或多模态数组），只是换掉
/// 承载它的角色；`tool_call_id` 一并丢掉，避免上游把它当未知字段。
fn relabel_as_user(msg: Value) -> Value {
    let content = msg.get("content").cloned().unwrap_or(json!(""));
    json!({"role": "user", "content": content})
}

/// 把上游不认的 message 角色映射成它认识的。
///
/// OpenAI 的 `developer` 角色（新版 SDK 用它替代 `system`）上游不认，
/// 直接转发会得到：
///
/// ```text
/// InferHub.001001044.400 Message role cannot empty
/// ```
///
/// 实测把它改成 `system` 后请求正常。
pub fn normalize_roles(body: &mut Map<String, Value>) {
    let Some(messages) = body.get_mut("messages").and_then(|m| m.as_array_mut()) else {
        return;
    };
    for msg in messages {
        let Some(obj) = msg.as_object_mut() else {
            continue;
        };
        if obj.get("role").and_then(|r| r.as_str()) == Some("developer") {
            obj.insert("role".to_string(), Value::from("system"));
        }
    }
}

/// 单张图片解码后的上限。
///
/// 与 CodeArts 网页/IDE 的图片约束同一量级；真正的上游限制仍会生效，
/// 这里只挡住明显异常的 payload，避免白烧一次额度。
pub const MAX_IMAGE_BYTES: usize = 20 * 1024 * 1024;

/// 原生识图的图片入参校验，返回 `true` 表示请求里确实带了图片。
///
/// 上游 `/api/v2/chat/completions` 只接受 base64 `data:` URL：远程
/// `http(s)://` 会被 `400 ModelArts.81001 unsupported image url` 拒绝。
/// 在发请求前给出可读错误，省掉一次注定失败的往返。
///
/// 顺带把「有没有图片」一起算出来，调用方不必再遍历一遍整个 body。
pub fn validate_image_input(body: &Map<String, Value>) -> Result<bool, String> {
    let Some(messages) = body.get("messages").and_then(Value::as_array) else {
        return Ok(false);
    };
    let mut found = false;
    for message in messages {
        let Some(parts) = message.get("content").and_then(Value::as_array) else {
            continue;
        };
        for part in parts {
            if !is_image_part(part) {
                continue;
            }
            found = true;
            let url = image_url_of(part).ok_or("图片缺少 URL")?;
            check_image_url(url)?;
        }
    }
    Ok(found)
}

/// 该 content part 是否是图片块（三种协议归一化后的形状）。
fn is_image_part(part: &Value) -> bool {
    matches!(
        part.get("type").and_then(Value::as_str),
        Some("image_url" | "input_image" | "image")
    )
}

/// 从各家协议的图片块里取出 URL：OpenAI / Responses / Anthropic 三种形状。
fn image_url_of(part: &Value) -> Option<&str> {
    part.get("image_url")
        .and_then(|value| match value {
            Value::String(url) => Some(url.as_str()),
            other => other.get("url").and_then(Value::as_str),
        })
        .or_else(|| part.get("url").and_then(Value::as_str))
        .or_else(|| {
            part.get("source")
                .and_then(|source| source.get("url").and_then(Value::as_str))
        })
}

fn check_image_url(url: &str) -> Result<(), String> {
    if url.starts_with("http://") || url.starts_with("https://") {
        return Err(
            "上游只接受 base64 data URL，不接受远程图片 URL；请先在客户端把图片内联为 \
             data:image/...;base64,..."
                .into(),
        );
    }
    let Some(data) = url.strip_prefix("data:") else {
        return Err("图片必须是 data:image/...;base64,... 格式".into());
    };
    let Some((meta, payload)) = data.split_once(',') else {
        return Err("图片 data URL 格式无效".into());
    };
    if !meta
        .split(';')
        .any(|value| value.eq_ignore_ascii_case("base64"))
    {
        return Err("图片 data URL 必须使用 base64 编码".into());
    }
    let mime = meta.split(';').next().unwrap_or("").to_ascii_lowercase();
    if !matches!(
        mime.as_str(),
        "image/png" | "image/jpeg" | "image/jpg" | "image/webp" | "image/gif"
    ) {
        return Err(format!("不支持的图片类型 {mime}；支持 PNG/JPEG/WebP/GIF"));
    }
    let decoded = base64_decoded_len_capped(payload, MAX_IMAGE_BYTES);
    if decoded == 0 {
        return Err("图片内容为空".into());
    }
    if decoded > MAX_IMAGE_BYTES {
        return Err(format!(
            "单张图片超过上限 {MAX_IMAGE_BYTES} 字节（约 {} MiB）",
            MAX_IMAGE_BYTES / 1024 / 1024
        ));
    }
    Ok(())
}

/// 估算 base64 解码后的字节数（不计空白与补位），超过 `limit` 就提前返回。
///
/// 一张 20 MiB 的图片对应约 27 MB 的 base64 字符串；全量扫完没有意义，
/// 达到上限即可判定超限并停止。
fn base64_decoded_len_capped(payload: &str, limit: usize) -> usize {
    // 解码后字节数 ≈ 有效字符数 * 3 / 4，反推需要扫描的字符数上限。
    let max_chars = limit.saturating_mul(4) / 3 + 4;
    let mut chars = 0usize;
    for byte in payload.bytes() {
        if byte.is_ascii_whitespace() || byte == b'=' {
            continue;
        }
        chars += 1;
        if chars > max_chars {
            return chars.saturating_mul(3) / 4;
        }
    }
    chars.saturating_mul(3) / 4
}

/// 估算 base64 解码后的字节数（不计空白与补位），用于大小校验。
#[cfg(test)]
fn base64_decoded_len(payload: &str) -> usize {
    base64_decoded_len_capped(payload, usize::MAX / 4)
}

/// 只记录多模态消息结构，绝不输出提示词、base64 或凭证。
pub fn request_shape(body: &Map<String, Value>) -> Value {
    let messages = body
        .get("messages")
        .and_then(|v| v.as_array())
        .map(|messages| {
            messages
                .iter()
                .map(|message| {
                    let role = message.get("role").cloned().unwrap_or(Value::Null);
                    let content = match message.get("content") {
                        Some(Value::Array(parts)) => Value::Array(
                            parts
                                .iter()
                                .map(|part| {
                                    let mut shape = serde_json::json!({
                                        "type": part.get("type").cloned().unwrap_or(Value::Null),
                                        "fields": part.as_object().map(|o| o.keys().collect::<Vec<_>>()),
                                    });
                                    if let Some(url) = part
                                        .get("image_url")
                                        .and_then(|v| v.get("url"))
                                        .and_then(|v| v.as_str())
                                    {
                                        let data = url.split_once(',').map(|(_, d)| d).unwrap_or("");
                                        shape["image"] = serde_json::json!({
                                            "scheme": url.split(':').next().unwrap_or(""),
                                            "mime": url.split(';').next().unwrap_or(""),
                                            "data_chars": data.len(),
                                            "url": if url.starts_with("data:") { Value::Null } else { Value::String(url.split('?').next().unwrap_or("").to_string()) },
                                        });
                                    }
                                    shape
                                })
                                .collect(),
                        ),
                        Some(Value::String(_)) => serde_json::json!("text"),
                        Some(other) => serde_json::json!({"type": other.as_str().map(|_| "text")}),
                        None => Value::Null,
                    };
                    serde_json::json!({"role": role, "content": content})
                })
                .collect::<Vec<_>>()
        })
        .unwrap_or_default();
    serde_json::json!({"messages": messages})
}

/// 上游是否把错误塞进了 HTTP 200。
pub fn is_business_error(value: &Value) -> bool {
    value.get("error_code").is_some() || value.get("error_msg").is_some()
}

/// 转成 OpenAI 错误对象。
pub fn error_payload(value: &Value) -> Value {
    serde_json::json!({
        "error": {
            "message": value.get("error_msg").and_then(|v| v.as_str())
                .unwrap_or("CodeArts upstream error"),
            "type": "upstream_error",
            "code": value.get("error_code").cloned().unwrap_or(Value::Null),
        }
    })
}

/// 结束原因归一化：CodeArts 的 "other" 语义上等于正常结束。
pub fn normalize_finish_reason(value: Option<&Value>) -> String {
    match value.and_then(|v| v.as_str()) {
        None | Some("other") => "stop".to_string(),
        Some(other) => other.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn body(v: Value) -> Map<String, Value> {
        v.as_object().cloned().unwrap()
    }

    #[test]
    fn caps_oversized_max_tokens() {
        let mut b = body(json!({"max_tokens": 384000}));
        let capped = normalize_params(&mut b, 65536);
        assert_eq!(capped, Some((384000, 65536)));
        assert_eq!(b["max_tokens"], json!(65536));
    }

    #[test]
    fn accepts_upstream_ceiling() {
        // 上游真实上限是 131072；旧版按 65536 裁剪导致 128k 请求被腰斩。
        let mut b = body(json!({"max_tokens": 131072}));
        assert_eq!(normalize_params(&mut b, 131072), None);
        assert_eq!(b["max_tokens"], json!(131072));
    }

    #[test]
    fn never_exceeds_upstream_ceiling() {
        // 即便配置/document 把 cap 设得比上游更大，也必须收敛到真实上限。
        let mut b = body(json!({"max_tokens": 384000}));
        let capped = normalize_params(&mut b, 999_999);
        assert_eq!(capped, Some((384000, UPSTREAM_MAX_OUTPUT_TOKENS)));
        assert_eq!(b["max_tokens"], json!(UPSTREAM_MAX_OUTPUT_TOKENS));
    }

    #[test]
    fn maps_max_output_tokens_to_max_tokens() {
        let mut b = body(json!({"max_output_tokens": 1000}));
        normalize_params(&mut b, 65536);
        assert_eq!(b["max_tokens"], json!(1000));
        assert!(b.get("max_output_tokens").is_none());
    }

    #[test]
    fn maps_developer_role_to_system() {
        let mut b = body(json!({"messages": [
            {"role": "developer", "content": "你是助手"},
            {"role": "user", "content": "hi"}
        ]}));
        normalize_params(&mut b, 1000);
        assert_eq!(b["messages"][0]["role"], "system");
        // 其他角色不受影响
        assert_eq!(b["messages"][1]["role"], "user");
    }

    #[test]
    fn drops_reasoning_field() {
        let mut b = body(json!({"reasoning": {"effort": "max"}}));
        normalize_params(&mut b, 65536);
        assert!(b.get("reasoning").is_none());
    }

    #[test]
    fn detects_images_only_in_structured_content() {
        // 字符串里的 image_url 只是文本，不算图片入参。
        let text = body(json!({"messages":[{"role":"user","content":"image_url"}]}));
        assert_eq!(validate_image_input(&text), Ok(false));

        let image = body(json!({"messages":[{"role":"user","content":[
            {"type":"image_url","image_url":{"url":"data:image/png;base64,AAA"}}
        ]}]}));
        assert_eq!(validate_image_input(&image), Ok(true));
    }

    #[test]
    fn reports_found_images_in_every_supported_shape() {
        let multi = body(json!({"messages": [
            {"role": "user", "content": "先看这张"},
            {"role": "user", "content": [
                {"type": "image_url", "image_url": {"url": "data:image/png;base64,QUJD"}},
                {"type": "input_image", "image_url": "data:image/jpeg;base64,QUJD"}
            ]}
        ]}));
        assert_eq!(validate_image_input(&multi), Ok(true));

        // 没有 messages 也没有图片：不算带图，也不报错。
        let empty = body(json!({"messages": []}));
        assert_eq!(validate_image_input(&empty), Ok(false));
        let bare = body(json!({}));
        assert_eq!(validate_image_input(&bare), Ok(false));
    }

    #[test]
    fn validates_supported_native_image_payloads() {
        let b = body(json!({"messages": [{"role": "user", "content": [
            {"type": "text", "text": "看"},
            {"type": "image_url", "image_url": {"url": "data:image/png;base64,QUJD"}},
            {"type": "input_image", "image_url": "data:image/jpeg;base64,QUJD"},
            {"type": "image", "url": "data:image/webp;base64,QUJD"},
            {"type": "image", "source": {"type": "url", "url": "data:image/gif;base64,QUJD"}}
        ]}]}));
        assert_eq!(validate_image_input(&b), Ok(true));
    }

    #[test]
    fn rejects_remote_and_malformed_image_urls() {
        for url in [
            "https://example.com/a.png",
            "http://example.com/a.png",
            "data:image/png,QUJD",
            "data:text/plain;base64,QUJD",
            "data:image/png;base64,",
            "not-a-url",
        ] {
            let b = body(json!({"messages": [{"role": "user", "content": [
                {"type": "image_url", "image_url": {"url": url}}
            ]}]}));
            assert!(validate_image_input(&b).is_err(), "应拒绝该图片 URL：{url}");
        }
    }

    #[test]
    fn estimates_base64_size_for_limits() {
        assert_eq!(base64_decoded_len(""), 0);
        assert_eq!(base64_decoded_len("QUJD"), 3);
        assert_eq!(base64_decoded_len("QUJD\nRA=="), 4);
        assert!(base64_decoded_len(&"A".repeat(MAX_IMAGE_BYTES / 3 * 4 + 8)) > MAX_IMAGE_BYTES);
    }

    #[test]
    fn stops_scanning_once_the_limit_is_exceeded() {
        // 远超上限的 payload 只需扫到阈值，没必要全量遍历 27 MB。
        let huge = "A".repeat(MAX_IMAGE_BYTES / 3 * 4 * 4);
        assert!(base64_decoded_len_capped(&huge, MAX_IMAGE_BYTES) > MAX_IMAGE_BYTES);
        // 刚好在上限内仍要精确。
        assert_eq!(base64_decoded_len_capped("QUJD", MAX_IMAGE_BYTES), 3);
    }

    #[test]
    fn preserves_multimodal_image_content() {
        let content = json!([
            {"type": "text", "text": "描述图片"},
            {"type": "image_url", "image_url": {
                "url": "data:image/png;base64,AAA", "detail": "high"
            }}
        ]);
        let mut b = body(json!({"messages": [{"role": "user", "content": content.clone()}]}));
        normalize_params(&mut b, 1000);
        assert_eq!(b["messages"][0]["content"], content);
    }

    /// 线上真实事故：一次调用 5 个工具，第一个结果带截图，适配层在工具链
    /// 中间插了一条 user 图片消息，后面 4 个 tool 结果全变成孤儿。
    #[test]
    fn reorders_tool_results_split_by_interleaved_message() {
        let mut b = body(json!({"messages": [
            {"role": "user", "content": "看这个仓库"},
            {"role": "assistant", "content": null, "tool_calls": [
                {"id": "Read_8", "type": "function", "function": {"name": "Read", "arguments": "{}"}},
                {"id": "Read_9", "type": "function", "function": {"name": "Read", "arguments": "{}"}},
                {"id": "Read_10", "type": "function", "function": {"name": "Read", "arguments": "{}"}},
                {"id": "Read_11", "type": "function", "function": {"name": "Read", "arguments": "{}"}},
                {"id": "Bash_12", "type": "function", "function": {"name": "Bash", "arguments": "{}"}}
            ]},
            {"role": "tool", "tool_call_id": "Read_8", "content": "shot"},
            {"role": "user", "content": [
                {"type": "image_url", "image_url": {"url": "data:image/png;base64,AAA"}}
            ]},
            {"role": "tool", "tool_call_id": "Read_9", "content": "a"},
            {"role": "tool", "tool_call_id": "Read_10", "content": "b"},
            {"role": "tool", "tool_call_id": "Read_11", "content": "c"},
            {"role": "tool", "tool_call_id": "Bash_12", "content": "d"}
        ]}));
        normalize_params(&mut b, 1000);

        let msgs = b["messages"].as_array().unwrap();
        // assistant 之后必须紧跟 5 条连续 tool 消息，且顺序与 tool_calls 一致
        assert_eq!(msgs[1]["role"], "assistant");
        let ids: Vec<&str> = msgs[2..7]
            .iter()
            .map(|m| m["tool_call_id"].as_str().unwrap())
            .collect();
        assert_eq!(ids, ["Read_8", "Read_9", "Read_10", "Read_11", "Bash_12"]);
        for msg in &msgs[2..7] {
            assert_eq!(msg["role"], "tool");
        }
        // 被打断的图片消息仍在，只是挪到了工具链之后
        let image_msg = msgs[7..]
            .iter()
            .find(|m| m["content"].is_array())
            .expect("图片消息不能丢");
        assert_eq!(image_msg["role"], "user");
        assert_eq!(msgs.len(), 8);
    }

    #[test]
    fn backfills_missing_tool_result_instead_of_dropping_call() {
        // 只回传了前两个结果：缺的补占位，不能删 tool_calls（否则又对不上号）
        let mut b = body(json!({"messages": [
            {"role": "user", "content": "hi"},
            {"role": "assistant", "content": null, "tool_calls": [
                {"id": "c1", "type": "function", "function": {"name": "f", "arguments": "{}"}},
                {"id": "c2", "type": "function", "function": {"name": "f", "arguments": "{}"}},
                {"id": "c3", "type": "function", "function": {"name": "f", "arguments": "{}"}}
            ]},
            {"role": "tool", "tool_call_id": "c2", "content": "ok"}
        ]}));
        normalize_params(&mut b, 1000);

        let msgs = b["messages"].as_array().unwrap();
        assert_eq!(msgs.len(), 5);
        let ids: Vec<&str> = msgs[2..5]
            .iter()
            .map(|m| m["tool_call_id"].as_str().unwrap())
            .collect();
        // 按 tool_calls 的声明顺序补齐，c2 的真实结果被提到它该在的位置
        assert_eq!(ids, ["c1", "c2", "c3"]);
        assert_eq!(msgs[3]["content"], "ok");
        assert!(msgs[2]["content"].as_str().unwrap().contains("未返回结果"));
    }

    #[test]
    fn demotes_orphan_tool_message_without_losing_content() {
        // 前面没有 assistant 认领这条 tool：上游会因未知 tool_call_id 拒绝，
        // 改成 user 保留内容。
        let mut b = body(json!({"messages": [
            {"role": "user", "content": "hi"},
            {"role": "tool", "tool_call_id": "ghost", "content": "残缺结果"}
        ]}));
        normalize_params(&mut b, 1000);

        let msgs = b["messages"].as_array().unwrap();
        assert_eq!(msgs.len(), 2);
        assert_eq!(msgs[1]["role"], "user");
        assert_eq!(msgs[1]["content"], "残缺结果");
        assert!(msgs[1].get("tool_call_id").is_none());
    }

    #[test]
    fn leaves_healthy_tool_chains_untouched() {
        let healthy = json!([
            {"role": "user", "content": "hi"},
            {"role": "assistant", "content": null, "tool_calls": [
                {"id": "c1", "type": "function", "function": {"name": "f", "arguments": "{}"}}
            ]},
            {"role": "tool", "tool_call_id": "c1", "content": "ok"},
            {"role": "user", "content": "谢谢"}
        ]);
        let mut b = body(json!({"messages": healthy.clone()}));
        normalize_params(&mut b, 1000);
        assert_eq!(
            Value::Array(b["messages"].as_array().unwrap().clone()),
            healthy
        );
    }

    /// 下面这组用例全部对应 2026-11 实测到的上游 `InferHub.001001005.400`
    /// 触发条件：上游只回一句 "The request param is invalid"，不说是哪个字段。

    #[test]
    fn repair_rejects_multiple_choices() {
        // n=2 无法收敛成 1（会静默改变语义），必须可读地拒绝。
        let mut b = body(json!({"messages": json!([]), "n": 2}));
        let err = repair_params(&mut b).unwrap_err();
        assert!(err.contains("n=2"), "错误信息应带上原值：{err}");
        assert!(err.contains("n=1"));

        let mut ok = body(json!({"n": 1}));
        assert!(repair_params(&mut ok).unwrap().is_empty());
        assert!(ok.get("n").is_none(), "n=1 是默认值，应移除");
    }

    #[test]
    fn repair_clamps_out_of_range_sampling_params() {
        let mut b = body(json!({
            "temperature": 3.0,
            "top_p": 0.0,
            "presence_penalty": -3.0,
            "frequency_penalty": 2.5,
            "seed": -1,
            "top_k": 0,
            "top_logprobs": 21
        }));
        let notes = repair_params(&mut b).unwrap();

        assert_eq!(b["temperature"], json!(2.0));
        assert_eq!(b["top_p"], json!(1.0), "top_p=0 上游回 001001028，必须收敛");
        assert_eq!(b["presence_penalty"], json!(-2.0));
        assert_eq!(b["frequency_penalty"], json!(2.0));
        assert!(b.get("seed").is_none());
        assert!(b.get("top_k").is_none());
        assert_eq!(b["top_logprobs"], json!(20));
        assert_eq!(notes.len(), 7, "{notes:?}");
    }

    #[test]
    fn repair_keeps_in_range_values_untouched() {
        // 边界值本身是合法的，不能被动到（实测 2.0 / -2.0 / 1.0 都能通过）。
        let mut b = body(json!({
            "temperature": 2.0,
            "top_p": 1.0,
            "presence_penalty": -2.0,
            "frequency_penalty": 2.0,
            "seed": 0,
            "top_k": 1,
            "top_logprobs": 0,
            "top_p_extra": 1
        }));
        assert!(repair_params(&mut b).unwrap().is_empty());
        assert_eq!(b["temperature"], json!(2.0));
        assert_eq!(b["top_p"], json!(1.0));
        assert_eq!(b["seed"], json!(0));
    }

    #[test]
    fn repair_normalizes_stop() {
        // 上游只认数组；字符串与空串都会触发通用参数错。
        let mut s = body(json!({"stop": "END"}));
        repair_params(&mut s).unwrap();
        assert_eq!(s["stop"], json!(["END"]));

        let mut empty = body(json!({"stop": ""}));
        repair_params(&mut empty).unwrap();
        assert!(empty.get("stop").is_none());

        // 上限 5 项（实测第 6 项起回 ModelArts.81001 stop array too long）。
        let mut long = body(json!({"stop": ["a", "b", "c", "d", "e", "f", "g"]}));
        repair_params(&mut long).unwrap();
        assert_eq!(long["stop"].as_array().unwrap().len(), 5);

        let mut five = body(json!({"stop": ["a", "b", "c", "d", "e"]}));
        assert!(repair_params(&mut five).unwrap().is_empty());
    }

    #[test]
    fn repair_drops_unsupported_max_tokens_and_logit_bias() {
        let mut b = body(json!({"max_tokens": 0, "logit_bias": {}}));
        let notes = repair_params(&mut b).unwrap();
        assert!(b.get("max_tokens").is_none(), "max_tokens=0 上游拒绝");
        assert!(b.get("logit_bias").is_none(), "上游不支持 logit_bias");
        assert_eq!(notes.len(), 2, "{notes:?}");
    }

    #[test]
    fn repair_tool_choice_only_accepts_three_strings() {
        let tools = json!([{"type": "function", "function": {"name": "f"}}]);

        // 对象形状（OpenAI 的"指定函数"）上游不认 → 降级 auto，工具仍可用。
        let mut obj = body(
            json!({"tools": tools, "tool_choice": {"type": "function", "function": {"name": "f"}}}),
        );
        let notes = repair_params(&mut obj).unwrap();
        assert_eq!(obj["tool_choice"], json!("auto"));
        assert!(notes.iter().any(|n| n.contains("tool_choice")));

        // 其它字符串（如 Anthropic 的 "any"）同样降级。
        let mut any = body(json!({"tools": tools, "tool_choice": "any"}));
        repair_params(&mut any).unwrap();
        assert_eq!(any["tool_choice"], json!("auto"));

        // 三个合法值保持原样。
        for ok in ["none", "auto", "required"] {
            let mut b = body(json!({"tools": tools, "tool_choice": ok}));
            assert!(repair_params(&mut b).unwrap().is_empty(), "{ok} 应被保留");
            assert_eq!(b["tool_choice"], json!(ok));
        }

        // 没有工具定义时这个字段是多余的，去掉更安全。
        let mut bare = body(json!({"tool_choice": {"type": "function", "name": "f"}}));
        repair_params(&mut bare).unwrap();
        assert!(bare.get("tool_choice").is_none());
    }

    #[test]
    fn repair_fixes_tool_definition_shapes() {
        let mut b = body(json!({"tools": [
            // 缺 type → 补 function
            {"function": {"name": "a"}},
            // 缺 parameters.type → 补 object
            {"type": "function", "function": {"name": "b", "parameters": {"properties": {}}}},
            // 缺 parameters → 补空 object schema
            {"type": "function", "function": {"name": "c"}},
            // 非 function 工具（上游只认这一种）→ 丢弃
            {"type": "web_search"},
            // 空名字 → 丢弃
            {"type": "function", "function": {"name": ""}}
        ]}));
        repair_params(&mut b).unwrap();
        let tools = b["tools"].as_array().unwrap();
        assert_eq!(tools.len(), 3, "{tools:?}");
        assert_eq!(tools[0]["type"], json!("function"));
        assert_eq!(tools[1]["function"]["parameters"]["type"], json!("object"));
        assert_eq!(
            tools[2]["function"]["parameters"],
            json!({"type": "object"})
        );
    }

    #[test]
    fn repair_drops_all_invalid_tools() {
        let mut b = body(json!({"tools": [{"type": "web_search"}]}));
        let notes = repair_params(&mut b).unwrap();
        assert!(b.get("tools").is_none());
        assert!(notes.iter().any(|n| n.contains("tools")));
    }

    #[test]
    fn repair_strips_empty_text_parts() {
        // {"type":"text","text":""} 会让上游整单回 ModelArts.81001
        // "text content is empty"——长会话一旦混进这种块就永久失败。
        let mut b = body(json!({"messages": [
            {"role": "user", "content": [{"type": "text", "text": ""}]},
            {"role": "assistant", "content": []},
            {"role": "user", "content": [
                {"type": "text", "text": "keep"},
                {"type": "text", "text": ""},
                {"type": "text"}
            ]}
        ]}));
        let notes = repair_params(&mut b).unwrap();
        let msgs = b["messages"].as_array().unwrap();
        assert_eq!(msgs[0]["content"], json!(""), "空数组应降级成空字符串");
        assert_eq!(msgs[1]["content"], json!(""));
        assert_eq!(
            msgs[2]["content"],
            json!([{"type": "text", "text": "keep"}])
        );
        assert!(notes.iter().any(|n| n.contains("空文本块")), "{notes:?}");
    }

    #[test]
    fn repair_keeps_non_empty_text_parts() {
        let content = json!([
            {"type": "text", "text": "hi"},
            {"type": "image_url", "image_url": {"url": "data:image/png;base64,AAAA"}}
        ]);
        let mut b = body(json!({"messages": [{"role": "user", "content": content.clone()}]}));
        repair_params(&mut b).unwrap();
        assert_eq!(b["messages"][0]["content"], content);
    }

    #[test]
    fn repair_converts_object_content_to_text() {
        // 对象/数字 content 上游回 001001021；转成文本至少保住语义。
        let mut b = body(json!({"messages": [{"role": "user", "content": {"a": 1}}]}));
        repair_params(&mut b).unwrap();
        assert_eq!(b["messages"][0]["content"], json!("{\"a\":1}"));
    }

    #[test]
    fn repair_fixes_tool_call_shapes() {
        let mut b = body(json!({"messages": [
            {"role": "user", "content": "hi"},
            {"role": "assistant", "content": null, "tool_calls": [
                // 缺 type 且 arguments 是对象：两种形状都会被上游拒绝
                {"id": "c1", "function": {"name": "f", "arguments": {"a": 1}}}
            ]},
            {"role": "tool", "tool_call_id": "c1", "content": "ok"}
        ]}));
        repair_params(&mut b).unwrap();
        let call = &b["messages"][1]["tool_calls"][0];
        assert_eq!(call["type"], json!("function"));
        assert_eq!(call["function"]["arguments"], json!("{\"a\":1}"));
    }

    #[test]
    fn param_fingerprint_never_leaks_prompt_or_base64() {
        // 日志里必须有参数形状以便事后复现，但绝不能含提示词与图片内容。
        let secret = "TOP-SECRET-PROMPT";
        let b64 = "iVBORw0KGgoAAAANSUhEUg".repeat(50);
        let fp = param_fingerprint(&body(json!({
            "model": "kimi-k3.1-pro",
            "messages": [
                {"role": "user", "content": secret},
                {"role": "user", "content": [
                    {"type": "text", "text": secret},
                    {"type": "image_url", "image_url": {"url": format!("data:image/png;base64,{b64}")}}
                ]}
            ],
            "temperature": 0.7,
            "tools": [{"type": "function", "function": {"name": "f"}}],
            "tool_choice": "auto"
        })));
        assert!(!fp.contains(secret), "指纹泄漏了提示词：{fp}");
        assert!(!fp.contains("iVBORw0KGgo"), "指纹泄漏了 base64：{fp}");
        assert!(fp.contains("messages=2"), "{fp}");
        assert!(fp.contains("user:2"), "{fp}");
        assert!(fp.contains("temperature=0.7"), "{fp}");
        assert!(fp.contains("tools=1"), "{fp}");
        assert!(fp.contains("tool_choice=auto"), "{fp}");
    }
}

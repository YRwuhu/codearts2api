//! 请求提示词清洗（WAF 误拦截修复）。
//!
//! 移植自 [honlnk/zcode-prompt-sanitizer](https://github.com/honlnk/zcode-prompt-sanitizer)
//! （MIT）：ZCode 会把当前仓库的 git 上下文以 `<system-reminder>` 的形式注入
//! 到消息里，其中这一句会被部分上游的内容安全网关当成提示词注入，整单拒绝：
//!
//! ```text
//! Main branch (you will usually use this for PRs): master
//! ```
//!
//! CodeArts 侧的表现是 `InferHub.ModelArts.81011.403`，返回一段固定拒答文案
//! （"抱歉，我无法提供关于您提问内容的建议…"），请求根本没到模型。
//!
//! 这里在请求发往上游前做**字面量替换**（不做正则，避免误伤），把这句话改写成
//! 语义等价、但不会被 WAF 命中的形式。只改请求，不碰响应；规则可配置。
//!
//! 注意注入位置：ZCode 3.x 把 gitStatus 放在 **user** 角色的 system-reminder
//! 里（不是 system），所以内置规则默认同时作用于 `system` 和 `user`。

use std::collections::BTreeMap;
use std::sync::OnceLock;

use serde_json::Value;

/// 一条字面量替换规则。
#[derive(Clone, Debug)]
struct Rule {
    id: String,
    enabled: bool,
    scopes: Vec<String>,
    find: String,
    replacement: String,
}

#[derive(Debug, Default)]
struct Ruleset {
    rules: Vec<Rule>,
}

/// 内置规则：覆盖 ZCode 已观测到的注入变体。
///
/// 只改 gitStatus 这句提示词本身——它描述的只是默认分支名，改写后模型拿到的
/// 信息不变（`Default git branch: master`），不会影响任务语义。
fn builtin_rules() -> Vec<Rule> {
    let scopes = vec!["system".to_string(), "user".to_string()];
    vec![
        Rule {
            id: "zcode-git-pr-hint".to_string(),
            enabled: true,
            scopes: scopes.clone(),
            find: "Main branch (you will usually use this for PRs):".to_string(),
            replacement: "Default git branch:".to_string(),
        },
        Rule {
            id: "zcode-git-pr-hint-lowercase".to_string(),
            enabled: true,
            scopes,
            find: "main branch (you will usually use this for PRs):".to_string(),
            replacement: "Default git branch:".to_string(),
        },
    ]
}

/// 进程级只加载一次：规则不热更新，改了配置重启即可。
fn ruleset() -> &'static Ruleset {
    static RULESET: OnceLock<Ruleset> = OnceLock::new();
    RULESET.get_or_init(load_ruleset)
}

fn sanitize_disabled() -> bool {
    matches!(
        std::env::var("CODEARTS_SANITIZE").ok().map(|v| v.trim().to_ascii_lowercase()),
        Some(v) if v == "0" || v == "off" || v == "false" || v == "no"
    )
}

fn load_ruleset() -> Ruleset {
    if sanitize_disabled() {
        return Ruleset::default();
    }

    // 显式指定时找不到文件属于配置错误，要报出来；默认路径不存在则静默用内置。
    let configured = std::env::var("CODEARTS_SANITIZE_RULES")
        .ok()
        .filter(|v| !v.trim().is_empty());
    let path = configured
        .clone()
        .unwrap_or_else(|| "sanitize.json".to_string());
    let path = std::path::Path::new(&path);

    if configured.is_some() || path.exists() {
        match std::fs::read_to_string(path) {
            Ok(text) => match parse_rules(&text) {
                Ok(rules) => {
                    println!("[sanitize] 载入 {} 条规则：{}", rules.len(), path.display());
                    return Ruleset { rules };
                }
                Err(e) => eprintln!(
                    "[sanitize] 规则文件无效，改用内置规则：{e}（{}）",
                    path.display()
                ),
            },
            Err(e) => eprintln!("[sanitize] 无法读取规则文件，改用内置规则：{e}"),
        }
    }

    Ruleset {
        rules: builtin_rules(),
    }
}

/// 解析规则文件：`{"rules": [...]}` 或直接一个数组。
///
/// 与上游一致：文件里给了 `rules` 就**整体替换**内置规则，不做合并——这样
/// 用户能显式关掉某条内置规则。
fn parse_rules(text: &str) -> Result<Vec<Rule>, String> {
    let value: Value = serde_json::from_str(text).map_err(|e| format!("JSON 解析失败：{e}"))?;
    let raw = value.get("rules").cloned().unwrap_or(value);
    let array = raw.as_array().ok_or("rules 必须是数组")?;

    let mut rules = Vec::with_capacity(array.len());
    let mut seen = std::collections::BTreeSet::new();
    for (i, item) in array.iter().enumerate() {
        let id = item
            .get("id")
            .and_then(Value::as_str)
            .filter(|s| !s.is_empty())
            .ok_or_else(|| format!("rules[{i}].id 必须是非空字符串"))?
            .to_string();
        if !seen.insert(id.clone()) {
            return Err(format!("重复的规则 id：{id}"));
        }
        let find = item
            .get("match")
            .and_then(Value::as_str)
            .filter(|s| !s.is_empty())
            .ok_or_else(|| format!("rules[{i}].match 必须是非空字符串"))?
            .to_string();
        let replacement = item
            .get("replacement")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string();
        let enabled = item.get("enabled").and_then(Value::as_bool).unwrap_or(true);
        let scopes = match item.get("scopes") {
            Some(Value::Array(list)) => {
                let mut out = Vec::with_capacity(list.len());
                for s in list {
                    let s = s.as_str().ok_or_else(|| {
                        format!("rules[{i}].scopes 只能是 system/user/assistant/tool")
                    })?;
                    if !matches!(s, "system" | "user" | "assistant" | "tool") {
                        return Err(format!("rules[{i}].scopes 含非法角色：{s}"));
                    }
                    out.push(s.to_string());
                }
                out
            }
            // 默认与内置规则一致：ZCode 的注入在 system-reminder（user）里。
            None | Some(Value::Null) => vec!["system".to_string(), "user".to_string()],
            _ => return Err(format!("rules[{i}].scopes 必须是数组")),
        };
        rules.push(Rule {
            id,
            enabled,
            scopes,
            find,
            replacement,
        });
    }
    Ok(rules)
}

/// 清洗请求体；有命中时返回可读的日志摘要，没命中返回 `None`。
///
/// 只处理 `messages[].content`（字符串，或 vision 风格的 content part 数组）。
/// 三种对外协议在进入 `chat::execute` 前都已归一化成这个形状，所以一处接入
/// 就覆盖 `/v1/chat/completions`、`/v1/responses`、`/v1/messages`。
pub fn apply(body: &mut Value) -> Option<String> {
    let ruleset = ruleset();
    apply_rules(body, &ruleset.rules)
}

fn apply_rules(body: &mut Value, rules: &[Rule]) -> Option<String> {
    let active: Vec<&Rule> = rules.iter().filter(|r| r.enabled).collect();
    if active.is_empty() {
        return None;
    }
    let messages = body.get_mut("messages")?.as_array_mut()?;

    let mut fired: BTreeMap<&str, usize> = BTreeMap::new();
    for message in messages.iter_mut() {
        let Some(obj) = message.as_object_mut() else {
            continue;
        };
        let role = obj
            .get("role")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string();
        let Some(content) = obj.get_mut("content") else {
            continue;
        };
        rewrite_content(content, &role, &active, &mut fired);
    }

    if fired.is_empty() {
        return None;
    }
    let summary = fired
        .iter()
        .map(|(id, n)| format!("{id}×{n}"))
        .collect::<Vec<_>>()
        .join(", ");
    Some(summary)
}

fn rewrite_content<'a>(
    content: &mut Value,
    role: &str,
    rules: &[&'a Rule],
    fired: &mut BTreeMap<&'a str, usize>,
) {
    match content {
        Value::String(text) => {
            rewrite_text(text, role, rules, fired);
        }
        Value::Array(parts) => {
            for part in parts {
                let Some(obj) = part.as_object_mut() else {
                    continue;
                };
                if obj.get("type").and_then(Value::as_str) != Some("text") {
                    continue;
                }
                if let Some(Value::String(text)) = obj.get_mut("text") {
                    rewrite_text(text, role, rules, fired);
                }
            }
        }
        _ => {}
    }
}

fn rewrite_text<'a>(
    text: &mut String,
    role: &str,
    rules: &[&'a Rule],
    fired: &mut BTreeMap<&'a str, usize>,
) {
    for rule in rules {
        if !rule.scopes.iter().any(|scope| scope == role) {
            continue;
        }
        if !text.contains(&rule.find) {
            continue;
        }
        let count = text.matches(&rule.find).count();
        *text = text.replace(&rule.find, &rule.replacement);
        *fired.entry(rule.id.as_str()).or_insert(0) += count;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn rule(find: &str, replacement: &str, scopes: &[&str]) -> Rule {
        Rule {
            id: "test".to_string(),
            enabled: true,
            scopes: scopes.iter().map(|s| s.to_string()).collect(),
            find: find.to_string(),
            replacement: replacement.to_string(),
        }
    }

    #[test]
    fn rewrites_zcode_git_hint_in_user_message() {
        // ZCode 3.x 把 gitStatus 注入到 user 的 system-reminder 里
        let mut body = json!({"messages": [
            {"role": "user", "content": "Context:\nMain branch (you will usually use this for PRs): master\nGit user: me"},
            {"role": "assistant", "content": "Main branch (you will usually use this for PRs): should stay"}
        ]});
        let report = apply_rules(&mut body, &builtin_rules()).unwrap();
        assert!(report.contains("zcode-git-pr-hint"));
        assert_eq!(
            body["messages"][0]["content"],
            "Context:\nDefault git branch: master\nGit user: me"
        );
        // assistant 不在内置规则的作用域里，保持原样
        assert_eq!(
            body["messages"][1]["content"],
            "Main branch (you will usually use this for PRs): should stay"
        );
    }

    #[test]
    fn rewrites_system_message_too() {
        let mut body = json!({"messages": [
            {"role": "system", "content": "main branch (you will usually use this for PRs): main"}
        ]});
        assert!(apply_rules(&mut body, &builtin_rules()).is_some());
        assert_eq!(body["messages"][0]["content"], "Default git branch: main");
    }

    #[test]
    fn handles_vision_content_parts() {
        let mut body = json!({"messages": [
            {"role": "user", "content": [
                {"type": "text", "text": "Main branch (you will usually use this for PRs): dev"},
                {"type": "image_url", "image_url": {"url": "data:image/png;base64,AAA"}}
            ]}
        ]});
        assert!(apply_rules(&mut body, &builtin_rules()).is_some());
        assert_eq!(
            body["messages"][0]["content"][0]["text"],
            "Default git branch: dev"
        );
        // 图片 part 原样保留
        assert_eq!(body["messages"][0]["content"][1]["type"], "image_url");
    }

    #[test]
    fn replaces_all_occurrences() {
        let mut body = json!({"messages": [
            {"role": "system", "content": "AAA and AAA again AAA"}
        ]});
        let rules = vec![rule("AAA", "B", &["system"])];
        apply_rules(&mut body, &rules);
        assert_eq!(body["messages"][0]["content"], "B and B again B");
    }

    #[test]
    fn empty_replacement_deletes() {
        let mut body = json!({"messages": [
            {"role": "system", "content": "token: SECRET-1234"}
        ]});
        let rules = vec![rule("SECRET-", "", &["system"])];
        apply_rules(&mut body, &rules);
        assert_eq!(body["messages"][0]["content"], "token: 1234");
    }

    #[test]
    fn skips_disabled_rules() {
        let mut body = json!({"messages": [{"role": "system", "content": "Main branch (you will usually use this for PRs): x"}]});
        let before = body.clone();
        let mut disabled = rule(
            "Main branch (you will usually use this for PRs):",
            "Default git branch:",
            &["system"],
        );
        disabled.enabled = false;
        assert!(apply_rules(&mut body, &[disabled]).is_none());
        assert_eq!(body, before);
    }

    #[test]
    fn respects_scopes() {
        let mut body = json!({"messages": [
            {"role": "tool", "content": "Main branch (you will usually use this for PRs): x"}
        ]});
        // 默认不含 tool：工具输出里的原文不能被改写
        assert!(apply_rules(&mut body, &builtin_rules()).is_none());
    }

    #[test]
    fn match_is_literal_not_regex() {
        let mut body = json!({"messages": [{"role": "system", "content": "use .* literally"}]});
        let rules = vec![rule(".*", "STAR", &["system"])];
        apply_rules(&mut body, &rules);
        assert_eq!(body["messages"][0]["content"], "use STAR literally");
    }

    #[test]
    fn leaves_body_untouched_when_nothing_matches() {
        let mut body = json!({"messages": [
            {"role": "system", "content": "You are a helpful assistant."},
            {"role": "user", "content": "hi"},
            {"role": "tool", "tool_call_id": "c1", "content": "ok"}
        ]});
        let before = body.clone();
        assert!(apply_rules(&mut body, &builtin_rules()).is_none());
        assert_eq!(body, before);
    }

    #[test]
    fn body_without_messages_is_ignored() {
        let mut body = json!({"input": "Main branch (you will usually use this for PRs): master"});
        let before = body.clone();
        assert!(apply_rules(&mut body, &builtin_rules()).is_none());
        assert_eq!(body, before);
    }

    #[test]
    fn parses_custom_rule_file() {
        let text = r#"{
            "rules": [
                {"id": "custom", "match": "foo", "replacement": "bar", "scopes": ["user"]},
                {"id": "off", "match": "x", "enabled": false}
            ]
        }"#;
        let rules = parse_rules(text).unwrap();
        assert_eq!(rules.len(), 2);
        assert_eq!(rules[0].scopes, ["user"]);
        assert!(!rules[1].enabled);
        // 未写 scopes 时的默认值
        assert_eq!(rules[1].scopes, ["system", "user"]);
        assert!(parse_rules("[]").unwrap().is_empty());
    }

    #[test]
    fn rejects_invalid_rule_files() {
        for bad in [
            r#"{"rules": "nope"}"#,
            r#"{"rules": [{"id": "", "match": "x"}]}"#,
            r#"{"rules": [{"id": "a", "match": ""}]}"#,
            r#"{"rules": [{"id": "a", "match": "x"}, {"id": "a", "match": "y"}]}"#,
            r#"{"rules": [{"id": "a", "match": "x", "scopes": ["nope"]}]}"#,
        ] {
            assert!(parse_rules(bad).is_err(), "应拒绝：{bad}");
        }
    }
}

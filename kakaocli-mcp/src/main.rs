//! kakaocli-mcp: a minimal stdio MCP server exposing kakaocli to AI agents.
//!
//! Transport: newline-delimited JSON-RPC 2.0 over stdin/stdout (MCP stdio).
//! Each tool call shells out to the `kakaocli` binary with `--json` and returns
//! its output, so all platform logic (Windows DEK scan, macOS AX, schema
//! dispatch) is reused verbatim — one source of truth.
//!
//! READ-ONLY BUILD: the `send` tool has been removed. This server can only
//! read chats; it has no way to send a message.
//!
//! KEY CACHE: kakaocli caches decryption keys in plain text at
//! %TEMP%\kakaocli\dek_cache.json and reuses it even after new chats are
//! opened. This build deletes that file before each call (so newly opened
//! chats are picked up) and after each call (so keys are not left on disk).
//!
//! EXCLUDED CHATS: results from the chat IDs in `EXCLUDED_CHAT_IDS` (plus any
//! IDs listed in `excluded_chats.txt` next to this exe, one per line) are
//! removed before anything is returned to the AI. If a result can't be parsed
//! for filtering, nothing is returned (fail closed).

use serde_json::{json, Value};
use std::collections::HashSet;
use std::io::{BufRead, Write};

const PROTOCOL_VERSION: &str = "2024-11-05";

/// Chats whose content must never reach the AI.
/// 18374231911657749 = "오스틴 텍사스 한인 최대 챗방" (open community chat).
const EXCLUDED_CHAT_IDS: &[i64] = &[18374231911657749];

/// Optional extra exclusions: `excluded_chats.txt` beside this exe.
const EXCLUDE_FILE: &str = "excluded_chats.txt";

fn main() {
    let stdin = std::io::stdin();
    let mut out = std::io::stdout();
    let reader = stdin.lock();

    for line in reader.lines() {
        let line = match line {
            Ok(l) => l,
            Err(_) => break,
        };
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        let req: Value = match serde_json::from_str(trimmed) {
            Ok(v) => v,
            Err(e) => {
                eprintln!("[kakaocli-mcp] parse error: {e}");
                continue;
            }
        };

        let id = req.get("id").cloned();
        let method = req.get("method").and_then(|m| m.as_str()).unwrap_or("");

        // Notifications (no id) get no response.
        if id.is_none() {
            continue;
        }
        let id = id.unwrap();

        let response = match method {
            "initialize" => reply(
                id,
                json!({
                    "protocolVersion": PROTOCOL_VERSION,
                    "capabilities": { "tools": {} },
                    "serverInfo": { "name": "kakaocli-mcp", "version": env!("CARGO_PKG_VERSION") }
                }),
            ),
            "ping" => reply(id, json!({})),
            "tools/list" => reply(id, json!({ "tools": tool_specs() })),
            "tools/call" => handle_tool_call(id, req.get("params")),
            other => error(id, -32601, &format!("method not found: {other}")),
        };

        let _ = writeln!(out, "{response}");
        let _ = out.flush();
    }
}

fn reply(id: Value, result: Value) -> Value {
    json!({ "jsonrpc": "2.0", "id": id, "result": result })
}

fn error(id: Value, code: i64, message: &str) -> Value {
    json!({ "jsonrpc": "2.0", "id": id, "error": { "code": code, "message": message } })
}

/// Build a text-content tool result. `is_error` marks a failed call.
fn tool_result(id: Value, text: String, is_error: bool) -> Value {
    reply(
        id,
        json!({
            "content": [ { "type": "text", "text": text } ],
            "isError": is_error
        }),
    )
}

fn tool_specs() -> Value {
    json!([
        {
            "name": "chats",
            "description": "채팅방 목록을 조회한다 (JSON).",
            "inputSchema": {
                "type": "object",
                "properties": { "limit": { "type": "integer", "description": "최대 개수 (기본 50)" } }
            }
        },
        {
            "name": "messages",
            "description": "한 채팅방의 메시지를 조회한다. chat(부분일치) 또는 me(나와의 채팅) 중 하나를 지정.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "chat": { "type": "string", "description": "채팅방 이름 (부분일치)" },
                    "me": { "type": "boolean", "description": "나와의 채팅" },
                    "since": { "type": "string", "description": "시간 범위 (예: 1h, 7d, 2026-09-01)" },
                    "limit": { "type": "integer", "description": "최대 개수 (기본 50)" }
                }
            }
        },
        {
            "name": "find",
            "description": "메시지/방/친구를 검색한다.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "keyword": { "type": "string" },
                    "rooms": { "type": "boolean", "description": "방 이름 검색" },
                    "friends": { "type": "boolean", "description": "친구 이름 검색" },
                    "all": { "type": "boolean", "description": "통합 검색" }
                },
                "required": ["keyword"]
            }
        },
        {
            "name": "stats",
            "description": "메시지 통계(방별·날짜별·시간대별)를 집계한다. Windows는 앱에서 연 방만 집계될 수 있다.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "since": { "type": "string", "description": "시간 범위 (예: 7d, 30d)" },
                    "top": { "type": "integer", "description": "방별 상위 N (기본 20)" }
                }
            }
        },
        {
            "name": "check",
            "description": "앱/권한/DB 상태를 점검한다.",
            "inputSchema": { "type": "object", "properties": {} }
        }
    ])
}

fn handle_tool_call(id: Value, params: Option<&Value>) -> Value {
    let params = match params {
        Some(p) => p,
        None => return error(id, -32602, "missing params"),
    };
    let name = params.get("name").and_then(|v| v.as_str()).unwrap_or("");
    let args = params
        .get("arguments")
        .cloned()
        .unwrap_or_else(|| json!({}));

    let argv = match build_argv(name, &args) {
        Ok(a) => a,
        Err(e) => return tool_result(id, e, true),
    };

    let out = match run_kakaocli(&argv) {
        Ok(out) => out,
        Err(e) => return tool_result(id, format!("kakaocli 실행 실패: {e}"), true),
    };
    match filter_output(name, &args, &out, &excluded_ids()) {
        Ok(text) => tool_result(id, text, false),
        Err(e) => tool_result(id, e, true),
    }
}

/// Built-in exclusions plus any IDs in `excluded_chats.txt` beside the exe.
fn excluded_ids() -> HashSet<i64> {
    let mut set: HashSet<i64> = EXCLUDED_CHAT_IDS.iter().copied().collect();
    if let Ok(cur) = std::env::current_exe() {
        if let Some(dir) = cur.parent() {
            if let Ok(text) = std::fs::read_to_string(dir.join(EXCLUDE_FILE)) {
                set.extend(parse_exclude_file(&text));
            }
        }
    }
    set
}

/// One chat ID per line; anything after `#` is a comment.
fn parse_exclude_file(text: &str) -> Vec<i64> {
    text.lines()
        .filter_map(|l| l.split('#').next())
        .filter_map(|l| l.trim().parse::<i64>().ok())
        .collect()
}

const FILTER_FAIL: &str =
    "결과를 확인할 수 없어 표시하지 않습니다 (제외 채팅방 보호). kakaocli를 직접 실행해 확인하세요.";
const EXCLUDED_ONLY: &str =
    "이 채팅방은 제외 목록에 있어 읽을 수 없습니다. 다른 방을 찾으시면 더 정확한 이름으로 다시 시도하세요.";

/// Remove anything belonging to an excluded chat. Fails closed: if the output
/// isn't the JSON shape we expect, return an error instead of the raw text.
fn filter_output(
    name: &str,
    args: &Value,
    out: &str,
    excluded: &HashSet<i64>,
) -> Result<String, String> {
    if name == "check" {
        return Ok(out.to_string()); // status only, no chat content
    }
    let mut v: Value = serde_json::from_str(out).map_err(|_| FILTER_FAIL.to_string())?;
    let flag = |k: &str| args.get(k).and_then(|x| x.as_bool()).unwrap_or(false);
    let is_excluded = |x: Option<&Value>| {
        x.and_then(|n| n.as_i64())
            .map(|n| excluded.contains(&n))
            .unwrap_or(false)
    };

    match name {
        // Chat lists: the chat's own id is "id".
        "chats" => retain_array(&mut v, |e| !is_excluded(e.get("id")))?,
        "find" if flag("rooms") => retain_array(&mut v, |e| !is_excluded(e.get("id")))?,
        // Friend search returns contacts only (no chat content).
        "find" if flag("friends") => {}
        // Messages: each carries "chat_id".
        "find" => retain_array(&mut v, |e| !is_excluded(e.get("chat_id")))?,
        "messages" => {
            let before = v.as_array().map(|a| a.len()).ok_or(FILTER_FAIL)?;
            retain_array(&mut v, |e| !is_excluded(e.get("chat_id")))?;
            let after = v.as_array().map(|a| a.len()).unwrap_or(0);
            if before > 0 && after == 0 {
                return Err(EXCLUDED_ONLY.to_string());
            }
        }
        "stats" => {
            let top = args
                .get("top")
                .and_then(|x| x.as_u64())
                .unwrap_or(20)
                .min(1000) as usize;
            let rooms = v
                .get_mut("by_room")
                .and_then(|r| r.as_array_mut())
                .ok_or(FILTER_FAIL)?;
            let mut removed = 0i64;
            rooms.retain(|r| {
                if is_excluded(r.get("chat_id")) {
                    removed += r.get("count").and_then(|c| c.as_i64()).unwrap_or(0);
                    false
                } else {
                    true
                }
            });
            rooms.truncate(top);
            if removed > 0 {
                if let Some(t) = v.get("total").and_then(|t| t.as_i64()) {
                    v["total"] = json!(t - removed);
                }
                v["excluded_note"] = json!(
                    "제외 채팅방은 by_room과 total에서 뺐습니다. by_date/by_hour 건수에는 포함돼 있을 수 있습니다."
                );
            }
        }
        _ => return Err(FILTER_FAIL.to_string()),
    }
    serde_json::to_string_pretty(&v).map_err(|_| FILTER_FAIL.to_string())
}

fn retain_array(v: &mut Value, keep: impl Fn(&Value) -> bool) -> Result<(), String> {
    let arr = v.as_array_mut().ok_or(FILTER_FAIL)?;
    arr.retain(|e| keep(e));
    Ok(())
}

/// Translate a tool call into kakaocli argv. Read tools always append `--json`.
fn build_argv(name: &str, args: &Value) -> Result<Vec<String>, String> {
    let s = |k: &str| args.get(k).and_then(|v| v.as_str()).map(|v| v.to_string());
    let b = |k: &str| args.get(k).and_then(|v| v.as_bool()).unwrap_or(false);
    let i = |k: &str| args.get(k).and_then(|v| v.as_i64());

    let mut a: Vec<String> = Vec::new();
    match name {
        "chats" => {
            a.push("chats".into());
            if let Some(n) = i("limit") {
                a.push("--limit".into());
                a.push(n.to_string());
            }
            a.push("--json".into());
        }
        "messages" => {
            a.push("msg".into());
            if b("me") {
                a.push("--me".into());
            } else if let Some(c) = s("chat") {
                a.push("--chat".into());
                a.push(c);
            } else {
                // chat/me가 없으면 CLI가 대화형 방 선택으로 빠져 헤드리스에서 멈춘다.
                return Err("chat 또는 me 중 하나가 필요합니다".into());
            }
            if let Some(since) = s("since") {
                a.push("--since".into());
                a.push(since);
            }
            if let Some(n) = i("limit") {
                a.push("--limit".into());
                a.push(n.to_string());
            }
            a.push("--json".into());
        }
        "find" => {
            let kw = s("keyword").ok_or("keyword가 필요합니다")?;
            a.push("find".into());
            a.push(kw);
            if b("rooms") {
                a.push("--rooms".into());
            }
            if b("friends") {
                a.push("--friends".into());
            }
            if b("all") {
                a.push("--all".into());
            }
            a.push("--json".into());
        }
        "stats" => {
            a.push("stats".into());
            if let Some(since) = s("since") {
                a.push("--since".into());
                a.push(since);
            }
            // Ask for every room so excluded chats can be removed exactly;
            // the requested `top` is applied after filtering.
            a.push("--top".into());
            a.push("9999".into());
            a.push("--json".into());
        }
        "check" => {
            a.push("check".into());
            a.push("--json".into());
        }
        other => return Err(format!("알 수 없는 도구: {other}")),
    }
    Ok(a)
}

/// Locate the kakaocli binary next to this one; fall back to PATH.
fn kakaocli_path() -> std::path::PathBuf {
    let exe_name = if cfg!(windows) {
        "kakaocli.exe"
    } else {
        "kakaocli"
    };
    if let Ok(cur) = std::env::current_exe() {
        if let Some(dir) = cur.parent() {
            let sibling = dir.join(exe_name);
            if sibling.exists() {
                return sibling;
            }
        }
    }
    std::path::PathBuf::from("kakaocli")
}

/// Run kakaocli. `Ok` on zero exit (success), `Err` on non-zero so the caller
/// marks the tool result `isError: true` instead of reporting failure as success.
fn run_kakaocli(argv: &[String]) -> Result<String, String> {
    clear_dek_cache(); // fresh scan: picks up chats opened since the last call
    let result = std::process::Command::new(kakaocli_path())
        .args(argv)
        .output();
    clear_dek_cache(); // don't leave plaintext keys in %TEMP%
    let output = result.map_err(|e| e.to_string())?;
    let stdout = String::from_utf8_lossy(&output.stdout).into_owned();
    if output.status.success() {
        return Ok(stdout);
    }
    // Non-zero exit → surface stdout+stderr as an error.
    let stderr = String::from_utf8_lossy(&output.stderr);
    let mut msg = stdout;
    if !stderr.trim().is_empty() {
        if !msg.is_empty() {
            msg.push('\n');
        }
        msg.push_str(&stderr);
    }
    if msg.trim().is_empty() {
        msg = format!("kakaocli 비정상 종료: {}", output.status);
    }
    Err(msg)
}

/// kakaocli's DEK cache file (same path kakaocli uses: temp_dir/kakaocli).
fn dek_cache_path() -> std::path::PathBuf {
    std::env::temp_dir().join("kakaocli").join("dek_cache.json")
}

/// Delete the plaintext DEK cache. Missing file is fine.
fn clear_dek_cache() {
    let _ = std::fs::remove_file(dek_cache_path());
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn clear_dek_cache_removes_file() {
        let p = dek_cache_path();
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        std::fs::write(&p, "{}").unwrap();
        clear_dek_cache();
        assert!(!p.exists());
        clear_dek_cache(); // missing file must not panic
    }

    #[test]
    fn messages_me_takes_precedence_over_chat() {
        let a = build_argv("messages", &json!({ "me": true, "chat": "x", "limit": 10 })).unwrap();
        assert_eq!(a, vec!["msg", "--me", "--limit", "10", "--json"]);
    }

    #[test]
    fn read_tools_append_json() {
        let a = build_argv("chats", &json!({ "limit": 5 })).unwrap();
        assert_eq!(a, vec!["chats", "--limit", "5", "--json"]);
    }

    #[test]
    fn find_requires_keyword() {
        assert!(build_argv("find", &json!({})).is_err());
    }

    #[test]
    fn messages_requires_chat_or_me() {
        // 둘 다 없으면 CLI 대화형 선택을 피하려 에러여야 한다.
        assert!(build_argv("messages", &json!({ "limit": 10 })).is_err());
        assert!(build_argv("messages", &json!({ "chat": "x" })).is_ok());
        assert!(build_argv("messages", &json!({ "me": true })).is_ok());
    }

    const COMMUNITY: i64 = 18374231911657749;

    fn ex() -> HashSet<i64> {
        EXCLUDED_CHAT_IDS.iter().copied().collect()
    }

    #[test]
    fn chats_hides_community_chat() {
        let out = json!([{ "id": 1, "display_name": "client" },
                         { "id": COMMUNITY, "display_name": "오스틴 텍사스 한인 최대 챗방" }])
        .to_string();
        let r = filter_output("chats", &json!({}), &out, &ex()).unwrap();
        assert!(r.contains("client"));
        assert!(!r.contains("최대 챗방") && !r.contains(&COMMUNITY.to_string()));
    }

    #[test]
    fn messages_from_community_chat_are_refused() {
        let out = json!([{ "id": 9, "chat_id": COMMUNITY, "text": "secret" }]).to_string();
        let r = filter_output("messages", &json!({ "chat": "오스틴" }), &out, &ex());
        assert!(r.is_err());
        assert!(!r.unwrap_err().contains("secret"));
    }

    #[test]
    fn messages_from_client_chat_pass() {
        let out = json!([{ "id": 9, "chat_id": 5, "text": "hello" }]).to_string();
        let r = filter_output("messages", &json!({ "chat": "x" }), &out, &ex()).unwrap();
        assert!(r.contains("hello"));
    }

    #[test]
    fn find_drops_community_messages_and_rooms() {
        let msgs = json!([{ "id": 1, "chat_id": COMMUNITY, "text": "a" },
                          { "id": 2, "chat_id": 5, "text": "b" }])
        .to_string();
        let r = filter_output("find", &json!({ "keyword": "x" }), &msgs, &ex()).unwrap();
        assert!(r.contains("\"b\"") && !r.contains("\"a\""));
        let rooms = json!([{ "id": COMMUNITY, "display_name": "최대 챗방" }]).to_string();
        let r = filter_output(
            "find",
            &json!({ "keyword": "챗방", "rooms": true }),
            &rooms,
            &ex(),
        )
        .unwrap();
        assert!(!r.contains("최대 챗방"));
    }

    #[test]
    fn stats_removes_community_and_applies_top() {
        let out = json!({
            "total": 1000,
            "by_room": [
                { "chat_id": COMMUNITY, "name": "최대 챗방", "count": 900 },
                { "chat_id": 1, "name": "a", "count": 60 },
                { "chat_id": 2, "name": "b", "count": 40 }
            ],
            "by_date": [], "by_hour": [],
            "coverage": { "rooms_counted": 3, "rooms_total": 3, "partial": false }
        })
        .to_string();
        let r = filter_output("stats", &json!({ "top": 1 }), &out, &ex()).unwrap();
        let v: Value = serde_json::from_str(&r).unwrap();
        assert_eq!(v["total"], 100);
        assert_eq!(v["by_room"].as_array().unwrap().len(), 1);
        assert_eq!(v["by_room"][0]["name"], "a");
        assert!(!r.contains("최대 챗방"));
    }

    #[test]
    fn unparseable_output_fails_closed() {
        let r = filter_output(
            "messages",
            &json!({ "chat": "x" }),
            "not json secret",
            &ex(),
        );
        assert!(r.is_err());
        assert!(!r.unwrap_err().contains("secret"));
    }

    #[test]
    fn exclude_file_parsing() {
        let ids = parse_exclude_file("# comment\n123\n 456  # note\nabc\n\n");
        assert_eq!(ids, vec![123, 456]);
    }

    #[test]
    fn stats_always_requests_all_rooms() {
        let a = build_argv("stats", &json!({ "since": "7d", "top": 5 })).unwrap();
        assert_eq!(a, vec!["stats", "--since", "7d", "--top", "9999", "--json"]);
    }

    #[test]
    fn send_tool_is_removed() {
        assert!(build_argv(
            "send",
            &json!({ "chat": "x", "message": "hi", "confirm": true })
        )
        .is_err());
        assert!(!tool_specs().to_string().contains("\"send\""));
    }
}

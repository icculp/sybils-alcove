//! Agent launch attempts, independent of child transcript retention.
//!
//! A child rollout proves that an agent existed. It does not prove that every
//! launch attempt was retained: a wrapper can use an ephemeral session root, a
//! native spawn can fail before creating a child, and a subagent can launch
//! another agent beneath the viewer's normal display depth. The caller's tool
//! record is therefore the source of truth for the attempt.

use std::collections::HashMap;
use std::sync::OnceLock;

use regex::Regex;
use serde::Serialize;
use serde_json::Value;

use crate::spool::ToolCall;

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct RawLaunch {
    pub call_id: String,
    pub at: String,
    pub launcher: String,
    pub kind: String,
    pub task: String,
    pub status: String,
}

#[derive(Clone, Debug, Serialize, PartialEq, Eq)]
pub struct LaunchAttempt {
    pub id: String,
    pub at: String,
    pub harness: String,
    pub session_id: String,
    pub caller_id: String,
    pub caller_parent_id: String,
    pub caller_role: String,
    pub launcher: String,
    pub kind: String,
    pub task: String,
    pub status: String,
    pub child_id: String,
    pub transcript: bool,
}

fn literals_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        Regex::new(r#"(?s)\"(?:\\.|[^\"\\])*\"|'(?:\\.|[^'\\])*'|`(?:\\.|[^`\\])*`"#)
            .expect("literal regex")
    })
}

fn const_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        Regex::new(r"\b(?:const|let|var)\s+([A-Za-z_$][A-Za-z0-9_$]*)\s*=\s*(__ALCOVE_STR_[0-9]+__)")
            .expect("const regex")
    })
}

fn command_property_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        Regex::new(
            r"\b(?:cmd|command)\s*:\s*([A-Za-z_$][A-Za-z0-9_$]*|__ALCOVE_STR_[0-9]+__)",
        )
        .expect("command property regex")
    })
}

fn unquote(raw: &str) -> String {
    if raw.starts_with('"') {
        return serde_json::from_str::<String>(raw).unwrap_or_default();
    }
    if raw.len() < 2 {
        return String::new();
    }
    raw[1..raw.len() - 1]
        .replace("\\'", "'")
        .replace("\\`", "`")
        .replace("\\\"", "\"")
        .replace("\\n", "\n")
}

fn string_values(value: &Value, out: &mut Vec<String>) {
    match value {
        Value::String(s) => out.push(s.clone()),
        Value::Array(items) => {
            for item in items {
                string_values(item, out);
            }
        }
        Value::Object(map) => {
            for (key, value) in map {
                if matches!(key.as_str(), "cmd" | "command") {
                    string_values(value, out);
                }
            }
        }
        _ => {}
    }
}

fn command_candidates(arguments: &Value) -> Vec<String> {
    let mut out = Vec::new();
    match arguments {
        Value::String(raw) => {
            if let Ok(parsed) = serde_json::from_str::<Value>(raw) {
                string_values(&parsed, &mut out);
            } else {
                // `functions.exec` is JavaScript. Mask literals first, then read
                // only values actually supplied to `cmd:`/`command:`. Looking at
                // every literal would interpret command examples inside an
                // apply_patch payload as processes that really ran.
                let mut masked = String::with_capacity(raw.len());
                let mut literals: HashMap<String, String> = HashMap::new();
                let mut end = 0;
                for (i, found) in literals_re().find_iter(raw).enumerate() {
                    masked.push_str(&raw[end..found.start()]);
                    let marker = format!("__ALCOVE_STR_{i}__");
                    masked.push_str(&marker);
                    literals.insert(marker, unquote(found.as_str()));
                    end = found.end();
                }
                masked.push_str(&raw[end..]);

                let mut variables: HashMap<String, String> = HashMap::new();
                for captures in const_re().captures_iter(&masked) {
                    if let Some(value) = literals.get(&captures[2]) {
                        variables.insert(captures[1].to_string(), value.clone());
                    }
                }
                for captures in command_property_re().captures_iter(&masked) {
                    let token = &captures[1];
                    if let Some(value) = literals.get(token).or_else(|| variables.get(token)) {
                        out.push(value.clone());
                    }
                }
            }
        }
        other => string_values(other, &mut out),
    }
    out
}

fn clean_token(token: &str) -> &str {
    token.trim_matches(|c: char| matches!(c, '\'' | '"' | '`' | '(' | ')' | '{' | '}'))
}

fn known_program(token: &str, rest: &[String]) -> Option<String> {
    let base = clean_token(token).rsplit('/').next().unwrap_or("");
    let lower = base.to_ascii_lowercase();
    if lower.contains("codex-spark-triage") {
        return Some("spark".into());
    }
    if lower == "codex" || lower.starts_with("codex-") || lower.ends_with("-codex") {
        let sub = rest.first().map(|s| clean_token(s)).unwrap_or("");
        if matches!(sub, "exec" | "review") || lower != "codex" {
            return Some("codex".into());
        }
    }
    if lower == "claude" || lower.contains("claude-agent") {
        if !rest.iter().any(|s| matches!(clean_token(s), "--version" | "agents" | "mcp")) {
            return Some("claude".into());
        }
    }
    if lower == "hermes" || lower.contains("hermes-agent") {
        return Some("hermes".into());
    }
    if matches!(lower.as_str(), "opencode" | "aider") || lower.contains("luna-agent") {
        return Some(lower);
    }
    None
}

fn shell_segments(command: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut current = String::new();
    let mut quote = None;
    let mut escaped = false;
    for ch in command.chars() {
        if escaped {
            current.push(ch);
            escaped = false;
            continue;
        }
        if ch == '\\' {
            current.push(ch);
            escaped = true;
            continue;
        }
        if let Some(open) = quote {
            current.push(ch);
            if ch == open {
                quote = None;
            }
            continue;
        }
        if matches!(ch, '\'' | '"' | '`') {
            quote = Some(ch);
            current.push(ch);
        } else if matches!(ch, '\n' | ';' | '|' | '&') {
            if !current.trim().is_empty() {
                out.push(std::mem::take(&mut current));
            }
        } else {
            current.push(ch);
        }
    }
    if !current.trim().is_empty() {
        out.push(current);
    }
    out
}

fn shell_tokens(segment: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut current = String::new();
    let mut quote = None;
    let mut escaped = false;
    for ch in segment.chars() {
        if escaped {
            current.push(ch);
            escaped = false;
        } else if ch == '\\' {
            escaped = true;
        } else if let Some(open) = quote {
            if ch == open {
                quote = None;
            } else {
                current.push(ch);
            }
        } else if matches!(ch, '\'' | '"' | '`') {
            quote = Some(ch);
        } else if ch.is_whitespace() {
            if !current.is_empty() {
                out.push(std::mem::take(&mut current));
            }
        } else {
            current.push(ch);
        }
    }
    if escaped {
        current.push('\\');
    }
    if !current.is_empty() {
        out.push(current);
    }
    out
}

fn command_launchers_inner(command: &str, depth: usize) -> Vec<String> {
    if depth > 4 {
        return Vec::new();
    }
    let mut out = Vec::new();
    for segment in shell_segments(command) {
        let tokens = shell_tokens(&segment);
        for (i, token) in tokens.iter().enumerate() {
            let clean = clean_token(token);
            if clean.is_empty()
                || clean == "!"
                || clean.contains('=')
                || clean.starts_with('-')
                || matches!(clean, "env" | "sudo" | "command" | "nohup" | "setsid" | "timeout")
                || clean.chars().all(|c| c.is_ascii_digit() || matches!(c, '.' | 's' | 'm' | 'h'))
            {
                continue;
            }

            let base = clean.rsplit('/').next().unwrap_or("");
            let rest = &tokens[i + 1..];
            if matches!(base, "bash" | "sh" | "zsh" | "dash") {
                if let Some(at) = rest
                    .iter()
                    .position(|part| part.starts_with('-') && part.contains('c'))
                {
                    if let Some(nested) = rest.get(at + 1) {
                        for launcher in command_launchers_inner(nested, depth + 1) {
                            if !out.contains(&launcher) {
                                out.push(launcher);
                            }
                        }
                    }
                }
            } else if let Some(launcher) = known_program(clean, rest) {
                if !out.contains(&launcher) {
                    out.push(launcher);
                }
            }
            // The first non-wrapper executable decides what this shell segment
            // does. Arguments to rg/sed/etc. may mention agent binaries.
            break;
        }
    }
    out
}

/// Return one entry per agent executable observed in a shell command.
pub fn command_launchers(command: &str) -> Vec<String> {
    command_launchers_inner(command, 0)
}

fn clipped_task(launcher: &str) -> String {
    format!("{launcher} launch command observed; child details unavailable until reconciled")
}

/// Fold one Codex transcript event into wrapped launch attempts.
pub fn observe_codex(event: &Value, out: &mut Vec<RawLaunch>) {
    if event.get("type").and_then(Value::as_str) != Some("response_item") {
        return;
    }
    let Some(payload) = event.get("payload").filter(|p| p.is_object()) else {
        return;
    };
    let ptype = payload.get("type").and_then(Value::as_str).unwrap_or("");
    let call_id = payload.get("call_id").and_then(Value::as_str).unwrap_or("");
    if matches!(ptype, "function_call_output" | "custom_tool_call_output") {
        let prefix = format!("{call_id}:");
        for launch in out
            .iter_mut()
            .filter(|l| l.call_id == call_id || l.call_id.starts_with(&prefix))
        {
            launch.status = "returned".into();
        }
        return;
    }
    if !matches!(ptype, "function_call" | "custom_tool_call" | "local_shell_call") {
        return;
    }
    let name = payload.get("name").and_then(Value::as_str).unwrap_or("");
    let arguments = payload
        .get("arguments")
        .or_else(|| payload.get("input"))
        .or_else(|| payload.get("action"))
        .unwrap_or(&Value::Null);
    let at = event.get("timestamp").and_then(Value::as_str).unwrap_or("");
    if ptype == "function_call" && name.contains("spawn_agent") {
        let parsed = arguments
            .as_str()
            .and_then(|raw| serde_json::from_str::<Value>(raw).ok());
        let input = parsed.as_ref().unwrap_or(arguments);
        let launcher = input
            .get("agent_type")
            .or_else(|| input.get("subagent_type"))
            .and_then(Value::as_str)
            .filter(|value| !value.is_empty())
            .unwrap_or("native agent");
        let task = input
            .get("message")
            .or_else(|| input.get("prompt"))
            .or_else(|| input.get("description"))
            .and_then(Value::as_str)
            .map(|value| value.chars().take(240).collect())
            .unwrap_or_else(|| clipped_task(launcher));
        out.push(RawLaunch {
            call_id: if call_id.is_empty() { at.into() } else { call_id.into() },
            at: at.into(),
            launcher: launcher.into(),
            kind: "native call".into(),
            task,
            status: "attempted".into(),
        });
        return;
    }
    let executes_shell = match ptype {
        "function_call" => matches!(name, "exec_command" | "Bash" | "bash" | "shell"),
        "custom_tool_call" => name == "exec",
        "local_shell_call" => true,
        _ => false,
    };
    if !executes_shell {
        return;
    }
    let mut launchers = Vec::new();
    for command in command_candidates(arguments) {
        for launcher in command_launchers(&command) {
            if !launchers.contains(&launcher) {
                launchers.push(launcher);
            }
        }
    }
    for (i, launcher) in launchers.into_iter().enumerate() {
        let id = if call_id.is_empty() {
            format!("{at}:{i}")
        } else {
            call_id.to_string()
        };
        if out.iter().any(|l| l.call_id == id && l.launcher == launcher) {
            continue;
        }
        out.push(RawLaunch {
            call_id: id,
            at: at.into(),
            task: clipped_task(&launcher),
            launcher,
            kind: "wrapped".into(),
            status: "attempted".into(),
        });
    }
}

/// Fold one Claude transcript event into launch attempts. Native Agent/Task
/// calls and shell-wrapped CLIs use the same ledger; neither needs a child file.
pub fn observe_claude(event: &Value, out: &mut Vec<RawLaunch>) {
    let ts = event.get("timestamp").and_then(Value::as_str).unwrap_or("");
    let Some(blocks) = event
        .get("message")
        .and_then(|m| m.get("content"))
        .and_then(Value::as_array)
    else {
        return;
    };
    for block in blocks {
        let btype = block.get("type").and_then(Value::as_str).unwrap_or("");
        if btype == "tool_result" {
            let id = block.get("tool_use_id").and_then(Value::as_str).unwrap_or("");
            for launch in out.iter_mut().filter(|launch| launch.call_id == id) {
                launch.status = if block.get("is_error").and_then(Value::as_bool) == Some(true) {
                    "failed".into()
                } else {
                    "returned".into()
                };
            }
            continue;
        }
        if btype != "tool_use" {
            continue;
        }
        let name = block.get("name").and_then(Value::as_str).unwrap_or("");
        let id = block.get("id").and_then(Value::as_str).unwrap_or("");
        let input = block.get("input").unwrap_or(&Value::Null);
        let mut launchers = if matches!(name, "Agent" | "Task") {
            vec![input
                .get("subagent_type")
                .or_else(|| input.get("agent_type"))
                .and_then(Value::as_str)
                .filter(|v| !v.is_empty())
                .unwrap_or("claude")
                .to_string()]
        } else {
            let mut commands = Vec::new();
            string_values(input, &mut commands);
            commands.into_iter().flat_map(|c| command_launchers(&c)).collect()
        };
        launchers.sort();
        launchers.dedup();
        for launcher in launchers {
            if out.iter().any(|launch| launch.call_id == id && launch.launcher == launcher) {
                continue;
            }
            let task = input
                .get("description")
                .and_then(Value::as_str)
                .map(|s| s.chars().take(240).collect())
                .unwrap_or_else(|| clipped_task(&launcher));
            out.push(RawLaunch {
                call_id: if id.is_empty() { format!("{ts}:{}", out.len()) } else { id.into() },
                at: ts.into(),
                launcher,
                kind: if matches!(name, "Agent" | "Task") {
                    "native call".into()
                } else {
                    "wrapped".into()
                },
                task,
                status: "attempted".into(),
            });
        }
    }
}

pub fn scope_raw(
    raw: &[RawLaunch],
    harness: &str,
    session_id: &str,
    caller_id: &str,
    caller_parent_id: &str,
    caller_role: &str,
) -> Vec<LaunchAttempt> {
    raw.iter()
        .map(|launch| LaunchAttempt {
            id: format!(
                "{harness}:{session_id}:{caller_id}:{}:{}",
                launch.call_id, launch.launcher
            ),
            at: launch.at.clone(),
            harness: harness.into(),
            session_id: session_id.into(),
            caller_id: caller_id.into(),
            caller_parent_id: caller_parent_id.into(),
            caller_role: caller_role.into(),
            launcher: launch.launcher.clone(),
            kind: if launch.kind.is_empty() { "wrapped".into() } else { launch.kind.clone() },
            task: launch.task.clone(),
            status: launch.status.clone(),
            child_id: String::new(),
            transcript: false,
        })
        .collect()
}

pub fn native_child(
    harness: &str,
    session_id: &str,
    caller_id: &str,
    caller_parent_id: &str,
    caller_role: &str,
    child_id: &str,
    child_role: &str,
    status: &str,
    task: &str,
    transcript: bool,
) -> LaunchAttempt {
    LaunchAttempt {
        id: format!("{harness}:{session_id}:{caller_id}:child:{child_id}"),
        at: String::new(),
        harness: harness.into(),
        session_id: session_id.into(),
        caller_id: caller_id.into(),
        caller_parent_id: caller_parent_id.into(),
        caller_role: caller_role.into(),
        launcher: if child_role.is_empty() { "native agent".into() } else { child_role.into() },
        kind: "native child".into(),
        task: task.into(),
        status: status.into(),
        child_id: child_id.into(),
        transcript,
    }
}

pub fn from_spool(calls: &[ToolCall]) -> Vec<LaunchAttempt> {
    let mut posts: HashMap<&str, Option<bool>> = HashMap::new();
    for call in calls.iter().filter(|c| c.event == "post") {
        if let Some(id) = call.tool_use_id.as_deref() {
            posts.insert(id, call.ok);
        }
    }
    let mut out = Vec::new();
    for call in calls.iter().filter(|c| c.event == "pre") {
        let launchers = if !call.agent_launchers.is_empty() {
            call.agent_launchers.clone()
        } else {
            call.arg.as_deref().map(command_launchers).unwrap_or_default()
        };
        for launcher in launchers {
            let call_id = call.tool_use_id.clone().unwrap_or_else(|| call.id());
            let caller = call.agent_id.as_deref().unwrap_or(&call.session_id);
            let parent = call.agent_id.as_ref().map(|_| call.session_id.as_str()).unwrap_or("");
            let status = match posts.get(call_id.as_str()) {
                Some(Some(true)) => "returned",
                Some(Some(false)) => "failed",
                Some(None) => "returned",
                None => "attempted",
            };
            out.push(LaunchAttempt {
                id: format!("{}:{}:{}:{call_id}:{launcher}", call.harness, call.session_id, caller),
                at: call.ts.clone(),
                harness: call.harness.clone(),
                session_id: call.session_id.clone(),
                caller_id: caller.into(),
                caller_parent_id: parent.into(),
                caller_role: call.agent_type.clone().unwrap_or_default(),
                launcher: launcher.clone(),
                kind: "tool call".into(),
                task: clipped_task(&launcher),
                status: status.into(),
                child_id: String::new(),
                transcript: false,
            });
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn classifies_agent_program_not_search_argument() {
        assert_eq!(command_launchers("/root/bin/codex-spark-triage 'task'"), vec!["spark"]);
        assert_eq!(command_launchers("env FOO=1 timeout 30s codex exec 'task'"), vec!["codex"]);
        assert!(command_launchers("rg -n 'codex exec|codex-spark-triage' .").is_empty());
    }

    #[test]
    fn classifies_compound_and_shell_wrapped_launches() {
        assert_eq!(
            command_launchers("bash -lc 'codex exec task; claude -p other'"),
            vec!["codex", "claude"]
        );
    }

    #[test]
    fn codex_custom_exec_recovers_wrapped_launch() {
        let event = json!({
            "timestamp": "2026-08-01T01:02:03Z",
            "type": "response_item",
            "payload": {
                "type": "custom_tool_call",
                "call_id": "call-1",
                "name": "exec",
                "arguments": "const r = await tools.exec_command({cmd: \"/root/bin/codex-spark-triage 'bounded task'\", workdir: \"/root\"});"
            }
        });
        let mut out = Vec::new();
        observe_codex(&event, &mut out);
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].launcher, "spark");
        assert_eq!(out[0].status, "attempted");
    }

    #[test]
    fn codex_native_spawn_is_logged_before_a_child_exists() {
        let call = json!({
            "timestamp": "2026-08-01T01:02:03Z",
            "type": "response_item",
            "payload": {
                "type": "function_call", "name": "spawn_agent", "call_id": "spawn-1",
                "arguments": "{\"agent_type\":\"worker\",\"message\":\"bounded task\"}"
            }
        });
        let result = json!({
            "type": "response_item",
            "payload": {"type": "function_call_output", "call_id": "spawn-1", "output": "error"}
        });
        let mut out = Vec::new();
        observe_codex(&call, &mut out);
        assert_eq!(out[0].launcher, "worker");
        assert_eq!(out[0].kind, "native call");
        assert_eq!(out[0].status, "attempted");
        observe_codex(&result, &mut out);
        assert_eq!(out[0].status, "returned");
    }

    #[test]
    fn inspection_command_does_not_manufacture_launch() {
        let event = json!({
            "type": "response_item",
            "payload": {
                "type": "custom_tool_call",
                "call_id": "call-2",
                "name": "exec",
                "arguments": "const r = await tools.exec_command({cmd: \"rg -n 'codex-spark-triage' /root/bin\"});"
            }
        });
        let mut out = Vec::new();
        observe_codex(&event, &mut out);
        assert!(out.is_empty());
    }

    #[test]
    fn patch_payload_with_command_example_is_not_a_launch() {
        let event = json!({
            "type": "response_item",
            "payload": {
                "type": "custom_tool_call", "name": "exec", "call_id": "call-3",
                "input": "const patch = \"*** Begin Patch\\n+ cmd: \\\"/root/bin/codex-spark-triage task\\\"\\n*** End Patch\"; text(await tools.apply_patch(patch));"
            }
        });
        let mut out = Vec::new();
        observe_codex(&event, &mut out);
        assert!(out.is_empty());
    }

    #[test]
    fn claude_nested_agent_call_is_an_attempt_without_child() {
        let event = json!({
            "timestamp": "2026-08-01T01:02:03Z",
            "message": {"content": [{
                "type": "tool_use", "id": "toolu-1", "name": "Agent",
                "input": {"subagent_type": "Explore", "description": "inspect queue"}
            }]}
        });
        let mut out = Vec::new();
        observe_claude(&event, &mut out);
        assert_eq!(out[0].launcher, "Explore");
        assert_eq!(out[0].task, "inspect queue");
    }
}

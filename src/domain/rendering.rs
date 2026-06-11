//! Rendering of agent output into Discord messages.
//!
//! Discord caps messages at 2000 characters. Markdown is split at line
//! boundaries; an open code fence is closed at the end of a chunk and
//! reopened at the start of the next so fences stay balanced.

use crate::domain::session::Part;
use serde_json::Value;

pub const DISCORD_MESSAGE_LIMIT: usize = 2000;

/// Marker prefixed to agent prose.
pub const AGENT_PREFIX: &str = "⬥ ";
/// Marker prefixed to tool/progress lines.
pub const TOOL_PREFIX: &str = "┣ ";
/// Marker prefixed to messages dispatched from the queue after a wait.
pub const QUEUE_PREFIX: &str = "» ";

/// Split markdown into chunks that fit in a Discord message, keeping code
/// fences balanced across chunks.
pub fn split_markdown(text: &str, limit: usize) -> Vec<String> {
    let mut chunks = Vec::new();
    let mut current = String::new();
    let mut open_fence: Option<String> = None;

    let flush = |current: &mut String, chunks: &mut Vec<String>, open_fence: &Option<String>| {
        if current.trim().is_empty() {
            current.clear();
            return;
        }
        let mut chunk = std::mem::take(current);
        if open_fence.is_some() {
            if !chunk.ends_with('\n') {
                chunk.push('\n');
            }
            chunk.push_str("```");
        }
        chunks.push(chunk);
    };

    for line in text.split_inclusive('\n') {
        let trimmed = line.trim_end();
        let fence_open_cost = open_fence.as_ref().map(|f| f.len() + 1).unwrap_or(0);
        // Reserve room to close a fence ("\n```" = 4 chars).
        let reserve = if open_fence.is_some() { 4 } else { 0 };

        if current.len() + line.len() + reserve > limit {
            flush(&mut current, &mut chunks, &open_fence);
            if let Some(fence) = &open_fence {
                current.push_str(fence);
                current.push('\n');
            }
        }

        // A single line longer than the limit gets hard-truncated.
        if line.len() + fence_open_cost + reserve > limit {
            let take = limit.saturating_sub(fence_open_cost + reserve + 4);
            let mut cut = take.min(line.len());
            while !line.is_char_boundary(cut) {
                cut -= 1;
            }
            current.push_str(&line[..cut]);
            current.push_str("...\n");
        } else {
            current.push_str(line);
        }

        if let Some(fence_body) = trimmed.strip_prefix("```") {
            if open_fence.is_some() {
                open_fence = None;
            } else {
                let lang = fence_body.trim();
                open_fence = Some(format!("```{lang}"));
            }
        }
    }
    flush(&mut current, &mut chunks, &open_fence);
    chunks
}

/// Render a single OpenCode part to Discord markdown. Returns None for parts
/// that should not be shown (step markers, snapshots, etc).
pub fn format_part(part: &Part) -> Option<String> {
    let p = &part.payload;
    match part.kind.as_str() {
        "text" => {
            let text = p.get("text").and_then(Value::as_str)?.trim();
            if text.is_empty() {
                return None;
            }
            Some(format!("{AGENT_PREFIX}{text}"))
        }
        "reasoning" => Some(format!("{TOOL_PREFIX}thinking")),
        "file" => {
            // `filename` is canonical in OpenCode file parts; `file` is kept
            // as a fallback for older event shapes.
            let name = p
                .get("filename")
                .or_else(|| p.get("file"))
                .and_then(Value::as_str)
                .unwrap_or("file");
            Some(format!("{TOOL_PREFIX}📄 {name}"))
        }
        "tool" => {
            let tool = p.get("tool").and_then(Value::as_str).unwrap_or("tool");
            let state = p.get("state").unwrap_or(&Value::Null);
            let status = state.get("status").and_then(Value::as_str).unwrap_or("");
            // Only render once the tool actually started/finished; pending
            // states would produce duplicate lines.
            if status != "running" && status != "completed" && status != "error" {
                return None;
            }
            let detail = tool_detail(tool, state);
            let suffix = if status == "error" { " (error)" } else { "" };
            Some(match detail {
                Some(d) => format!("{TOOL_PREFIX}{tool}: {d}{suffix}"),
                None => format!("{TOOL_PREFIX}{tool}{suffix}"),
            })
        }
        // step-start, step-finish, snapshot, patch, agent...
        _ => None,
    }
}

fn tool_detail(tool: &str, state: &Value) -> Option<String> {
    let input = state.get("input")?;
    let detail = match tool {
        "bash" => input.get("command").and_then(Value::as_str).map(str::to_string),
        "read" | "write" | "edit" => input
            .get("filePath")
            .or_else(|| input.get("file_path"))
            .and_then(Value::as_str)
            .map(str::to_string),
        "grep" | "glob" => input.get("pattern").and_then(Value::as_str).map(str::to_string),
        _ => state.get("title").and_then(Value::as_str).map(str::to_string),
    }?;
    let one_line = detail.replace('\n', " ");
    let mut short = one_line.trim().to_string();
    if short.len() > 120 {
        let mut cut = 120;
        while !short.is_char_boundary(cut) {
            cut -= 1;
        }
        short.truncate(cut);
        short.push_str("...");
    }
    Some(format!("`{}`", short.replace('`', "'")))
}

/// Footer shown after each assistant turn, e.g. `-# lily ⋅ 2m 30s`.
pub fn turn_footer(elapsed: std::time::Duration) -> String {
    let secs = elapsed.as_secs();
    let human = if secs >= 60 {
        format!("{}m {}s", secs / 60, secs % 60)
    } else {
        format!("{secs}s")
    };
    format!("-# lily ⋅ {human}")
}

/// First `max` characters of a normalized (whitespace-collapsed) prompt.
pub fn prompt_preview(prompt: &str, max: usize) -> String {
    let normalized = prompt.split_whitespace().collect::<Vec<_>>().join(" ");
    let mut cut = normalized.len().min(max);
    while !normalized.is_char_boundary(cut) {
        cut -= 1;
    }
    normalized[..cut].to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn split_short_text_is_single_chunk() {
        assert_eq!(split_markdown("hello world", 2000), vec!["hello world".to_string()]);
    }

    #[test]
    fn split_keeps_fences_balanced() {
        let mut text = String::from("```rust\n");
        for i in 0..100 {
            text.push_str(&format!("let x{i} = {i}; // padding padding padding\n"));
        }
        text.push_str("```\n");
        let chunks = split_markdown(&text, 500);
        assert!(chunks.len() > 1);
        for chunk in &chunks {
            assert!(chunk.len() <= 500, "chunk too long: {}", chunk.len());
            let fences = chunk.matches("```").count();
            assert_eq!(fences % 2, 0, "unbalanced fences in chunk: {chunk}");
        }
    }

    #[test]
    fn split_handles_overlong_line() {
        let text = "x".repeat(5000);
        let chunks = split_markdown(&text, 2000);
        assert!(chunks.iter().all(|c| c.len() <= 2000));
    }

    #[test]
    fn preview_collapses_whitespace() {
        assert_eq!(prompt_preview("a  b\n\nc", 120), "a b c");
    }
}

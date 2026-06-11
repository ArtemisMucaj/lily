//! Parsing of message suffixes that change delivery semantics.
//!
//! - `. queue` (any of `.!?,;:` + `queue`, a trailing `queue.`, `queue` on its
//!   own final line, or a standalone `queue` message): deliver after the
//!   current run finishes instead of interrupting it.
//! - `. btw`: fork the session context into a new thread and answer there in
//!   parallel. Unlike `queue`, `btw` requires punctuation or a newline before
//!   it, so `btw fix this` is not treated as a btw.
//!
//! The suffix is stripped before the prompt reaches the agent.

use regex::Regex;
use std::sync::OnceLock;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Delivery {
    /// Plain message: interrupts the current run after a grace period.
    Normal,
    /// Wait for the current run to finish, then send.
    Queue,
    /// Fork context into a new thread and ask there.
    Btw,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParsedMessage {
    pub delivery: Delivery,
    /// Message text with the suffix stripped.
    pub prompt: String,
}

fn queue_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r"(?i)(?:[.!?,;:]|^)\s*queue\.?\s*$|\n\s*queue\.?\s*$").unwrap())
}

fn btw_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r"(?i)[.!?,;:]\s*btw\.?\s*$|\n\s*btw\.?\s*$").unwrap())
}

/// Strip a detected suffix match from `text`, returning the remaining prompt.
fn strip_match(text: &str, re: &Regex) -> Option<String> {
    let m = re.find(text)?;
    Some(text[..m.start()].trim().to_string())
}

pub fn parse_message(text: &str) -> ParsedMessage {
    if let Some(prompt) = strip_match(text, queue_re())
        && !prompt.is_empty() {
            return ParsedMessage { delivery: Delivery::Queue, prompt };
        }
    if let Some(prompt) = strip_match(text, btw_re())
        && !prompt.is_empty() {
            return ParsedMessage { delivery: Delivery::Btw, prompt };
        }
    ParsedMessage { delivery: Delivery::Normal, prompt: text.trim().to_string() }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(s: &str) -> (Delivery, String) {
        let p = parse_message(s);
        (p.delivery, p.prompt)
    }

    #[test]
    fn plain_message() {
        assert_eq!(parse("fix the test"), (Delivery::Normal, "fix the test".into()));
    }

    #[test]
    fn queue_with_period() {
        assert_eq!(parse("fix the test. queue"), (Delivery::Queue, "fix the test".into()));
    }

    #[test]
    fn queue_with_bang() {
        assert_eq!(parse("commit it! queue"), (Delivery::Queue, "commit it".into()));
    }

    #[test]
    fn queue_trailing_period_needs_separator() {
        // Same as kimaki: `queue.` still needs punctuation/newline before it.
        assert_eq!(parse("review this queue."), (Delivery::Normal, "review this queue.".into()));
        assert_eq!(parse("review this. queue."), (Delivery::Queue, "review this".into()));
    }

    #[test]
    fn queue_own_line() {
        assert_eq!(parse("review this\nqueue"), (Delivery::Queue, "review this".into()));
    }

    #[test]
    fn queue_word_inside_text_not_stripped() {
        assert_eq!(
            parse("add a message queue implementation"),
            (Delivery::Normal, "add a message queue implementation".into())
        );
    }

    #[test]
    fn btw_with_punctuation() {
        assert_eq!(parse("why this approach? btw"), (Delivery::Btw, "why this approach".into()));
    }

    #[test]
    fn btw_requires_separator() {
        assert_eq!(
            parse("rename the btw variable btw"),
            (Delivery::Normal, "rename the btw variable btw".into())
        );
    }

    #[test]
    fn btw_own_line() {
        assert_eq!(
            parse("why did you pick sqlite\nbtw"),
            (Delivery::Btw, "why did you pick sqlite".into())
        );
    }

    #[test]
    fn bare_keyword_is_normal() {
        // A message that is only "queue" strips to an empty prompt; treat as normal text.
        assert_eq!(parse("queue"), (Delivery::Normal, "queue".into()));
    }
}

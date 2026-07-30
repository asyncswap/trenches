// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 AsyncSwap Labs, Inc.
//! Ask the machine about the market it is watching.
//!
//! v1 is deliberately thin: spawn the user's own `claude` CLI headless, with
//! the app's live state written into the prompt. Their auth, their models,
//! their bill — nothing here holds an API key, bundles a model, or trades.
//! The copilot reads the room and answers; every order stays a key YOU press.

/// True when a `claude` binary is on PATH — the feature exists exactly when
/// the user has installed it.
pub fn available() -> bool {
    which("claude")
}

fn which(bin: &str) -> bool {
    let Ok(path) = std::env::var("PATH") else { return false };
    std::env::split_paths(&path).any(|d| d.join(bin).is_file())
}

/// Fire the question on a plain thread and hand back the channel the answer
/// arrives on. The UI polls it with a spinner and an Esc — never blocking a
/// render loop on a language model.
pub fn spawn_ask(context: String, question: String) -> std::sync::mpsc::Receiver<String> {
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let _ = tx.send(ask(&context, &question));
    });
    rx
}

fn ask(context: &str, question: &str) -> String {
    use std::io::Write;
    use std::process::{Command, Stdio};

    if !available() {
        return "The copilot needs Claude Code installed — it borrows the `claude` \
 binary on your PATH (your login, your models). Get it at \
                https://claude.com/claude-code and press A again."
            .to_string();
    }

    let prompt = format!(
        "You are the copilot inside Trenches, a terminal app for trading memecoins. \
         The user is mid-session; answer like a sharp trading desk neighbor: terse, \
         concrete, plain text (no markdown headings), a few short paragraphs at most. \
         Numbers you cite must come from the context below. If the context can't \
         answer, say so plainly. Never invent trades and never claim you executed \
         anything — you cannot; only the user's keys trade.\n\n\
         LIVE CONTEXT\n{context}\n\nQUESTION\n{question}\n"
    );

    let child = Command::new("claude")
        .args(["-p", "--output-format", "text"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn();
    let mut child = match child {
        Ok(c) => c,
        Err(e) => return format!("could not start claude: {e}"),
    };
    if let Some(mut stdin) = child.stdin.take() {
        let _ = stdin.write_all(prompt.as_bytes());
    }
    match child.wait_with_output() {
        Ok(out) if out.status.success() => {
            let text = String::from_utf8_lossy(&out.stdout).trim().to_string();
            if text.is_empty() {
                "claude answered nothing — try again.".to_string()
            } else {
                text
            }
        }
        Ok(out) => format!(
            "claude exited with {}: {}",
            out.status,
            String::from_utf8_lossy(&out.stdout).chars().take(300).collect::<String>()
        ),
        Err(e) => format!("claude failed: {e}"),
    }
}

/// One conversation with the machine, spanning questions. The transcript is
/// what the UI renders; the session id is what lets the NEXT question resume
/// the same memory — claude holds the thread, we hold the receipt.
#[derive(Default)]
pub struct Chat {
    pub session_id: Option<String>,
    /// `(is_user, text)`, oldest first.
    pub transcript: Vec<(bool, String)>,
}

/// Ask within a conversation. Returns `(answer, session_id)` — the id from
/// claude's JSON envelope, handed back on every turn so a crashed parse on
/// one turn doesn't orphan the thread.
pub fn spawn_chat_ask(
    session: Option<String>,
    context: String,
    question: String,
) -> std::sync::mpsc::Receiver<(String, Option<String>)> {
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let _ = tx.send(ask_chat(session.as_deref(), &context, &question));
    });
    rx
}

fn ask_chat(session: Option<&str>, context: &str, question: &str) -> (String, Option<String>) {
    use std::io::Write;
    use std::process::{Command, Stdio};

    if !available() {
        return (
            "The copilot needs Claude Code installed — it borrows the `claude` binary on your PATH. Get it at https://claude.com/claude-code and press A again."
                .to_string(),
            None,
        );
    }

    // The first turn carries the manners and the room; follow-ups carry a
    // fresh look at the room — the market moved while you were typing.
    let prompt = match session {
        None => format!(
            "You are the copilot inside Trenches, a terminal app for trading memecoins. The user is mid-session; answer like a sharp trading desk neighbor: terse, concrete, plain text (no markdown headings). Numbers you cite must come from the context. If the context can't answer, say so plainly. Never claim you executed anything — you cannot; only the user's keys trade.\n\nLIVE CONTEXT\n{context}\n\nQUESTION\n{question}\n"
        ),
        Some(_) => format!(
            "CONTEXT UPDATE — the live market state as of this question (the tape may have moved since your last look):\n{context}\n\nQUESTION\n{question}\n"
        ),
    };

    let mut cmd = Command::new("claude");
    cmd.args(["-p", "--output-format", "json"]);
    if let Some(id) = session {
        cmd.args(["--resume", id]);
    }
    let child = cmd.stdin(Stdio::piped()).stdout(Stdio::piped()).stderr(Stdio::null()).spawn();
    let mut child = match child {
        Ok(c) => c,
        Err(e) => return (format!("could not start claude: {e}"), None),
    };
    if let Some(mut stdin) = child.stdin.take() {
        let _ = stdin.write_all(prompt.as_bytes());
    }
    match child.wait_with_output() {
        Ok(out) if out.status.success() => {
            let raw = String::from_utf8_lossy(&out.stdout);
            match serde_json::from_str::<serde_json::Value>(raw.trim()) {
                Ok(v) => {
                    let ans = v
                        .get("result")
                        .and_then(|r| r.as_str())
                        .unwrap_or("claude answered nothing — try again.")
                        .trim()
                        .to_string();
                    let sid = v.get("session_id").and_then(|s| s.as_str()).map(String::from);
                    (ans, sid)
                }
                Err(_) => (raw.trim().to_string(), None),
            }
        }
        Ok(out) => (
            format!(
                "claude exited with {}: {}",
                out.status,
                String::from_utf8_lossy(&out.stdout).chars().take(300).collect::<String>()
            ),
            None,
        ),
        Err(e) => (format!("claude failed: {e}"), None),
    }
}

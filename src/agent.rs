// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 AsyncSwap Labs, Inc.
//! Ask the machine about the market it is watching.
#![cfg_attr(not(feature = "agent"), allow(dead_code))]
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
        return "The copilot needs Claude Code installed — it borrows the `claude`  binary on your PATH (your login, your models). Get it at  https://claude.com/claude-code and press A again."
            .to_string();
    }

    let prompt = format!(
        "You are the copilot inside Trenches, a terminal app for trading memecoins.  The user is mid-session; answer like a sharp trading desk neighbor: terse,  concrete, plain text (no markdown headings), a few short paragraphs at most.  Numbers you cite must come from the context below. If the context can't  answer, say so plainly. Never invent trades and never claim you executed  anything — you cannot; only the user's keys trade.\n\n LIVE CONTEXT\n{context}\n\nQUESTION\n{question}\n"
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

/// One streamed reply: text as it is generated, then the receipt.
pub enum StreamEvent {
    Delta(String),
    Done { session_id: Option<String> },
    Fail(String),
}

/// Ask within a conversation, streaming. Deltas arrive as claude writes;
/// `Done` carries the session id that lets the next question resume the
/// thread. The child is spawned on a plain thread — the UI polls the
/// channel between frames and never blocks on the model.
pub fn spawn_stream(
    session: Option<String>,
    context: String,
    question: String,
) -> std::sync::mpsc::Receiver<StreamEvent> {
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || stream(tx, session.as_deref(), &context, &question));
    rx
}

fn stream(
    tx: std::sync::mpsc::Sender<StreamEvent>,
    session: Option<&str>,
    context: &str,
    question: &str,
) {
    use std::io::{BufRead, Write};
    use std::process::{Command, Stdio};

    if !available() {
        let _ = tx.send(StreamEvent::Fail(
            "the copilot needs Claude Code installed — https://claude.com/claude-code".into(),
        ));
        return;
    }
    let prompt = match session {
        None => format!(
            "You are the copilot inside Trenches, a terminal app for trading memecoins.  The user is mid-session; answer like a sharp trading desk neighbor: terse,  concrete, plain text (no markdown headings). Numbers you cite must come  from the context. If the context can't answer, say so plainly. Never claim  you executed anything — you cannot; only the user's keys trade.\n\n LIVE CONTEXT\n{context}\n\nQUESTION\n{question}\n"
        ),
        Some(_) => format!(
            "CONTEXT UPDATE — the live market as of this question:\n{context}\n\nQUESTION\n{question}\n"
        ),
    };
    let mut cmd = Command::new("claude");
    cmd.args([
        "-p",
        "--output-format",
        "stream-json",
        "--include-partial-messages",
        "--verbose",
    ]);
    if let Some(id) = session {
        cmd.args(["--resume", id]);
    }
    let child = cmd.stdin(Stdio::piped()).stdout(Stdio::piped()).stderr(Stdio::null()).spawn();
    let mut child = match child {
        Ok(c) => c,
        Err(e) => {
            let _ = tx.send(StreamEvent::Fail(format!("could not start claude: {e}")));
            return;
        }
    };
    if let Some(mut stdin) = child.stdin.take() {
        let _ = stdin.write_all(prompt.as_bytes());
    }
    let Some(stdout) = child.stdout.take() else {
        let _ = tx.send(StreamEvent::Fail("no stdout from claude".into()));
        return;
    };
    let mut got_delta = false;
    let mut session_id: Option<String> = None;
    for line in std::io::BufReader::new(stdout).lines() {
        let Ok(line) = line else { break };
        let Ok(v) = serde_json::from_str::<serde_json::Value>(&line) else { continue };
        match v.get("type").and_then(|t| t.as_str()) {
            Some("stream_event") => {
                if let Some(d) = v
                    .get("event")
                    .and_then(|e| e.get("delta"))
                    .and_then(|d| d.get("text"))
                    .and_then(|t| t.as_str())
                {
                    got_delta = true;
                    if tx.send(StreamEvent::Delta(d.to_string())).is_err() {
                        let _ = child.kill();
                        return;
                    }
                }
            }
            Some("result") => {
                session_id = v.get("session_id").and_then(|s| s.as_str()).map(String::from);
                // An older CLI without partial messages still answers — the
                // whole reply arrives here as one late delta.
                if !got_delta {
                    if let Some(r) = v.get("result").and_then(|r| r.as_str()) {
                        let _ = tx.send(StreamEvent::Delta(r.to_string()));
                    }
                }
            }
            _ => {}
        }
    }
    let _ = child.wait();
    let _ = tx.send(StreamEvent::Done { session_id });
}

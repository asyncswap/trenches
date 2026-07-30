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

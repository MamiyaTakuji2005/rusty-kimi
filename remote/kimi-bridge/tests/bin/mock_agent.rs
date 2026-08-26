//! A scripted stand-in for `kimi-agent` for the bridge e2e tests.
//!
//! Protocol on its stdin (one line per input):
//!
//! - `argv`     → replies its own argv[1..] joined with `0x1f`
//! - `say X`    → replies `X`
//! - `die`      → exits immediately without replying
//! - anything else → echoed back verbatim
//!
//! When its stdin closes it emits one final `MOCK-AGENT-EOF` line and
//! exits 0 — which is exactly what the real agent does on stdin EOF, and
//! what the close-propagation tests assert on.

use std::io::{BufRead, BufReader, Write};

fn main() {
    eprintln!(
        "mock-agent: argv: {:?}",
        std::env::args().collect::<Vec<_>>()
    );
    let stdin = std::io::stdin();
    let mut reader = BufReader::new(stdin.lock());
    let mut stdout = std::io::stdout();
    let mut line = String::new();
    loop {
        line.clear();
        match reader.read_line(&mut line) {
            Ok(0) => break,
            Ok(_) => {}
            Err(_) => break,
        }
        let trimmed = line.trim_end();
        if trimmed.is_empty() {
            continue;
        }
        if trimmed == "die" {
            break;
        }
        let reply = if trimmed == "argv" {
            std::env::args().skip(1).collect::<Vec<_>>().join("\u{1f}")
        } else if let Some(said) = trimmed.strip_prefix("say ") {
            said.to_string()
        } else {
            trimmed.to_string()
        };
        if writeln!(stdout, "{reply}")
            .and_then(|_| stdout.flush())
            .is_err()
        {
            break;
        }
    }
    // The client may have fully closed by now; a failed write here just
    // means nobody is listening, which is fine.
    let _ = writeln!(stdout, "MOCK-AGENT-EOF");
    let _ = stdout.flush();
}

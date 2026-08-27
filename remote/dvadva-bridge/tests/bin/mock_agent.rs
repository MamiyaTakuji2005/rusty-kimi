//! A scripted stand-in for `dvadva-agent` for the bridge e2e tests.
//!
//! Two modes, matching the agent's two transports.
//!
//! **Stdio** (no `--listen`): reads its stdin, writes its stdout, and exits
//! when stdin closes. What `spawn` drives.
//!
//! **Listening** (`--listen`, which the daemon appends): binds a loopback
//! port, mints a token, puts itself in the live-session registry, and serves
//! whoever attaches — one client at a time or several, and a client leaving
//! is not the end of anything. What `attach` drives. It stands in for the
//! real `wire/listener.rs`, so it does that module's token handshake, and
//! answers with the session it hosts.
//!
//! Protocol on either transport (one line per input):
//!
//! - `argv`     → replies its own argv[1..] joined with `0x1f`
//! - `pid`      → replies its process id, so a test can tell "the same agent"
//!   from "another one just like it"
//! - `say X`    → replies `X`
//! - `die`      → stops talking: over stdio that is the end of the process,
//!   over a socket it is the end of that one connection
//! - `crash`    → writes a line to stderr and exits 1, an agent falling over
//!   under a client that was attached to it
//! - `stop`     → exits 0, the clean stop a test uses to put its agent away
//! - `fail X`   → writes `X` to stderr and exits 2, standing in for the
//!   real agent's startup failures (bad work dir, missing credentials)
//! - anything else → echoed back verbatim
//!
//! `--fail-to-start X` fails before listening at all: the supervised
//! equivalent of a bad work directory, which the daemon has to diagnose from
//! the agent's log rather than from a relay that never started.
//!
//! On stdin EOF it emits one final `MOCK-AGENT-EOF` line and exits 0 — which
//! is exactly what the real agent does on stdin EOF, and what the
//! close-propagation tests assert on.

use std::io::{BufRead, BufReader, Write};
use std::time::Duration;

use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader as AsyncBufReader};
use tokio::net::{TcpListener, TcpStream};

use dvadva_agent::live::{self, LiveSession, Registry};

/// A test agent that nobody stops must still not outlive the test run: a
/// stray one holds its own binary open, and on Windows that fails the next
/// build rather than the next test.
const MAX_LIFETIME: Duration = Duration::from_secs(120);

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    eprintln!("mock-agent: argv: {args:?}");

    if let Some(message) = flag_value(&args, "--fail-to-start") {
        eprintln!("{message}");
        std::process::exit(2);
    }
    if args.iter().any(|arg| arg == "--listen") {
        listen(&args);
        return;
    }
    stdio();
}

// ------------------------------------------------------------------ stdio

fn stdio() {
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
        match reply_to(trimmed) {
            Response::Say(reply) => {
                if writeln!(stdout, "{reply}")
                    .and_then(|_| stdout.flush())
                    .is_err()
                {
                    break;
                }
            }
            // Over stdio the connection is the process, so this is the end
            // of both.
            Response::Stop => break,
        }
    }
    // The client may have fully closed by now; a failed write here just
    // means nobody is listening, which is fine.
    let _ = writeln!(stdout, "MOCK-AGENT-EOF");
    let _ = stdout.flush();
}

// -------------------------------------------------------------- listening

fn listen(args: &[String]) {
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .expect("runtime");
    runtime.block_on(serve(args));
}

async fn serve(args: &[String]) {
    let session = flag_value(args, "--session")
        .unwrap_or_else(|| format!("mock-session-{}", std::process::id()));
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let addr = listener.local_addr().expect("addr");

    // The registry the daemon told us about, through `KIMI_SHARE_DIR`.
    let registry = Registry::shared();
    let token = format!("mock-token-{}", std::process::id());
    tokio::fs::create_dir_all(registry.dir())
        .await
        .expect("registry dir");
    let token_file = registry.dir().join(format!("{session}.token"));
    tokio::fs::write(&token_file, &token)
        .await
        .expect("token file");

    let entry = LiveSession {
        session: session.clone(),
        pid: std::process::id(),
        addr: addr.to_string(),
        token_file,
        work_dir: flag_value(args, "-w").unwrap_or_default(),
        protocol_version: "1.3".to_string(),
        agent_version: "mock".to_string(),
        started_at: live::now_seconds(),
    };
    // Held for the life of the process, exactly as the real listener holds
    // its own: the accept loop below never returns.
    let _registration = registry.register(&entry).await.expect("register");
    eprintln!("mock-agent: listening on {addr} for session {session}");

    tokio::spawn(async {
        tokio::time::sleep(MAX_LIFETIME).await;
        eprintln!("mock-agent: nobody stopped me; leaving anyway");
        std::process::exit(0);
    });

    loop {
        let Ok((socket, _peer)) = listener.accept().await else {
            continue;
        };
        let token = token.clone();
        let session = session.clone();
        tokio::spawn(async move {
            // A client leaving ends this task and nothing else — which is
            // the entire property these tests are about.
            let _ = attached(socket, &token, &session).await;
        });
    }
}

async fn attached(socket: TcpStream, token: &str, session: &str) -> std::io::Result<()> {
    let _ = socket.set_nodelay(true);
    let (reader, mut writer) = tokio::io::split(socket);
    let mut reader = AsyncBufReader::new(reader);

    let mut greeting = String::new();
    if reader.read_line(&mut greeting).await? == 0 {
        return Ok(());
    }
    let presented = serde_json::from_str::<serde_json::Value>(&greeting)
        .ok()
        .and_then(|value| {
            value
                .get("auth")
                .and_then(|auth| auth.as_str())
                .map(str::to_string)
        });
    if presented.as_deref() != Some(token) {
        let denied = serde_json::json!({"auth": "denied", "error": "invalid token"});
        writer.write_all(format!("{denied}\n").as_bytes()).await?;
        return writer.flush().await;
    }
    let ok = serde_json::json!({"auth": "ok", "session": session});
    writer.write_all(format!("{ok}\n").as_bytes()).await?;
    writer.flush().await?;

    let mut line = String::new();
    loop {
        line.clear();
        if reader.read_line(&mut line).await? == 0 {
            return Ok(());
        }
        let trimmed = line.trim_end();
        if trimmed.is_empty() {
            continue;
        }
        let Response::Say(reply) = reply_to(trimmed) else {
            // Only this connection ends. The process is the session, and
            // the session is not this client's to end.
            return Ok(());
        };
        writer.write_all(format!("{reply}\n").as_bytes()).await?;
        writer.flush().await?;
    }
}

// ----------------------------------------------------------------- shared

/// What one line of input means.
enum Response {
    /// Answer with this.
    Say(String),
    /// Stop talking. What that costs depends on the transport, which is the
    /// difference the attach tests are about.
    Stop,
}

/// What to answer. The commands that end the *process* do so from here, so
/// both transports obey the same script.
fn reply_to(input: &str) -> Response {
    if input == "die" {
        return Response::Stop;
    }
    if input == "stop" {
        std::process::exit(0);
    }
    if input == "crash" {
        eprintln!("mock-agent: falling over on request");
        std::process::exit(1);
    }
    if let Some(message) = input.strip_prefix("fail ") {
        eprintln!("{message}");
        std::process::exit(2);
    }
    if input == "argv" {
        return Response::Say(std::env::args().skip(1).collect::<Vec<_>>().join("\u{1f}"));
    }
    if input == "pid" {
        return Response::Say(std::process::id().to_string());
    }
    if let Some(said) = input.strip_prefix("say ") {
        return Response::Say(said.to_string());
    }
    Response::Say(input.to_string())
}

/// The value after a flag in argv, for the handful this mock reads itself.
fn flag_value(args: &[String], flag: &str) -> Option<String> {
    args.iter()
        .position(|arg| arg == flag)
        .and_then(|at| args.get(at + 1))
        .cloned()
}

//! The local daemon: runs on the frontend machine.
//!
//! It accepts the same bridge connections the frontends speak and forwards
//! them upstream to the remote daemon. In the common `ssh -L` deployment
//! it is optional (the tunnel already lands on the remote daemon's
//! loopback); it earns its keep when the network leg is a plain TCP hop or
//! when frontends should not know where the upstream lives.
//!
//! Stays a dumb relay: the client's header line is forwarded verbatim, the
//! upstream's single reply frame is passed through, everything after is
//! opaque bytes.

use std::io;
use std::time::Duration;

use tokio::io::{AsyncWriteExt, BufReader};
use tokio::net::{TcpListener, TcpStream};
use tokio::time::timeout;

use crate::proto::{self, Reply, Request};

/// How long a connection may take to send its header line.
const HEADER_TIMEOUT: Duration = Duration::from_secs(10);

/// Consecutive `accept()` failures tolerated before giving up — same
/// reasoning (and same numbers) as the remote daemon's loop.
const MAX_CONSECUTIVE_ACCEPT_ERRORS: usize = 64;

/// Pause after a failed `accept()` so a persistent error cannot spin hot.
const ACCEPT_ERROR_BACKOFF: Duration = Duration::from_millis(100);

/// Serve bridge connections forever, forwarding each one to `upstream`.
pub async fn serve(listener: TcpListener, upstream: String) -> io::Result<()> {
    let mut consecutive_errors = 0usize;
    loop {
        let (socket, peer) = match listener.accept().await {
            Ok(accepted) => {
                consecutive_errors = 0;
                accepted
            }
            Err(err) => {
                consecutive_errors += 1;
                eprintln!("dvadva-bridge: accept failed ({consecutive_errors}): {err}");
                if consecutive_errors >= MAX_CONSECUTIVE_ACCEPT_ERRORS {
                    return Err(io::Error::new(
                        err.kind(),
                        format!(
                            "giving up after {consecutive_errors} consecutive accept failures: {err}"
                        ),
                    ));
                }
                tokio::time::sleep(ACCEPT_ERROR_BACKOFF).await;
                continue;
            }
        };
        let _ = socket.set_nodelay(true);
        let upstream = upstream.clone();
        tokio::spawn(async move {
            if let Err(err) = handle(socket, &upstream).await {
                eprintln!("dvadva-bridge: {peer}: connection error: {err}");
            }
        });
    }
}

async fn handle(socket: TcpStream, upstream: &str) -> io::Result<()> {
    let mut client = BufReader::new(socket);

    let line = match timeout(HEADER_TIMEOUT, proto::read_line(&mut client)).await {
        Ok(result) => result?,
        Err(_) => {
            return Err(io::Error::new(
                io::ErrorKind::TimedOut,
                "no bridge frame within 10s",
            ));
        }
    };
    // Decode only to pick the mode; the raw line is forwarded verbatim so
    // the upstream daemon remains the single parser of client frames.
    let request = match proto::decode::<Request>(&line) {
        Ok(request) => request,
        Err(err) => {
            write_frame(&mut client, proto::encode(&Reply::error(err))).await?;
            return client.get_mut().shutdown().await;
        }
    };

    let mut upstream_sock = match TcpStream::connect(upstream).await {
        Ok(sock) => {
            let _ = sock.set_nodelay(true);
            sock
        }
        Err(err) => {
            let reply = Reply::error(format!("failed to reach upstream `{upstream}`: {err}"));
            write_frame(&mut client, proto::encode(&reply)).await?;
            return client.get_mut().shutdown().await;
        }
    };
    upstream_sock.write_all(line.as_bytes()).await?;
    upstream_sock.write_all(b"\n").await?;
    upstream_sock.flush().await?;

    let mut upstream_sock = BufReader::new(upstream_sock);
    match request {
        // One frame up, one frame back, done.
        Request::ListSessions | Request::Version => {
            let reply_line = proto::read_line(&mut upstream_sock).await?;
            write_frame(&mut client, reply_line).await?;
            client.get_mut().shutdown().await
        }
        Request::Spawn { .. } => {
            // The upstream acknowledges the spawn; hand that frame to the
            // client (verbatim — errors pass through too), then relay
            // opaquely until either side closes.
            let ack_line = proto::read_line(&mut upstream_sock).await?;
            write_frame(&mut client, ack_line).await?;
            relay(client, upstream_sock).await
        }
    }
}

/// Bidirectional byte copy between two already-connected sockets, with the
/// same close-propagation contract as the remote daemon's agent leg.
async fn relay(client: BufReader<TcpStream>, upstream: BufReader<TcpStream>) -> io::Result<()> {
    let (mut c_rd, mut c_wr) = tokio::io::split(client);
    let (mut u_rd, mut u_wr) = tokio::io::split(upstream);

    let to_upstream = tokio::spawn(async move {
        let _ = tokio::io::copy(&mut c_rd, &mut u_wr).await;
        let _ = u_wr.shutdown().await;
    });
    let to_client = tokio::spawn(async move {
        let _ = tokio::io::copy(&mut u_rd, &mut c_wr).await;
        let _ = c_wr.shutdown().await;
    });
    let (_, _) = tokio::join!(to_upstream, to_client);
    Ok(())
}

/// Write one pre-encoded frame line and flush it.
async fn write_frame<W>(writer: &mut W, frame: String) -> io::Result<()>
where
    W: tokio::io::AsyncWrite + Unpin,
{
    writer.write_all(frame.as_bytes()).await?;
    writer.write_all(b"\n").await?;
    writer.flush().await
}

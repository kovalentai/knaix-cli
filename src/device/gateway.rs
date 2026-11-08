//! The KVRPC/1 gateway: the only thing on this machine a coordinator can reach.
//!
//! A port of `createTunnelGateway` in the Node Runtime's `rpcTunnel.ts`, to the
//! letter of the spec. It listens on the tailnet address only. Each connection
//! must open with one line, `KVRPC/1 <ticket>` or `KVRPC/1-PING <ticket>`, at
//! most 4096 bytes, within 5 seconds. The ticket is verified against the key
//! pinned at enrolment and, on Kovalent's tailnet, Tailscale is asked whether
//! the peer is one of the account's nodes. Only then does the gateway connect
//! to rpc-server on loopback, answer `OK`, and pipe bytes both ways.
//!
//! No byte from a client reaches rpc-server before verification succeeds: the
//! preamble is read one byte at a time, so nothing past its newline is ever
//! buffered on the way. A refusal answers `NO refused` and logs the reason
//! here; the reason never goes on the wire.

use crate::device::ticket::{normalize_ip, verify_ticket, PinnedKey};
use std::collections::HashMap;
use std::net::{IpAddr, SocketAddr};
use std::path::PathBuf;
use std::sync::{Arc, Mutex, RwLock};
use std::time::{Duration, Instant};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::Semaphore;

pub const PROTOCOL: &str = "KVRPC/1";
pub const PING: &str = "KVRPC/1-PING";
pub const MAX_PREAMBLE_BYTES: usize = 4096;
pub const PREAMBLE_TIMEOUT: Duration = Duration::from_secs(5);
/// A coordinator opens one connection per device; this is generous headroom.
pub const MAX_CONNECTIONS: usize = 16;
/// Refusals from one address within the window before it is dropped unread.
pub const REFUSAL_BUDGET: u32 = 8;
pub const REFUSAL_WINDOW: Duration = Duration::from_secs(60);
pub const BLOCK_FOR: Duration = Duration::from_secs(300);

/// Where rpc-server answers: a loopback port (macOS), or the Unix socket of
/// the bridge inside its network namespace (Linux).
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Upstream {
    Tcp(u16),
    Unix(PathBuf),
}

/// rpc-server's address while it runs, None while it does not.
pub type SharedUpstream = Arc<RwLock<Option<Upstream>>>;

/// Who may connect, and how to tell.
pub struct Config {
    pub key: PinnedKey,
    pub device_id: String,
    pub upstream: SharedUpstream,
    /// On Kovalent's tailnet, the tag the coordinator must carry
    /// (`tag:u-<clientId>`). None on the account's own tailnet.
    pub node_tag: Option<String>,
    pub tailscale: Option<PathBuf>,
    pub log: Option<PathBuf>,
    /// The clock, injectable so the conformance vectors' fixed time can be used.
    pub now: fn() -> i64,
}

#[derive(Default)]
struct Budget {
    refusals: u32,
    window_start: Option<Instant>,
    blocked_until: Option<Instant>,
}

pub struct Gateway {
    config: Config,
    slots: Semaphore,
    budgets: Mutex<HashMap<IpAddr, Budget>>,
}

pub fn system_now() -> i64 {
    crate::device::state::now_unix() as i64
}

impl Gateway {
    pub fn new(config: Config) -> Arc<Self> {
        Arc::new(Self {
            config,
            slots: Semaphore::new(MAX_CONNECTIONS),
            budgets: Mutex::new(HashMap::new()),
        })
    }

    fn log(&self, line: &str) {
        if let Some(path) = &self.config.log {
            crate::device::state::append_log(path, line);
        }
    }

    /// Whether an address has spent its refusal budget. Checked before any
    /// work, so a peer hammering bad tickets costs no signature checks.
    fn blocked(&self, ip: IpAddr) -> bool {
        let mut budgets = self.budgets.lock().unwrap_or_else(|e| e.into_inner());
        match budgets.get(&ip).and_then(|b| b.blocked_until) {
            Some(until) if until > Instant::now() => true,
            Some(_) => {
                budgets.remove(&ip);
                false
            }
            None => false,
        }
    }

    fn charge(&self, ip: IpAddr) {
        let mut budgets = self.budgets.lock().unwrap_or_else(|e| e.into_inner());
        let now = Instant::now();
        let b = budgets.entry(ip).or_default();
        match b.window_start {
            Some(start) if now.duration_since(start) <= REFUSAL_WINDOW => b.refusals += 1,
            _ => {
                b.window_start = Some(now);
                b.refusals = 1;
            }
        }
        if b.refusals >= REFUSAL_BUDGET {
            b.blocked_until = Some(now + BLOCK_FOR);
            drop(budgets);
            self.log(&format!(
                "blocked {} for {} s after {} refusals in {} s",
                normalize_ip(ip),
                BLOCK_FOR.as_secs(),
                REFUSAL_BUDGET,
                REFUSAL_WINDOW.as_secs()
            ));
        }
    }

    /// Accept connections until the listener fails or the task is dropped.
    pub async fn serve(self: Arc<Self>, listener: TcpListener) {
        loop {
            let Ok((socket, peer)) = listener.accept().await else {
                tokio::time::sleep(Duration::from_millis(100)).await;
                continue;
            };
            let gateway = self.clone();
            tokio::spawn(async move { gateway.handle(socket, peer).await });
        }
    }

    async fn handle(self: Arc<Self>, mut socket: TcpStream, peer: SocketAddr) {
        let ip = peer.ip();
        if self.blocked(ip) {
            return;
        }
        let Ok(_slot) = self.slots.try_acquire() else {
            self.log(&format!(
                "refused {}: too_many_connections",
                normalize_ip(ip)
            ));
            return;
        };
        let _ = socket.set_nodelay(true);
        let remote = normalize_ip(ip);

        let line = match tokio::time::timeout(PREAMBLE_TIMEOUT, read_preamble(&mut socket)).await {
            Ok(Ok(line)) => line,
            Ok(Err(reason)) => {
                self.charge(ip);
                self.log(&format!("refused {remote}: {reason}"));
                return;
            }
            Err(_) => {
                self.charge(ip);
                self.log(&format!("refused {remote}: preamble_timeout"));
                return;
            }
        };

        let (verb, ticket) = match line.find(' ') {
            Some(i) if i > 0 => (&line[..i], line[i + 1..].trim()),
            _ => (line.as_str(), ""),
        };
        if verb != PROTOCOL && verb != PING {
            return self
                .refuse(&mut socket, ip, &remote, "unknown_protocol")
                .await;
        }

        let accepted = match verify_ticket(
            ticket,
            &self.config.key,
            &self.config.device_id,
            &remote,
            (self.config.now)(),
        ) {
            Ok(a) => a,
            Err(reason) => return self.refuse(&mut socket, ip, &remote, reason.as_str()).await,
        };

        // The spec allows added checks and forbids dropping any. On Kovalent's
        // tailnet, the peer must be one of this account's nodes.
        if let (Some(tag), Some(bin)) = (&self.config.node_tag, &self.config.tailscale) {
            match crate::device::tailnet::whois_tags(bin, &remote).await {
                Some(tags) if tags.iter().any(|t| t.eq_ignore_ascii_case(tag)) => {}
                Some(_) => return self.refuse(&mut socket, ip, &remote, "wrong_tag").await,
                None => return self.refuse(&mut socket, ip, &remote, "whois_failed").await,
            }
        }

        let what = format!(
            "pool {} coordinator {} plan {}",
            accepted.pool,
            accepted.coord,
            accepted
                .plan_version
                .map(|v| v.to_string())
                .unwrap_or_else(|| "?".into())
        );

        if verb == PING {
            let _ = socket.write_all(b"PONG\n").await;
            let _ = socket.shutdown().await;
            self.log(&format!("ping from {remote}: {what}"));
            return;
        }

        // Connect upstream first; answer OK only once there is somewhere for
        // the bytes to go.
        let target = self
            .config
            .upstream
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .clone();
        match target {
            Some(Upstream::Tcp(port)) => {
                let connected = tokio::time::timeout(
                    Duration::from_secs(3),
                    TcpStream::connect(("127.0.0.1", port)),
                )
                .await
                .ok()
                .and_then(Result::ok);
                match connected {
                    Some(upstream) => {
                        let _ = upstream.set_nodelay(true);
                        self.pipe(socket, upstream, &remote, &what).await
                    }
                    None => {
                        self.refuse(&mut socket, ip, &remote, "upstream_unavailable")
                            .await
                    }
                }
            }
            #[cfg(unix)]
            Some(Upstream::Unix(path)) => {
                let connected = tokio::time::timeout(
                    Duration::from_secs(3),
                    tokio::net::UnixStream::connect(&path),
                )
                .await
                .ok()
                .and_then(Result::ok);
                match connected {
                    Some(upstream) => self.pipe(socket, upstream, &remote, &what).await,
                    None => {
                        self.refuse(&mut socket, ip, &remote, "upstream_unavailable")
                            .await
                    }
                }
            }
            _ => {
                self.refuse(&mut socket, ip, &remote, "upstream_unavailable")
                    .await
            }
        }
    }

    async fn pipe<U: AsyncRead + AsyncWrite + Unpin>(
        &self,
        mut socket: TcpStream,
        mut upstream: U,
        remote: &str,
        what: &str,
    ) {
        if socket.write_all(b"OK\n").await.is_err() {
            return;
        }
        self.log(&format!("opened from {remote}: {what}"));
        let started = Instant::now();
        let moved = tokio::io::copy_bidirectional(&mut socket, &mut upstream).await;
        let (up, down) = moved.unwrap_or((0, 0));
        self.log(&format!(
            "closed from {remote}: {} s, {up} bytes in, {down} bytes out",
            started.elapsed().as_secs()
        ));
    }

    async fn refuse(&self, socket: &mut TcpStream, ip: IpAddr, remote: &str, reason: &str) {
        self.charge(ip);
        self.log(&format!("refused {remote}: {reason}"));
        // Deliberately vague on the wire.
        let _ = socket.write_all(b"NO refused\n").await;
        let _ = socket.shutdown().await;
    }
}

/// Read one `\n`-terminated line, a byte at a time, so nothing after it is
/// consumed. Bytes past the newline belong to rpc-server, and only after OK.
async fn read_preamble(socket: &mut TcpStream) -> Result<String, &'static str> {
    let mut line = Vec::with_capacity(512);
    let mut byte = [0u8; 1];
    loop {
        match socket.read(&mut byte).await {
            Ok(0) => return Err("closed"),
            Ok(_) => {}
            Err(_) => return Err("closed"),
        }
        if byte[0] == b'\n' {
            break;
        }
        line.push(byte[0]);
        // The limit includes the newline.
        if line.len() >= MAX_PREAMBLE_BYTES {
            return Err("preamble_too_long");
        }
    }
    String::from_utf8(line).map_err(|_| "malformed")
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::Value;
    use std::sync::atomic::Ordering;
    use tokio::io::AsyncBufReadExt;

    const VECTORS: &str = include_str!("../../tests/fixtures/rpc-tunnel-test-vectors.json");

    fn vectors() -> Value {
        serde_json::from_str(VECTORS).unwrap()
    }

    fn fixed_now() -> i64 {
        1_793_000_000
    }

    /// The vector that names no peer, so it verifies from 127.0.0.1.
    fn any_peer_ticket() -> String {
        vectors()["cases"]
            .as_array()
            .unwrap()
            .iter()
            .find(|c| c["name"] == "no peer named, any address")
            .unwrap()["ticket"]
            .as_str()
            .unwrap()
            .to_string()
    }

    /// A fake rpc-server that echoes, and counts the connections it gets.
    async fn echo_upstream() -> (u16, Arc<std::sync::atomic::AtomicUsize>) {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let count = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let c = count.clone();
        tokio::spawn(async move {
            while let Ok((mut s, _)) = listener.accept().await {
                c.fetch_add(1, Ordering::SeqCst);
                tokio::spawn(async move {
                    let (mut r, mut w) = s.split();
                    let _ = tokio::io::copy(&mut r, &mut w).await;
                });
            }
        });
        (port, count)
    }

    async fn gateway(upstream: u16) -> (SocketAddr, Arc<Gateway>) {
        let v = vectors();
        let target = (upstream != 0).then_some(Upstream::Tcp(upstream));
        let g = Gateway::new(Config {
            key: PinnedKey::from_pem(v["publicKeyPem"].as_str().unwrap()).unwrap(),
            device_id: v["deviceId"].as_str().unwrap().to_string(),
            upstream: Arc::new(RwLock::new(target)),
            node_tag: None,
            tailscale: None,
            log: None,
            now: fixed_now,
        });
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(g.clone().serve(listener));
        (addr, g)
    }

    async fn first_line(stream: &mut TcpStream) -> String {
        let mut reader = tokio::io::BufReader::new(stream);
        let mut line = String::new();
        let _ = tokio::time::timeout(Duration::from_secs(5), reader.read_line(&mut line)).await;
        line
    }

    #[tokio::test]
    async fn a_valid_ticket_opens_the_tunnel_and_bytes_flow_both_ways() {
        let (port, count) = echo_upstream().await;
        let (addr, _) = gateway(port).await;
        let mut s = TcpStream::connect(addr).await.unwrap();
        s.write_all(format!("KVRPC/1 {}\n", any_peer_ticket()).as_bytes())
            .await
            .unwrap();
        let mut ok = [0u8; 3];
        s.read_exact(&mut ok).await.unwrap();
        assert_eq!(&ok, b"OK\n");
        s.write_all(b"rpc bytes").await.unwrap();
        let mut back = [0u8; 9];
        s.read_exact(&mut back).await.unwrap();
        assert_eq!(&back, b"rpc bytes");
        assert_eq!(count.load(Ordering::SeqCst), 1);
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn the_tunnel_reaches_rpc_server_through_the_bridge_socket() {
        let dir = std::env::temp_dir().join(format!("knaix-gw-unix-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let sock = dir.join("rpc.sock");
        let _ = std::fs::remove_file(&sock);
        let listener = tokio::net::UnixListener::bind(&sock).unwrap();
        tokio::spawn(async move {
            while let Ok((mut s, _)) = listener.accept().await {
                tokio::spawn(async move {
                    let (mut r, mut w) = s.split();
                    let _ = tokio::io::copy(&mut r, &mut w).await;
                });
            }
        });
        let (addr, g) = gateway(0).await;
        *g.config.upstream.write().unwrap() = Some(Upstream::Unix(sock.clone()));
        let mut s = TcpStream::connect(addr).await.unwrap();
        s.write_all(format!("KVRPC/1 {}\n", any_peer_ticket()).as_bytes())
            .await
            .unwrap();
        let mut ok = [0u8; 3];
        s.read_exact(&mut ok).await.unwrap();
        assert_eq!(&ok, b"OK\n");
        s.write_all(b"graph").await.unwrap();
        let mut back = [0u8; 5];
        s.read_exact(&mut back).await.unwrap();
        assert_eq!(&back, b"graph");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn a_ping_never_touches_rpc_server() {
        let (port, count) = echo_upstream().await;
        let (addr, _) = gateway(port).await;
        let mut s = TcpStream::connect(addr).await.unwrap();
        s.write_all(format!("KVRPC/1-PING {}\n", any_peer_ticket()).as_bytes())
            .await
            .unwrap();
        assert_eq!(first_line(&mut s).await, "PONG\n");
        tokio::time::sleep(Duration::from_millis(50)).await;
        assert_eq!(count.load(Ordering::SeqCst), 0);
    }

    #[tokio::test]
    async fn every_refusal_delivers_zero_bytes_upstream() {
        let (port, count) = echo_upstream().await;
        let (addr, _) = gateway(port).await;
        let v = vectors();
        let refused: Vec<String> = v["cases"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|c| c["expect"] != "ok")
            .map(|c| c["ticket"].as_str().unwrap().to_string())
            .collect();
        assert!(refused.len() >= 7);
        // The budget would block 127.0.0.1 partway; stay under it.
        for ticket in refused.iter().take((REFUSAL_BUDGET - 1) as usize) {
            let mut s = TcpStream::connect(addr).await.unwrap();
            // Bytes after the newline must never reach rpc-server either.
            s.write_all(format!("KVRPC/1 {ticket}\nsmuggled").as_bytes())
                .await
                .unwrap();
            assert_eq!(first_line(&mut s).await, "NO refused\n", "{ticket}");
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
        assert_eq!(
            count.load(Ordering::SeqCst),
            0,
            "a refused connection reached rpc-server"
        );
    }

    #[tokio::test]
    async fn an_unknown_protocol_is_refused() {
        let (port, _) = echo_upstream().await;
        let (addr, _) = gateway(port).await;
        let mut s = TcpStream::connect(addr).await.unwrap();
        s.write_all(format!("HELLO {}\n", any_peer_ticket()).as_bytes())
            .await
            .unwrap();
        assert_eq!(first_line(&mut s).await, "NO refused\n");
    }

    #[tokio::test]
    async fn an_overlong_preamble_is_dropped_without_an_answer() {
        let (port, count) = echo_upstream().await;
        let (addr, _) = gateway(port).await;
        let mut s = TcpStream::connect(addr).await.unwrap();
        let _ = s.write_all(&vec![b'A'; MAX_PREAMBLE_BYTES + 10]).await;
        assert_eq!(first_line(&mut s).await, "");
        assert_eq!(count.load(Ordering::SeqCst), 0);
    }

    #[tokio::test]
    async fn rpc_server_down_is_a_refusal_not_an_ok() {
        let (addr, _) = gateway(0).await;
        let mut s = TcpStream::connect(addr).await.unwrap();
        s.write_all(format!("KVRPC/1 {}\n", any_peer_ticket()).as_bytes())
            .await
            .unwrap();
        assert_eq!(first_line(&mut s).await, "NO refused\n");
    }

    #[tokio::test]
    async fn an_address_that_spends_its_budget_is_dropped_unread() {
        let (port, _) = echo_upstream().await;
        let (addr, g) = gateway(port).await;
        for _ in 0..REFUSAL_BUDGET {
            let mut s = TcpStream::connect(addr).await.unwrap();
            s.write_all(b"KVRPC/1 not-a-ticket\n").await.unwrap();
            assert_eq!(first_line(&mut s).await, "NO refused\n");
        }
        assert!(g.blocked("127.0.0.1".parse().unwrap()));
        // Now even a valid ticket gets no answer at all.
        let mut s = TcpStream::connect(addr).await.unwrap();
        let _ = s
            .write_all(format!("KVRPC/1 {}\n", any_peer_ticket()).as_bytes())
            .await;
        assert_eq!(first_line(&mut s).await, "");
    }
}

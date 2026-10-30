//! `knaix device bridge`: the half of a Linux device that lives inside the
//! sandbox with rpc-server.
//!
//! On Linux the sandbox gives rpc-server a network namespace of its own, with
//! nothing in it but loopback, so it cannot reach anything: not the tailnet,
//! not the internet, not the owner's LAN. That also means the gateway, which
//! listens on the tailnet in the host's namespace, cannot reach rpc-server's
//! loopback port. This bridge closes that gap through the filesystem: it starts
//! rpc-server on the namespace's loopback, listens on a Unix socket in a
//! directory bound into the sandbox, and pipes each connection on the socket to
//! rpc-server. The gateway connects to the socket.
//!
//! macOS does not need it: `sandbox-exec` can restrict rpc-server to loopback
//! directly, so the gateway reaches it over TCP.
//!
//! Started by the device process, never by hand. It parses its own arguments
//! and runs before the rest of the CLI, so it reads no configuration and makes
//! no network calls of its own:
//!
//! ```text
//! knaix device bridge --socket <path> --port <n> -- <rpc-server> <args...>
//! ```

use std::path::PathBuf;

#[cfg_attr(not(unix), allow(dead_code))]
pub struct Args {
    pub socket: PathBuf,
    pub port: u16,
    pub program: String,
    pub program_args: Vec<String>,
}

/// Parse `--socket <path> --port <n> -- <program> <args...>`.
#[cfg_attr(not(unix), allow(dead_code))]
pub fn parse(args: &[String]) -> Result<Args, String> {
    let mut socket = None;
    let mut port = None;
    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--socket" => {
                socket = args.get(i + 1).map(PathBuf::from);
                i += 2;
            }
            "--port" => {
                port = args.get(i + 1).and_then(|p| p.parse::<u16>().ok());
                i += 2;
            }
            "--" => {
                i += 1;
                break;
            }
            other => return Err(format!("unexpected argument {other}")),
        }
    }
    let rest = &args[i.min(args.len())..];
    let (Some(socket), Some(port), Some(program)) = (socket, port, rest.first()) else {
        return Err(
            "usage: knaix device bridge --socket <path> --port <n> -- <program> <args...>".into(),
        );
    };
    Ok(Args {
        socket,
        port,
        program: program.clone(),
        program_args: rest[1..].to_vec(),
    })
}

/// Whether this command line is the bridge, checked before clap sees it.
pub fn is_bridge(argv: &[String]) -> bool {
    argv.get(1).map(String::as_str) == Some("device")
        && argv.get(2).map(String::as_str) == Some("bridge")
}

#[cfg(unix)]
pub async fn entry(argv: Vec<String>) -> std::process::ExitCode {
    use std::net::Ipv4Addr;
    use std::time::Duration;
    use tokio::net::{TcpStream, UnixListener};
    let args = match parse(&argv[3..]) {
        Ok(a) => a,
        Err(e) => {
            eprintln!("{e}");
            return std::process::ExitCode::from(2);
        }
    };
    let mut child = match tokio::process::Command::new(&args.program)
        .args(&args.program_args)
        .stdin(std::process::Stdio::null())
        .kill_on_drop(true)
        .spawn()
    {
        Ok(c) => c,
        Err(e) => {
            eprintln!("bridge: could not start {}: {e}", args.program);
            return std::process::ExitCode::from(1);
        }
    };
    // The socket appears only once rpc-server answers, so the device process
    // can take "the socket accepts" to mean "rpc-server is up".
    let mut ready = false;
    for _ in 0..240 {
        if let Ok(Some(_)) = child.try_wait() {
            break;
        }
        if TcpStream::connect((Ipv4Addr::LOCALHOST, args.port))
            .await
            .is_ok()
        {
            ready = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(250)).await;
    }
    if !ready {
        eprintln!("bridge: rpc-server did not start listening");
        let _ = child.kill().await;
        return std::process::ExitCode::from(1);
    }
    let _ = std::fs::remove_file(&args.socket);
    let listener = match UnixListener::bind(&args.socket) {
        Ok(l) => l,
        Err(e) => {
            eprintln!("bridge: could not listen on {}: {e}", args.socket.display());
            let _ = child.kill().await;
            return std::process::ExitCode::from(1);
        }
    };
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(&args.socket, std::fs::Permissions::from_mode(0o600));
    }
    let port = args.port;
    let accept = async move {
        loop {
            let Ok((mut conn, _)) = listener.accept().await else {
                continue;
            };
            tokio::spawn(async move {
                if let Ok(mut upstream) = TcpStream::connect((Ipv4Addr::LOCALHOST, port)).await {
                    let _ = upstream.set_nodelay(true);
                    let _ = tokio::io::copy_bidirectional(&mut conn, &mut upstream).await;
                }
            });
        }
    };
    let mut term = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate()).ok();
    let code = tokio::select! {
        status = child.wait() => status.ok().and_then(|s| s.code()).unwrap_or(1),
        _ = accept => 1,
        _ = async { match term.as_mut() { Some(t) => { t.recv().await; } None => std::future::pending::<()>().await } } => {
            let _ = child.kill().await;
            0
        }
    };
    let _ = std::fs::remove_file(&args.socket);
    std::process::ExitCode::from(code.clamp(0, 255) as u8)
}

#[cfg(not(unix))]
pub async fn entry(_argv: Vec<String>) -> std::process::ExitCode {
    eprintln!("knaix device bridge runs on Linux only.");
    std::process::ExitCode::from(1)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[cfg(unix)]
    use std::time::Duration;

    fn s(v: &[&str]) -> Vec<String> {
        v.iter().map(|x| x.to_string()).collect()
    }

    #[test]
    fn arguments_parse_and_everything_after_the_separator_is_the_program() {
        let a = parse(&s(&[
            "--socket",
            "/r/rpc.sock",
            "--port",
            "4100",
            "--",
            "/i/ggml-rpc-server",
            "--host",
            "127.0.0.1",
        ]))
        .unwrap();
        assert_eq!(a.socket, PathBuf::from("/r/rpc.sock"));
        assert_eq!(a.port, 4100);
        assert_eq!(a.program, "/i/ggml-rpc-server");
        assert_eq!(a.program_args, s(&["--host", "127.0.0.1"]));
    }

    #[test]
    fn missing_pieces_are_refused() {
        assert!(parse(&s(&["--socket", "/r", "--", "x"])).is_err());
        assert!(parse(&s(&["--socket", "/r", "--port", "1"])).is_err());
        assert!(parse(&s(&["--bogus"])).is_err());
    }

    #[test]
    fn only_the_bridge_command_line_is_taken_early() {
        assert!(is_bridge(&s(&[
            "knaix", "device", "bridge", "--socket", "x"
        ])));
        assert!(!is_bridge(&s(&["knaix", "device", "status"])));
        assert!(!is_bridge(&s(&["knaix"])));
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn the_socket_reaches_the_program_on_loopback() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        // A stand-in for rpc-server: an echo server on loopback, started by the
        // test rather than the bridge, then the bridge pointed at it with a
        // program that just stays alive.
        let echo = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = echo.local_addr().unwrap().port();
        tokio::spawn(async move {
            while let Ok((mut s, _)) = echo.accept().await {
                tokio::spawn(async move {
                    let (mut r, mut w) = s.split();
                    let _ = tokio::io::copy(&mut r, &mut w).await;
                });
            }
        });
        let dir = std::env::temp_dir().join(format!("knaix-bridge-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let sock = dir.join("rpc.sock");
        let argv = s(&[
            "knaix",
            "device",
            "bridge",
            "--socket",
            sock.to_str().unwrap(),
            "--port",
            &port.to_string(),
            "--",
            "sleep",
            "30",
        ]);
        let bridge = tokio::spawn(entry(argv));
        let mut conn = None;
        for _ in 0..100 {
            if let Ok(c) = tokio::net::UnixStream::connect(&sock).await {
                conn = Some(c);
                break;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        let mut conn = conn.expect("the bridge never listened");
        conn.write_all(b"tensor").await.unwrap();
        let mut back = [0u8; 6];
        conn.read_exact(&mut back).await.unwrap();
        assert_eq!(&back, b"tensor");
        bridge.abort();
        let _ = std::fs::remove_dir_all(&dir);
    }
}

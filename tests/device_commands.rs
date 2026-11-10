//! `knaix device` commands, run the way a user runs them.
//!
//! Through the binary rather than in-process, for safety as much as fidelity:
//! `leave` and `rekey` stop services by asking launchd or systemd by name. With
//! HOME a scratch directory and PATH empty, neither can be found, so no test can
//! stop a real device on the machine running it.
//!
//! Unix only: device commands are refused on Windows, and the stand-in for a
//! running device is a `sleep` process.
#![cfg(unix)]

use serde_json::{json, Value};
use std::fs;
use std::io::{BufRead, BufReader, Read, Write};
use std::net::TcpListener;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::sync::{Arc, Mutex};
use std::time::{SystemTime, UNIX_EPOCH};

const DEV: &str = "88888888-8888-4888-8888-888888888888";
/// The ticket key in the RPC tunnel test vectors.
const KEY_ID: &str = "c1d40ab1863aca6e";
const KEY_PEM: &str = "-----BEGIN PUBLIC KEY-----\nMCowBQYDK2VwAyEAt6E/WRc+E5MpT+ohkHoqZeZG0Y3Hv1hE7bAlp5ykVFY=\n-----END PUBLIC KEY-----\n";

fn now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_secs()
}

fn scratch_home(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("knaix-dev-{name}-{}", std::process::id()));
    let _ = fs::remove_dir_all(&dir);
    fs::create_dir_all(dir.join(".knaix/device")).unwrap();
    fs::create_dir_all(dir.join("empty-path")).unwrap();
    dir
}

fn knaix(home: &Path) -> Command {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_knaix"));
    cmd.env("HOME", home)
        .env("PATH", home.join("empty-path"))
        .env_remove("KNAIX_TOKEN")
        .env_remove("KNAIX_API_URL")
        .env_remove("XDG_CONFIG_HOME")
        .env("KNAIX_NO_UPDATE_CHECK", "1")
        .env("NO_COLOR", "1")
        .stdin(Stdio::null());
    cmd
}

fn run(cmd: &mut Command) -> (i32, String, String) {
    let Output {
        status,
        stdout,
        stderr,
    } = cmd.output().expect("failed to run knaix");
    (
        status.code().expect("killed by a signal"),
        String::from_utf8_lossy(&stdout).into_owned(),
        String::from_utf8_lossy(&stderr).into_owned(),
    )
}

/// A control plane answering `"METHOD /path"` routes; anything else is a 404.
/// Records each request as `"METHOD /path <authorization>"`.
fn api(routes: Vec<(&'static str, u16, Value)>) -> (String, Arc<Mutex<Vec<String>>>) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    let calls = Arc::new(Mutex::new(Vec::new()));
    let log = calls.clone();
    std::thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(mut stream) = stream else { continue };
            let mut reader = BufReader::new(stream.try_clone().unwrap());
            let mut line = String::new();
            reader.read_line(&mut line).unwrap();
            let route = line.split(' ').take(2).collect::<Vec<_>>().join(" ");
            let (mut auth, mut length) = (String::new(), 0usize);
            loop {
                let mut header = String::new();
                reader.read_line(&mut header).unwrap();
                let header = header.trim_end();
                if header.is_empty() {
                    break;
                }
                let (name, value) = header.split_once(": ").unwrap_or((header, ""));
                match name.to_ascii_lowercase().as_str() {
                    "authorization" => auth = value.to_string(),
                    "content-length" => length = value.parse().unwrap(),
                    _ => {}
                }
            }
            let mut body = vec![0u8; length];
            reader.read_exact(&mut body).unwrap();
            log.lock()
                .unwrap()
                .push(format!("{route} {auth}").trim_end().to_string());
            let (status, reply) = routes
                .iter()
                .find(|(r, _, _)| *r == route)
                .map(|(_, s, b)| (*s, b.to_string()))
                .unwrap_or((404, "{}".into()));
            let _ = write!(
                stream,
                "HTTP/1.1 {status} X\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{reply}",
                reply.len()
            );
        }
    });
    (url, calls)
}

/// Enrol the scratch machine with the control plane at `api_url`.
fn enrol(home: &Path, api_url: &str, overrides: Value) {
    let mut record = json!({
        "device_id": DEV,
        "token": "kvd_secret",
        "name": "studio",
        "api_url": api_url,
        "ticket_key_id": "0ld0ld0ld0ld0ld0",
        "ticket_key_pem": "pem",
        "gateway_port": 47100,
        "backend": "metal",
        "variant": "macos-arm64",
        "compute_tag": "tag:c-acme",
        "joined_tailnet_by_knaix": true,
        "enrolled_at": 1_793_000_000u64,
    });
    record
        .as_object_mut()
        .unwrap()
        .extend(overrides.as_object().unwrap().clone());
    fs::write(home.join(".knaix/device.json"), record.to_string()).unwrap();
}

fn record(home: &Path) -> Option<Value> {
    let raw = fs::read_to_string(home.join(".knaix/device.json")).ok()?;
    Some(serde_json::from_str(&raw).unwrap())
}

/// A process standing in for `knaix device run`, recorded as the running
/// device, with `runtime` laid over what it reports. Reaped when it exits, so a
/// stopped one stops counting as alive, and killed when the test ends, so it
/// cannot hold a pipe another test is reading.
fn running_device(home: &Path, runtime: Value) -> Running {
    let child = Command::new("/bin/sleep").arg("30").spawn().unwrap();
    let pid = child.id();
    let child = Arc::new(Mutex::new(child));
    let reaper = child.clone();
    std::thread::spawn(move || {
        while matches!(reaper.lock().unwrap().try_wait(), Ok(None)) {
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
    });
    let mut rt = json!({
        "pid": pid,
        "started_at": now() - 60,
        "gateway": "100.64.0.9:47100",
        "rpc_port": 50123,
        "rpc_server_running": true,
        "sandbox": "sandbox-exec",
        "last_heartbeat_at": now() - 12,
        "last_heartbeat_ok": true,
        "last_error": null,
        "warnings": [],
        "server_ticket_key_id": null,
        "usable_memory_bytes": 17_179_869_184u64,
    });
    rt.as_object_mut()
        .unwrap()
        .extend(runtime.as_object().unwrap().clone());
    fs::write(home.join(".knaix/device/runtime.json"), rt.to_string()).unwrap();
    Running(pid, child)
}

struct Running(u32, Arc<Mutex<std::process::Child>>);

impl Drop for Running {
    // Through the handle, not the pid: once reaped, the pid may be another
    // process's, and the handle knows not to signal it.
    fn drop(&mut self) {
        let _ = self.1.lock().unwrap().kill();
    }
}

fn alive(pid: u32) -> bool {
    unsafe { libc::kill(pid as i32, 0) == 0 }
}

fn wait_dead(pid: u32) -> bool {
    (0..50).any(|_| {
        std::thread::sleep(std::time::Duration::from_millis(20));
        !alive(pid)
    })
}

#[test]
fn status_of_a_machine_that_is_not_a_device_says_how_to_make_it_one() {
    let home = scratch_home("status-none");
    let (code, out, _) = run(knaix(&home).args(["device", "status"]));
    assert_eq!(code, 0);
    assert!(
        out.starts_with("This machine is not a compute device."),
        "{out}"
    );

    let (_, out, _) = run(knaix(&home).args(["-o", "json", "device", "status"]));
    assert_eq!(
        serde_json::from_str::<Value>(&out).unwrap(),
        json!({"enrolled": false})
    );
}

#[test]
fn status_reports_the_running_device_and_the_accounts_view_of_it() {
    let home = scratch_home("status-running");
    let (url, calls) = api(vec![(
        "GET /api/compute/devices",
        200,
        json!({"devices": [
            {"id": "someone-else"},
            {"id": DEV, "online": true, "poolNodeIds": ["p1"], "warnings": ["llama_build_unknown"]},
        ]}),
    )]);
    enrol(&home, &url, json!({}));
    let _device = running_device(&home, json!({"server_ticket_key_id": KEY_ID}));

    let (code, out, err) = run(knaix(&home)
        .env("KNAIX_TOKEN", "sess")
        .args(["device", "status"]));

    assert_eq!(code, 0, "{err}");
    for expected in [
        &format!("studio ({DEV})"),
        "State          running",
        "Account        online",
        "Pools          in 1 pool\n",
        "Offers         16.0 GB (metal)",
        "not installed or changed; run `knaix device join`",
        "Gateway        100.64.0.9:47100",
        "rpc-server     running on loopback, sandboxed (sandbox-exec)",
        "Tailnet        tag:c-acme",
        "Ticket key     0ld0ld0ld0ld0ld0",
        &format!("The control plane now signs with key {KEY_ID}."),
        "Warning: The control plane does not know which llama.cpp build this device runs.",
    ] {
        assert!(out.contains(expected), "missing {expected:?} in:\n{out}");
    }
    assert!(out.contains("Heartbeat      ok 1"), "{out}");
    assert!(
        !out.contains("starts it again"),
        "a running device was told to start"
    );
    assert_eq!(
        *calls.lock().unwrap(),
        ["GET /api/compute/devices Bearer sess"]
    );

    let (_, out, _) = run(knaix(&home)
        .env("KNAIX_TOKEN", "sess")
        .args(["-o", "json", "device", "status"]));
    let v: Value = serde_json::from_str(&out).unwrap();
    assert_eq!(v["running"], true);
    assert_eq!(v["online"], true);
    assert_eq!(v["poolNodeIds"], json!(["p1"]));
    assert_eq!(v["pendingTicketKeyId"], KEY_ID);
    assert_eq!(v["llamaInstallVerified"], false);
    assert_eq!(v["warnings"], json!(["llama_build_unknown"]));
}

#[test]
fn status_of_a_device_whose_heartbeat_fails_says_what_went_wrong() {
    let home = scratch_home("status-failing");
    enrol(&home, "http://127.0.0.1:9", json!({"compute_tag": null}));
    let _device = running_device(
        &home,
        json!({
            "gateway": null,
            "rpc_server_running": false,
            "last_heartbeat_ok": false,
            "last_error": "Could not reach the Kovalent API",
            "warnings": ["no_gateway_endpoint"],
        }),
    );

    // No session: the account's view is left out, and the device's own warnings shown.
    let (_, out, _) = run(knaix(&home).args(["device", "status"]));

    for expected in [
        "Gateway        not listening",
        "rpc-server     not running",
        "Heartbeat      failing (last success 1",
        "Last error     Could not reach the Kovalent API",
        "Tailnet        your own tailnet",
        "Warning: The gateway is not serving, so pools cannot reach this device.",
    ] {
        assert!(out.contains(expected), "missing {expected:?} in:\n{out}");
    }
    assert!(!out.contains("Account"), "{out}");

    let (_, out, _) = run(knaix(&home).args(["-o", "json", "device", "status"]));
    let v: Value = serde_json::from_str(&out).unwrap();
    assert_eq!(v["warnings"], json!(["no_gateway_endpoint"]));
    assert_eq!(v["online"], Value::Null);
}

#[test]
fn status_of_a_stopped_device_says_how_to_start_it_unless_it_was_removed() {
    let home = scratch_home("status-stopped");
    enrol(&home, "http://127.0.0.1:9", json!({}));

    let (_, out, _) = run(knaix(&home).args(["device", "status"]));
    assert!(out.contains("State          stopped"), "{out}");
    assert!(
        out.contains("Offers         measured when running (metal)"),
        "{out}"
    );
    assert!(out.contains("device join starts it again"), "{out}");

    fs::write(home.join(".knaix/device/removed"), "1").unwrap();
    let (_, out, _) = run(knaix(&home).args(["device", "status"]));
    assert!(
        out.contains("removed from your account; run `knaix device leave`"),
        "{out}"
    );
    assert!(!out.contains("starts it again"), "{out}");
}

#[test]
fn ls_lists_the_accounts_devices_and_marks_this_one() {
    let home = scratch_home("ls");
    let (url, _) = api(vec![(
        "GET /api/compute/devices",
        200,
        json!({"devices": [
            {"id": DEV, "name": "studio", "backend": "metal", "usableMemoryBytes": 17_179_869_184u64,
             "llamaBuild": "b11669", "online": true, "poolNodeIds": ["p1", "p2"], "warnings": ["w"]},
            {"id": "other", "name": "garage", "backend": "cuda", "online": false},
        ]}),
    )]);
    enrol(&home, &url, json!({}));
    let ls = |format: &str| {
        run(knaix(&home)
            .env("KNAIX_TOKEN", "sess")
            .env("KNAIX_API_URL", &url)
            .args(["-o", format, "device", "ls"]))
    };

    let (code, out, err) = ls("text");
    assert_eq!(code, 0, "{err}");
    for expected in [
        "studio (this machine)",
        "16.0 GB",
        "b11669",
        "online",
        "garage",
        "unknown",
        "offline",
    ] {
        assert!(out.contains(expected), "missing {expected:?} in:\n{out}");
    }
    assert!(!out.contains("garage (this machine)"));

    let (_, out, _) = ls("json");
    assert_eq!(
        serde_json::from_str::<Value>(&out)
            .unwrap()
            .as_array()
            .unwrap()
            .len(),
        2
    );
}

#[test]
fn ls_with_no_devices_says_how_to_add_one_and_without_a_session_is_refused() {
    let home = scratch_home("ls-empty");
    let (url, _) = api(vec![(
        "GET /api/compute/devices",
        200,
        json!({"devices": []}),
    )]);

    let (code, out, _) = run(knaix(&home)
        .env("KNAIX_TOKEN", "sess")
        .env("KNAIX_API_URL", &url)
        .args(["device", "ls"]));
    assert_eq!(code, 0);
    assert!(out.starts_with("No compute devices yet."), "{out}");

    let (code, _, err) = run(knaix(&home)
        .env("KNAIX_API_URL", &url)
        .args(["device", "ls"]));
    assert_eq!(code, 3, "expected Auth: {err}");
}

#[test]
fn leave_stops_the_device_removes_it_and_forgets_its_token() {
    let home = scratch_home("leave");
    let route: &'static str =
        Box::leak(format!("DELETE /api/compute/devices/{DEV}").into_boxed_str());
    let (url, calls) = api(vec![(route, 204, json!({}))]);
    enrol(&home, &url, json!({}));
    let device = running_device(&home, json!({}));

    let (code, out, err) = run(knaix(&home)
        .env("KNAIX_TOKEN", "sess")
        .args(["device", "leave", "--yes"]));

    assert_eq!(code, 0, "{err}");
    assert!(
        out.contains("Stopped studio and forgot its device token and ticket key."),
        "{out}"
    );
    assert!(out.contains("Removed from your account."), "{out}");
    assert!(out.contains("tailscale logout"), "{out}");
    assert!(wait_dead(device.0), "the running device was not stopped");
    assert_eq!(record(&home), None);
    assert!(!home.join(".knaix/device/runtime.json").exists());
    assert_eq!(*calls.lock().unwrap(), [format!("{route} Bearer sess")]);
}

#[test]
fn leave_says_when_the_account_still_lists_the_device() {
    let home = scratch_home("leave-listed");
    enrol(
        &home,
        "http://127.0.0.1:9",
        json!({"joined_tailnet_by_knaix": false}),
    );
    fs::write(home.join(".knaix/device/gateway.log"), "x").unwrap();

    let (code, out, _) = run(knaix(&home).args(["device", "leave", "--yes", "--purge"]));

    assert_eq!(code, 0);
    assert!(
        out.contains("studio was not running. Forgot its device token and ticket key."),
        "{out}"
    );
    assert!(
        out.contains("It is still listed in your account (Not logged in."),
        "{out}"
    );
    assert!(!out.contains("tailscale logout"), "{out}");
    assert_eq!(record(&home), None);
    assert!(
        !home.join(".knaix/device").exists(),
        "--purge left the logs"
    );
}

#[test]
fn leave_without_a_device_has_nothing_to_do_but_purge() {
    let home = scratch_home("leave-none");
    let (code, out, _) = run(knaix(&home).args(["device", "leave"]));
    assert_eq!(
        (code, out.trim()),
        (0, "This machine is not a compute device.")
    );

    fs::create_dir_all(home.join(".knaix/llama/b1-x")).unwrap();
    let (_, out, _) = run(knaix(&home).args(["device", "leave", "--purge"]));
    assert!(
        out.contains("Removed the llama.cpp build, cache and logs."),
        "{out}"
    );
    assert!(!home.join(".knaix/llama").exists());
    assert!(!home.join(".knaix/device").exists());
}

#[test]
fn leave_and_rekey_with_no_terminal_to_confirm_on_change_nothing() {
    let home = scratch_home("unconfirmed");
    let (url, _) = api(vec![(
        "GET /api/compute/ticket-key",
        200,
        json!({"keyId": KEY_ID, "publicKeyPem": KEY_PEM}),
    )]);
    enrol(&home, &url, json!({}));

    for args in [["device", "leave"], ["device", "rekey"]] {
        let (code, _, err) = run(knaix(&home).args(args));
        assert_eq!(code, 6, "{args:?} expected Denied: {err}");
        assert!(err.contains("Pass --yes"), "{err}");
    }
    assert_eq!(record(&home).unwrap()["ticket_key_id"], "0ld0ld0ld0ld0ld0");
}

#[test]
fn rekey_pins_the_served_key_and_stops_the_device_holding_the_old_one() {
    let home = scratch_home("rekey");
    let (url, _) = api(vec![(
        "GET /api/compute/ticket-key",
        200,
        json!({"keyId": KEY_ID, "publicKeyPem": KEY_PEM}),
    )]);
    enrol(&home, &url, json!({}));
    let device = running_device(&home, json!({}));

    let (code, out, err) = run(knaix(&home).args(["device", "rekey", "--yes"]));

    assert_eq!(code, 0, "{err}");
    assert!(out.contains(&format!("Served key:  {KEY_ID}")), "{out}");
    assert!(out.contains(&format!("Pinned {KEY_ID}.")), "{out}");
    assert!(
        out.contains("Stopped the running device so it does not keep the old key."),
        "{out}"
    );
    assert!(wait_dead(device.0));
    let saved = record(&home).unwrap();
    assert_eq!(
        (
            saved["ticket_key_id"].as_str(),
            saved["ticket_key_pem"].as_str()
        ),
        (Some(KEY_ID), Some(KEY_PEM))
    );

    // Asked again, the pinned key is already the served one.
    let (code, out, _) = run(knaix(&home).args(["device", "rekey"]));
    assert_eq!(code, 0);
    assert!(out.contains("Nothing to do."), "{out}");
}

/// With a background service installed, rekeying restarts it instead of
/// stopping the device. Which manager is asked is out of reach here (PATH is
/// empty); what is checked is that the service, not a stop, is the path taken.
#[test]
fn rekey_restarts_the_background_service_rather_than_stopping_it() {
    let home = scratch_home("rekey-service");
    let (url, _) = api(vec![(
        "GET /api/compute/ticket-key",
        200,
        json!({"keyId": KEY_ID, "publicKeyPem": KEY_PEM}),
    )]);
    enrol(&home, &url, json!({}));
    let device = running_device(&home, json!({}));
    for service in [
        "Library/LaunchAgents/com.kovalent.knaix-device.plist",
        ".config/systemd/user/knaix-device.service",
    ] {
        let path = home.join(service);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, "").unwrap();
    }

    let (code, out, err) = run(knaix(&home).args(["device", "rekey", "--yes"]));

    assert_eq!(code, 0, "{err}");
    assert!(
        out.contains("Restarted the background service with the new key."),
        "{out}"
    );
    assert!(!out.contains("Stopped the running device"), "{out}");
    assert!(
        alive(device.0),
        "the service restarts the device; rekey must not stop it"
    );
}

#[test]
fn rekey_refuses_a_key_that_is_not_what_it_claims() {
    let home = scratch_home("rekey-bad");
    let (url, _) = api(vec![(
        "GET /api/compute/ticket-key",
        200,
        json!({"keyId": "deadbeefdeadbeef", "publicKeyPem": KEY_PEM}),
    )]);
    enrol(&home, &url, json!({}));

    let (code, _, err) = run(knaix(&home).args(["device", "rekey", "--yes"]));
    assert_eq!(code, 6, "{err}");
    assert!(
        err.contains(&format!("(deadbeefdeadbeef vs {KEY_ID}). Not pinning it.")),
        "{err}"
    );

    let (url, _) = api(vec![(
        "GET /api/compute/ticket-key",
        200,
        json!({"keyId": "deadbeefdeadbeef", "publicKeyPem": "not a key"}),
    )]);
    enrol(&home, &url, json!({}));
    let (code, _, err) = run(knaix(&home).args(["device", "rekey", "--yes"]));
    assert_eq!(code, 1, "{err}");
    assert!(
        err.contains("The control plane served a ticket key this device cannot use"),
        "{err}"
    );
    assert_eq!(record(&home).unwrap()["ticket_key_id"], "0ld0ld0ld0ld0ld0");

    let none = scratch_home("rekey-none");
    let (code, _, err) = run(knaix(&none).args(["device", "rekey"]));
    assert_eq!(code, 7, "{err}");
}

#[test]
fn logs_show_each_log_with_times_as_ages() {
    let home = scratch_home("logs");
    fs::write(
        home.join(".knaix/device/gateway.log"),
        format!(
            "{} refused 100.64.0.7: bad ticket\nno timestamp here\n",
            now() - 120
        ),
    )
    .unwrap();

    let (code, out, _) = run(knaix(&home).args(["device", "logs", "-n", "5"]));

    assert_eq!(code, 0);
    assert!(out.contains("Gateway "), "{out}");
    assert!(
        out.contains("2 min ago  refused 100.64.0.7: bad ticket"),
        "{out}"
    );
    assert!(out.contains("  no timestamp here"), "{out}");
    assert!(
        out.contains("rpc-server ") && out.contains("(empty)"),
        "{out}"
    );
    assert!(
        !out.contains("Service"),
        "an empty service log is left out: {out}"
    );
}

/// Install llama.cpp the way `join` leaves it: the binary and the record of
/// its hash. The tag and build are the ones pinned in `src/device/llama.rs`.
fn install_llama(home: &Path, variant: &str) {
    use sha2::{Digest, Sha256};
    use std::os::unix::fs::PermissionsExt;
    let dir = home.join(format!(".knaix/llama/b11669-{variant}"));
    fs::create_dir_all(&dir).unwrap();
    // A stand-in that exits at once: rpc-server never comes up.
    let binary = b"#!/bin/sh\nexit 1\n";
    let path = dir.join("ggml-rpc-server");
    fs::write(&path, binary).unwrap();
    fs::set_permissions(&path, fs::Permissions::from_mode(0o755)).unwrap();
    let sha: String = Sha256::digest(binary)
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect();
    let record = json!({
        "tag": "b11669",
        "build": "11669-620d222",
        "variant": variant,
        "files": [["ggml-rpc-server", sha]],
    });
    fs::write(dir.join("install.json"), record.to_string()).unwrap();
}

/// A Tailscale on the PATH, up at 127.0.0.1 with `tags`.
fn tailscale(home: &Path, tags: &[&str]) {
    use std::os::unix::fs::PermissionsExt;
    let status = json!({
        "BackendState": "Running",
        "Self": {"TailscaleIPs": ["127.0.0.1"], "Tags": tags},
    });
    let path = home.join("empty-path/tailscale");
    fs::write(&path, format!("#!/bin/sh\necho '{status}'\n")).unwrap();
    fs::set_permissions(&path, fs::Permissions::from_mode(0o755)).unwrap();
}

fn free_port() -> u16 {
    TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port()
}

#[test]
fn run_is_refused_until_each_precondition_holds() {
    let home = scratch_home("run-refused");
    let device_run = || run(knaix(&home).args(["device", "run"]));
    let refused = |want: &str, code: i32| {
        let (got, _, err) = device_run();
        assert_eq!(got, code, "{err}");
        assert!(err.contains(want), "wanted {want:?}: {err}");
    };

    refused("not a compute device. Run `knaix device join` first.", 7);

    enrol(&home, "http://127.0.0.1:9", json!({}));
    {
        let device = running_device(&home, json!({}));
        refused(
            &format!("already running on this machine (process {})", device.0),
            7,
        );
    }
    // As a device that exits leaves it.
    fs::remove_file(home.join(".knaix/device/runtime.json")).unwrap();

    enrol(
        &home,
        "http://127.0.0.1:9",
        json!({"variant": "plan9-arm64"}),
    );
    refused("does not know the llama.cpp build 'plan9-arm64'", 7);

    enrol(&home, "http://127.0.0.1:9", json!({}));
    refused("llama.cpp b11669 is not installed", 7);

    install_llama(&home, "macos-arm64");
    enrol(
        &home,
        "http://127.0.0.1:9",
        json!({"ticket_key_pem": KEY_PEM}),
    );
    refused("The pinned ticket key does not match its recorded id", 6);

    let pinned = json!({"ticket_key_id": KEY_ID, "ticket_key_pem": KEY_PEM});
    enrol(&home, "http://127.0.0.1:9", pinned.clone());
    tailscale(&home, &["tag:c-other"]);
    refused(
        "no longer on Kovalent's tailnet with its device tag (tag:c-acme)",
        7,
    );

    tailscale(&home, &["tag:c-acme"]);
    let taken = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = taken.local_addr().unwrap().port();
    let mut on_taken = pinned;
    on_taken["gateway_port"] = json!(port);
    enrol(&home, "http://127.0.0.1:9", on_taken);
    refused(&format!("Could not listen on 127.0.0.1:{port}"), 7);
}

#[test]
fn run_serves_until_the_control_plane_refuses_it_and_then_stands_down() {
    let home = scratch_home("run-removed");
    let (url, calls) = api(vec![(
        "POST /api/compute/devices/heartbeat",
        401,
        json!({}),
    )]);
    let port = free_port();
    enrol(
        &home,
        &url,
        json!({"ticket_key_id": KEY_ID, "ticket_key_pem": KEY_PEM, "gateway_port": port}),
    );
    install_llama(&home, "macos-arm64");
    tailscale(&home, &["tag:c-acme"]);
    let services = [
        home.join("Library/LaunchAgents/com.kovalent.knaix-device.plist"),
        home.join(".config/systemd/user/knaix-device.service"),
    ];
    for path in &services {
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, "").unwrap();
    }

    let (code, _, err) = run(knaix(&home).args(["device", "run"]));

    assert_eq!(code, 3, "{err}");
    assert!(
        err.contains(&format!("Gateway listening on 127.0.0.1:{port}")),
        "{err}"
    );
    assert!(err.contains("no longer recognizes this device"), "{err}");
    assert_eq!(
        *calls.lock().unwrap(),
        ["POST /api/compute/devices/heartbeat Bearer kvd_secret"]
    );
    // Removed from the account: a note for `status`, and no restart.
    let device = home.join(".knaix/device");
    assert!(device.join("removed").exists());
    let log = fs::read_to_string(device.join("gateway.log")).unwrap();
    assert!(
        log.contains("removed from the account; stopping the service"),
        "{log}"
    );
    assert!(
        !device.join("runtime.json").exists(),
        "a stopped device is not running"
    );
    assert_eq!(
        services.iter().filter(|p| p.exists()).count(),
        1,
        "this platform's service is uninstalled; the other's was never its own"
    );
}

#[test]
fn join_is_refused_until_the_session_port_cap_and_backend_are_right() {
    let home = scratch_home("join-refused");
    let (url, calls) = api(vec![]);
    let join = |args: &[&str]| {
        run(knaix(&home)
            .env("KNAIX_TOKEN", "sess")
            .env("KNAIX_API_URL", &url)
            .args(["device", "join"])
            .args(args))
    };
    let refused = |args: &[&str], want: &str, code: i32| {
        let (got, _, err) = join(args);
        assert_eq!(got, code, "{args:?}: {err}");
        assert!(err.contains(want), "{args:?} wanted {want:?}: {err}");
    };

    let (code, _, err) = run(knaix(&home)
        .env("KNAIX_API_URL", &url)
        .args(["device", "join"]));
    assert_eq!(code, 3, "{err}");
    assert!(err.contains("Not logged in"), "{err}");
    refused(&["--max-memory", "lots"], "'lots' is not a size.", 2);
    refused(&["--port", "0"], "--port must be a port number, not 0.", 2);
    refused(&["--backend", "abacus"], "Unknown backend 'abacus'.", 2);
    refused(
        &["--backend", "cuda"],
        "No NVIDIA GPU answered nvidia-smi on this machine.",
        7,
    );

    enrol(&home, &url, json!({}));
    let device = running_device(&home, json!({}));
    let (code, out, err) = join(&[]);
    assert_eq!(code, 0, "{err}");
    assert!(
        out.contains(&format!(
            "This machine is already lending itself as studio (process {})",
            device.0
        )),
        "{out}"
    );
    assert!(
        calls.lock().unwrap().is_empty(),
        "nothing asked of the account"
    );
}

/// The llama.cpp build `join` picks for this machine with nothing to probe.
fn this_machines_variant() -> &'static str {
    match (std::env::consts::OS, std::env::consts::ARCH) {
        ("macos", "aarch64") => "macos-arm64",
        ("macos", _) => "macos-x64",
        (_, "aarch64") => "ubuntu-arm64",
        _ => "ubuntu-x64",
    }
}

#[test]
fn join_runs_as_far_as_this_machine_allows() {
    let home = scratch_home("join");
    let (url, calls) = api(vec![
        (
            "POST /api/compute/devices",
            201,
            json!({
                "device": {"id": DEV},
                "token": "kvd_new",
                "ticketKey": {"keyId": KEY_ID, "publicKeyPem": KEY_PEM},
                "warnings": ["llama_build_unknown", "no_gateway_endpoint"],
            }),
        ),
        ("POST /api/compute/devices/heartbeat", 401, json!({})),
    ]);
    install_llama(&home, this_machines_variant());
    tailscale(&home, &["tag:c-acme"]);
    let port = free_port();

    let (code, out, err) = run(knaix(&home)
        .env("KNAIX_TOKEN", "sess")
        .env("KNAIX_API_URL", &url)
        .args(["device", "join", "--name", "studio", "--port"])
        .arg(port.to_string()));

    assert!(out.contains("Hardware: "), "{out}");
    assert!(out.contains("llama.cpp: 11669-620d222 verified"), "{out}");
    if cfg!(target_os = "macos") {
        // sandbox-exec is always there: on to enrolling and serving, until
        // the control plane refuses the first heartbeat.
        assert_eq!(code, 3, "{err}");
        for expected in [
            "Sandbox: sandbox-exec".to_string(),
            format!("Tailnet: 127.0.0.1:{port} (tag:c-acme)"),
            format!("Device: enrolled as studio (ticket key {KEY_ID})"),
            "Warning: The control plane does not know which llama.cpp build".into(),
            "studio is lending itself to your pools.".into(),
        ] {
            assert!(out.contains(&expected), "missing {expected:?} in:\n{out}");
        }
        assert!(!out.contains("gateway is not serving"), "{out}");
        assert_eq!(
            *calls.lock().unwrap(),
            [
                "POST /api/compute/devices Bearer sess",
                "POST /api/compute/devices/heartbeat Bearer kvd_new",
            ]
        );
        assert_eq!(record(&home).unwrap()["device_id"], DEV);
    } else {
        // With PATH empty there is no systemd-run or bwrap to sandbox
        // rpc-server, and nothing is enrolled without one.
        assert_eq!(code, 7, "{err}");
        assert!(
            err.contains("No sandbox is available for rpc-server"),
            "{err}"
        );
        assert!(calls.lock().unwrap().is_empty());
        assert_eq!(record(&home), None);
    }
}

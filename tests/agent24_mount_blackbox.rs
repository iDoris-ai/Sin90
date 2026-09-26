//! Real-`agent24d` mount verification (design `DESIGN-LIFEOS.md` §6, A1/A2/A3/A11
//! — the gap `docs/STATUS.md`/`tests/standalone_blackbox.rs` left open since M0).
//!
//! Everything before this file exercised Sin90's own axum `Router` in-process
//! (`http::tests`) or a real Sin90 subprocess standing entirely on its own
//! (`standalone_blackbox.rs`). Neither ever put a real, already-built `agent24d`
//! binary in front of a real, already-built `sin90` binary — so nothing had
//! actually exercised `adapter_agent24`'s handshake against the real kernel, or
//! proven Agent24's constrained proxy forwards Sin90's routes/events unchanged.
//!
//! Requires a sibling Agent24 checkout (`AGENT24_CHECKOUT` env var, default
//! `../Agent24`) with a workspace at `<checkout>/rust` — this crate cannot
//! declare that as a normal Cargo dependency (it is a separate repo, and
//! `agent24d` is a binary, not a library `iDoris-ai/Sin90` should depend on).
//! `#[ignore]`d so `cargo test` stays green with no such checkout present
//! (CI, a clone with only this repo, another dev's machine); run explicitly:
//!
//!   cargo test --test agent24_mount_blackbox -- --ignored --test-threads=1
#![cfg(unix)]

use std::io::{BufReader, Read, Write};
use std::net::TcpStream;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use chrono::Timelike;

/// Locate the Agent24 checkout. `../Agent24` is this environment's actual
/// layout (both repos are sibling directories under the same parent) — an
/// env var override exists for anyone whose layout differs, not because this
/// default is a guess.
fn agent24_checkout() -> Option<PathBuf> {
    let dir = std::env::var("AGENT24_CHECKOUT")
        .map(PathBuf::from)
        .unwrap_or_else(|_| {
            PathBuf::from(env!("CARGO_MANIFEST_DIR"))
                .parent()
                .unwrap()
                .join("Agent24")
        });
    dir.join("rust/Cargo.toml").is_file().then_some(dir)
}

/// Build `agent24d` in the Agent24 checkout and return the binary path.
/// Deliberately NOT `cargo build` inside a `build.rs` or at Sin90's own
/// workspace level — this is one integration test's setup step, not
/// something every `cargo build` of this crate should pay for.
fn build_agent24d(checkout: &Path) -> PathBuf {
    let status = Command::new("cargo")
        .args(["build", "-p", "agent24d", "--bin", "agent24d"])
        .current_dir(checkout.join("rust"))
        .status()
        .expect("could not run cargo build for agent24d");
    assert!(status.success(), "cargo build -p agent24d failed");
    let bin = checkout.join("rust/target/debug/agent24d");
    assert!(
        bin.is_file(),
        "expected {} to exist after build",
        bin.display()
    );
    bin
}

/// Build Sin90 itself with the `test-hooks` feature on, for
/// `kernel_clients_roundtrip` — the ONLY test in this file that needs the
/// `POST /debug/kernel-roundtrip` route (`src/http/kernel_roundtrip.rs`),
/// which is compiled in exclusively under that feature (off by default,
/// `Cargo.toml`) so a real ship build never carries it.
///
/// Deliberately built into its OWN `--target-dir`, not the default
/// `target/debug` `CARGO_BIN_EXE_sin90` already points at — that path is
/// shared with `sin90_mounts_under_a_real_agent24_daemon` (which uses the
/// plain, `test-hooks`-off binary via `CARGO_BIN_EXE_sin90`), and rebuilding
/// the SAME path with a different feature set would overwrite that other
/// test's binary out from under it (Cargo does not give a bin target a
/// feature-specific filename) — a real, if easy to miss, source of flaky
/// cross-test interference within one `--test-threads=1` run.
fn build_sin90_with_test_hooks() -> PathBuf {
    let manifest_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let target_dir = manifest_dir.join("target/test-hooks-debug");
    let status = Command::new("cargo")
        .args([
            "build",
            "--bin",
            "sin90",
            "--features",
            "test-hooks",
            "--target-dir",
        ])
        .arg(&target_dir)
        .current_dir(&manifest_dir)
        .status()
        .expect("could not run cargo build for sin90 (test-hooks)");
    assert!(status.success(), "cargo build --features test-hooks failed");
    let bin = target_dir.join("debug/sin90");
    assert!(
        bin.is_file(),
        "expected {} to exist after build",
        bin.display()
    );
    bin
}

/// `/tmp` directly, NOT `std::env::temp_dir()` — on macOS the latter resolves
/// through `/var/folders/<hash>/<hash>/T`, and the daemon's out-of-process
/// callback socket lives at `<home>/.agent24/run/<pid>/callback.sock`. `sockaddr_un`
/// caps the whole path at ~103 usable bytes; the `/var/folders` prefix alone is
/// long enough to blow that budget before this function adds anything, and the
/// daemon then degrades every out-of-process module with `"callback sockets ...
/// would be longer than 103 bytes"` — not a Sin90 bug, a test fixture picking too
/// long a home directory (mirrors `agent24d/tests/me3f_blackbox.rs`'s own
/// `tempdir_in("/tmp")`, which exists for the identical reason).
fn tmp_home(tag: &str) -> PathBuf {
    let dir = Path::new("/tmp").join(format!("sin90-a24mount-{tag}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

/// Install Sin90 as a real out-of-process package: an EXACT byte copy of
/// Sin90's own `domain-os.yml` (the kernel's `manifest_digest` check compares
/// raw file bytes — `agent24-os-packages::discovery`, `agent24-os-proto`'s
/// `initialize.rs:269` — so a re-serialized or hand-typed copy that merely
/// LOOKS the same would fail the digest match this test exists to catch) plus
/// the just-built `sin90` binary at the exact relative path the manifest's
/// `spawn.command: bin/sin90` names (`agent24-os-proto::launch` resolves a
/// command containing `/` relative to the package directory, never PATH).
fn install_sin90(packages_root: &Path, sin90_bin: &Path) {
    let dir = packages_root.join("sin90");
    std::fs::create_dir_all(dir.join("bin")).unwrap();
    std::fs::write(
        dir.join("domain-os.yml"),
        include_bytes!("../domain-os.yml"),
    )
    .unwrap();
    std::fs::copy(sin90_bin, dir.join("bin/sin90")).unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mut perm = std::fs::metadata(dir.join("bin/sin90"))
            .unwrap()
            .permissions();
        perm.set_mode(0o755);
        std::fs::set_permissions(dir.join("bin/sin90"), perm).unwrap();
    }
}

/// `daemon_token` is Agent24's OWN kernel bearer token (`server.rs::auth` —
/// every `/api/v1/*` route except `GET /api/v1/health` requires it, domain-OS
/// routes included: `mount_all`'s routes fold in BEFORE `.layer(auth)`, so the
/// layer covers them too). It is unrelated to, and does not substitute for,
/// Sin90's OWN `x-sin90-actor-key` gate (`actor_key`) — the kernel's proxy
/// (`agent24-os-proto::proxy::forward`) strips `Authorization` before handing
/// the request to Sin90, so a caller through the real proxy needs BOTH: the
/// daemon token to get past the kernel, and (for a gated Sin90 route) the
/// actor key to get past Sin90 itself.
fn http_get(port: u16, daemon_token: Option<&str>, path: &str) -> Option<(u16, String)> {
    http_call(port, "GET", path, daemon_token, None, None)
}

fn http_call(
    port: u16,
    method: &str,
    path: &str,
    daemon_token: Option<&str>,
    actor_key: Option<&str>,
    body: Option<&str>,
) -> Option<(u16, String)> {
    let mut s = TcpStream::connect(("127.0.0.1", port)).ok()?;
    s.set_read_timeout(Some(Duration::from_secs(10))).unwrap();
    let auth = daemon_token
        .map(|t| format!("authorization: Bearer {t}\r\n"))
        .unwrap_or_default();
    let actor = actor_key
        .map(|k| format!("x-sin90-actor-key: {k}\r\n"))
        .unwrap_or_default();
    let body = body.unwrap_or("");
    write!(
        s,
        "{method} {path} HTTP/1.1\r\nhost: x\r\n{auth}{actor}content-type: application/json\r\n\
         content-length: {}\r\nconnection: close\r\n\r\n{body}",
        body.len()
    )
    .ok()?;
    let mut raw = String::new();
    s.read_to_string(&mut raw).ok()?;
    let status = raw.split(' ').nth(1)?.parse().ok()?;
    let resp_body = raw.split_once("\r\n\r\n").map(|(_, b)| b.to_owned())?;
    Some((status, resp_body))
}

/// Graceful-first on every exit path (unwind included), same reasoning as
/// Agent24's own `me3f_blackbox.rs::Running` — a plain `Child` drop does not
/// terminate the OS process, and an unconditional SIGKILL here has already
/// been observed (in that file's own history) to orphan a module blocked in
/// `accept()`.
struct Running(std::process::Child);

impl Drop for Running {
    fn drop(&mut self) {
        #[allow(clippy::cast_possible_wrap)]
        if let Some(pid) = rustix::process::Pid::from_raw(self.0.id() as i32) {
            let _ = rustix::process::kill_process(pid, rustix::process::Signal::Term);
            let by = Instant::now() + Duration::from_secs(10);
            while self.0.try_wait().is_ok_and(|s| s.is_none()) && Instant::now() < by {
                std::thread::sleep(Duration::from_millis(20));
            }
        }
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

struct Daemon {
    #[allow(dead_code)]
    run: Running,
    port: u16,
    token: String,
    /// Both streams, continuously drained — unlike a one-shot ready-line
    /// read, this test also needs to find a line Sin90 itself prints deep
    /// into the run (see `read_actor_key`), not just at startup.
    stdout: Arc<Mutex<Vec<String>>>,
    stderr: Arc<Mutex<Vec<String>>>,
}

impl Daemon {
    fn combined_log(&self) -> String {
        let out = self.stdout.lock().unwrap().join("\n");
        let err = self.stderr.lock().unwrap().join("\n");
        format!("--- stdout ---\n{out}\n--- stderr ---\n{err}")
    }

    /// Sin90 persists its actor keys to `<A24_DATA_DIR>/actor-keys.json`
    /// (Agent24's module launch passes only a fixed env allowlist, so
    /// `SIN90_*` env keys can't reach a mounted module). The data dir lives
    /// under this test's isolated `HOME`, so search there rather than
    /// hard-coding Agent24's layout. Also asserts the key never reached the
    /// daemon log — Agent24 re-logs every module output line, so a printed
    /// key is readable by anyone with log access (Codex 2026-09-22, Medium).
    fn read_actor_key(&self, home: &Path, which: &str, timeout: Duration) -> String {
        let deadline = Instant::now() + timeout;
        let path = loop {
            if let Some(p) = find_file(home, "actor-keys.json") {
                break p;
            }
            assert!(
                Instant::now() < deadline,
                "sin90 never created actor-keys.json under {}; log:\n{}",
                home.display(),
                self.combined_log()
            );
            std::thread::sleep(Duration::from_millis(50));
        };
        let keys: serde_json::Value =
            serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
        let key = keys[which].as_str().unwrap().to_owned();
        assert!(
            !self.combined_log().contains(&key),
            "the raw {which} key leaked into the daemon log"
        );
        key
    }
}

fn find_file(dir: &Path, name: &str) -> Option<std::path::PathBuf> {
    for entry in std::fs::read_dir(dir).ok()?.flatten() {
        let path = entry.path();
        if path.file_name().is_some_and(|n| n == name) {
            return Some(path);
        }
        if entry.file_type().is_ok_and(|t| t.is_dir()) {
            if let Some(found) = find_file(&path, name) {
                return Some(found);
            }
        }
    }
    None
}

fn drain_into(mut reader: impl std::io::Read + Send + 'static, sink: Arc<Mutex<Vec<String>>>) {
    std::thread::spawn(move || {
        let mut buf = BufReader::new(&mut reader);
        let mut line = String::new();
        loop {
            line.clear();
            match std::io::BufRead::read_line(&mut buf, &mut line) {
                Ok(0) | Err(_) => break,
                Ok(_) => sink.lock().unwrap().push(line.trim_end().to_owned()),
            }
        }
    });
}

/// `extra_env` — T3.5.1: lets a caller set `A24_SCHEDULER_TICK_SECS=1`
/// (`agent24d/src/server.rs`'s own env read, default 10s) so a real tick
/// reaches a due cron slot within the test's own patience, the same knob
/// Agent24's own `me4_scheduler_blackbox.rs::start` sets for the identical
/// reason. Every existing caller in this file passes `&[]`.
fn start_daemon(home: &Path, agent24d_bin: &Path, extra_env: &[(&str, &str)]) -> Daemon {
    let mut child = Command::new(agent24d_bin)
        .env_clear()
        .env("PATH", std::env::var_os("PATH").unwrap_or_default())
        .env("HOME", home)
        .envs(extra_env.iter().copied())
        .args(["serve", "--port", "0"])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap_or_else(|e| panic!("could not spawn {}: {e}", agent24d_bin.display()));
    let mut stdout = child.stdout.take().unwrap();
    let stderr = child.stderr.take().unwrap();
    let run = Running(child);

    // The ready line is the FIRST line of stdout — read it directly, then
    // hand the rest of the same stream to `drain_into` for continuous
    // capture (the key-leak assertion in `read_actor_key` needs it).
    let mut first_line = Vec::new();
    loop {
        let mut byte = [0u8; 1];
        match stdout.read(&mut byte) {
            Ok(1) => {
                if byte[0] == b'\n' {
                    break;
                }
                first_line.push(byte[0]);
            }
            _ => panic!("daemon closed stdout before printing a ready line"),
        }
    }
    let ready: serde_json::Value =
        serde_json::from_slice(&first_line).expect("the ready line must be JSON");

    let stdout_lines = Arc::new(Mutex::new(Vec::new()));
    let stderr_lines = Arc::new(Mutex::new(Vec::new()));
    drain_into(stdout, stdout_lines.clone());
    drain_into(stderr, stderr_lines.clone());

    Daemon {
        run,
        port: u16::try_from(ready["port"].as_u64().unwrap()).unwrap(),
        token: ready["token"].as_str().unwrap().to_owned(),
        stdout: stdout_lines,
        stderr: stderr_lines,
    }
}

fn os_list_entry(d: &Daemon, name: &str) -> serde_json::Value {
    let (status, body) =
        http_get(d.port, Some(&d.token), "/api/v1/os").expect("the daemon answered /api/v1/os");
    assert_eq!(status, 200, "{body}");
    let list: serde_json::Value = serde_json::from_str(&body).unwrap();
    list["modules"]
        .as_array()
        .unwrap()
        .iter()
        .find(|m| m["name"] == name)
        .cloned()
        .unwrap_or_else(|| panic!("{name} not in module list: {body}"))
}

// ---------------------------------------------------------------------------
// T3.5.1 — M3 real-mount acceptance harness additions. Everything below this
// point is shared by `routine_m3_real_mount_acceptance` only; the two tests
// above it predate T3.5.1 and are left untouched other than `start_daemon`
// growing an `extra_env` parameter (every existing call site now passes
// `&[]`, unchanged behavior).
// ---------------------------------------------------------------------------

/// Same two-step race `sin90_mounts_under_a_real_agent24_daemon` /
/// `kernel_clients_roundtrip` each retry around inline: `os list` can report
/// `mounted` slightly before the module has actually finished handshaking
/// and is ready to serve a proxied request. Factored out here (rather than
/// inlined a third time) because `routine_m3_real_mount_acceptance` calls it
/// after EVERY one of its several real daemon (re)starts.
fn wait_for_sin90_ready(d: &Daemon, timeout: Duration) {
    let deadline = Instant::now() + timeout;
    loop {
        if os_list_entry(d, "sin90")["state"] == "mounted" {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "sin90 never reached state \"mounted\"; daemon log:\n{}",
            d.combined_log()
        );
        std::thread::sleep(Duration::from_millis(100));
    }
    let deadline = Instant::now() + timeout;
    loop {
        if let Some((status, body)) = http_get(d.port, Some(&d.token), "/api/v1/sin90/today") {
            if status == 200 {
                return;
            }
            assert!(
                Instant::now() < deadline,
                "sin90 never answered /today through the real proxy (last status {status}): \
                 {body}; daemon log:\n{}",
                d.combined_log()
            );
        } else {
            assert!(
                Instant::now() < deadline,
                "sin90 never answered /today through the real proxy; daemon log:\n{}",
                d.combined_log()
            );
        }
        std::thread::sleep(Duration::from_millis(100));
    }
}

/// The KERNEL's own `GET /api/v1/schedules` (top-level route, `agent24d`'s
/// `RESERVED_KERNEL_SEGMENTS` — not a Sin90 route, not proxied to it) —
/// gated only by the daemon bearer token, same as `GET /api/v1/os`.
fn kernel_schedules(d: &Daemon) -> serde_json::Value {
    let (status, body) = http_get(d.port, Some(&d.token), "/api/v1/schedules")
        .unwrap_or_else(|| panic!("no response from GET /api/v1/schedules"));
    assert_eq!(status, 200, "{body}");
    serde_json::from_str(&body).unwrap()
}

/// Every row in a `GET /api/v1/schedules` response owned by Sin90 under the
/// given kernel key (`agent24_protocol::types::ScheduleOwner`'s wire shape:
/// `{"module": "sin90", "key": ...}`).
fn schedule_rows_for_key<'a>(
    schedules: &'a serde_json::Value,
    key: &str,
) -> Vec<&'a serde_json::Value> {
    schedules["schedules"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|s| s["owner"]["module"] == "sin90" && s["owner"]["key"] == key)
        .collect()
}

/// Polls `GET /api/v1/schedules` until EXACTLY ONE row is owned by `key` AND
/// that row satisfies `pred` — or panics: on more than one row for `key` at
/// ANY observed instant (the "幂等对账" invariant this whole test exists to
/// check), or on the deadline elapsing first. `pred` lets one poller serve
/// every one of this test's waits (row exists at all, `enabled == true`,
/// `user_suspended == true`, ...) instead of a bespoke loop per condition.
fn wait_for_one_schedule_row_where(
    d: &Daemon,
    key: &str,
    timeout: Duration,
    mut pred: impl FnMut(&serde_json::Value) -> bool,
) -> serde_json::Value {
    let deadline = Instant::now() + timeout;
    loop {
        let schedules = kernel_schedules(d);
        let rows = schedule_rows_for_key(&schedules, key);
        // Set on every iteration (not just the branches that don't return),
        // so the diagnostic below always reflects the most recent poll.
        let last_seen: Option<serde_json::Value> = rows.first().map(|r| (**r).clone());
        match rows.as_slice() {
            [row] if pred(row) => return (*row).clone(),
            [] | [_] => {}
            many => panic!(
                "kernel reported {} rows owned by sin90 for key {key:?}, expected at most 1: {schedules}",
                many.len()
            ),
        }
        assert!(
            Instant::now() < deadline,
            "schedule {key:?} never matched the expected condition within {timeout:?}; last \
             seen: {last_seen:?}; daemon log:\n{}",
            d.combined_log()
        );
        std::thread::sleep(Duration::from_millis(200));
    }
}

fn wait_for_one_schedule_row(d: &Daemon, key: &str, timeout: Duration) -> serde_json::Value {
    wait_for_one_schedule_row_where(d, key, timeout, |_| true)
}

/// Polls until `key` has NO row owned by sin90 at all (T3.5.1's "retire ->
/// 内核那一行消失").
fn wait_for_schedule_row_absent(d: &Daemon, key: &str, timeout: Duration) {
    let deadline = Instant::now() + timeout;
    loop {
        let schedules = kernel_schedules(d);
        let rows = schedule_rows_for_key(&schedules, key);
        if rows.is_empty() {
            return;
        }
        assert!(
            Instant::now() < deadline,
            "kernel still reports {} row(s) owned by sin90 for key {key:?} after retire: \
             {schedules}; daemon log:\n{}",
            rows.len(),
            d.combined_log()
        );
        std::thread::sleep(Duration::from_millis(200));
    }
}

/// `POST /api/v1/sin90/routines` with the human actor key — every write in
/// this test's own flow goes through Sin90's real HTTP route, never a direct
/// store call, so the outbox/reconciler/kernel path this test exists to
/// prove is exercised the same way a real user's client would exercise it.
fn create_routine(d: &Daemon, human_key: &str, body: &str) -> serde_json::Value {
    let (status, resp) = http_call(
        d.port,
        "POST",
        "/api/v1/sin90/routines",
        Some(&d.token),
        Some(human_key),
        Some(body),
    )
    .unwrap();
    assert_eq!(status, 201, "POST /routines: {resp}");
    serde_json::from_str(&resp).unwrap()
}

/// Every Sin90-internal `routine.fired` event recorded for `routine_id`
/// (`store::repo::record_routine_fire`'s own `append_event(.., "routine",
/// routine_id, "fired", ..)` — entity `"routine"`, `kind` literally
/// `"fired"`, NOT `"routine.fired"`; the dotted form is only the mirrored
/// `EventSink` kind `state.emit` sends the kernel, a separate write this
/// function does not look at).
fn routine_fired_events(d: &Daemon, routine_id: &str) -> Vec<serde_json::Value> {
    let (status, body) = http_get(
        d.port,
        Some(&d.token),
        &format!("/api/v1/sin90/events?entity=routine&entity_id={routine_id}"),
    )
    .unwrap_or_else(|| panic!("no response from GET /events"));
    assert_eq!(status, 200, "{body}");
    let parsed: serde_json::Value = serde_json::from_str(&body).unwrap();
    parsed["events"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|e| e["kind"] == "fired")
        .cloned()
        .collect()
}

/// Polls [`routine_fired_events`] until it has at least `want` entries, or
/// panics on the deadline. The caller (this file's `wait_for_real_tick_fire`
/// callers) still does its OWN exact-count assertion after this returns —
/// this only waits for "at least", the count check itself is the test's
/// actual assertion.
fn wait_for_fired_count_at_least(
    d: &Daemon,
    routine_id: &str,
    want: usize,
    timeout: Duration,
) -> Vec<serde_json::Value> {
    let deadline = Instant::now() + timeout;
    loop {
        let fired = routine_fired_events(d, routine_id);
        if fired.len() >= want {
            return fired;
        }
        assert!(
            Instant::now() < deadline,
            "routine {routine_id} has only {} \"fired\" event(s) after {timeout:?}, wanted \
             {want}: {fired:?}; daemon log:\n{}",
            fired.len(),
            d.combined_log()
        );
        std::thread::sleep(Duration::from_millis(300));
    }
}

fn pick_free_port() -> u16 {
    std::net::TcpListener::bind(("127.0.0.1", 0))
        .unwrap()
        .local_addr()
        .unwrap()
        .port()
}

fn wait_for_port_open(port: u16, timeout: Duration) {
    let deadline = Instant::now() + timeout;
    loop {
        if TcpStream::connect(("127.0.0.1", port)).is_ok() {
            return;
        }
        assert!(
            Instant::now() < deadline,
            "standalone sin90 never started listening on 127.0.0.1:{port}"
        );
        std::thread::sleep(Duration::from_millis(50));
    }
}

fn spawn_ws_subscriber(port: u16, token: &str) -> std::sync::mpsc::Receiver<serde_json::Value> {
    use tokio_tungstenite::tungstenite;
    use tungstenite::client::IntoClientRequest;

    let (tx, rx) = std::sync::mpsc::channel();
    let (ready_tx, ready_rx) = std::sync::mpsc::channel();
    let url = format!("ws://127.0.0.1:{port}/api/v1/events");
    let token = token.to_owned();
    std::thread::spawn(move || {
        let rt = tokio::runtime::Runtime::new().unwrap();
        rt.block_on(async move {
            let mut request = url.into_client_request().expect("a valid ws:// url");
            request
                .headers_mut()
                .insert("Authorization", format!("Bearer {token}").parse().unwrap());
            let (mut socket, _) = tokio_tungstenite::connect_async(request)
                .await
                .expect("the real WS upgrade must succeed");
            let _ = ready_tx.send(());
            use futures::StreamExt;
            while let Some(Ok(tungstenite::Message::Text(text))) = socket.next().await {
                let Ok(value) = serde_json::from_str::<serde_json::Value>(&text) else {
                    continue;
                };
                if tx.send(value).is_err() {
                    break;
                }
            }
        });
    });
    ready_rx
        .recv_timeout(Duration::from_secs(10))
        .expect("the WS subscriber never finished its upgrade");
    rx
}

/// **A1/A2/A3/A11 — the real-kernel gap this file exists to close.** A real
/// `agent24d` binary mounts a real, freshly-built `sin90` binary as an
/// out-of-process package living outside both repos' source trees, and every
/// one of Sin90's own direct-write/read routes round-trips through the real
/// constrained proxy, ending in a real event observed at the real WS
/// consumer boundary — not the synthetic Python fixture `me3f_blackbox.rs`
/// uses to prove the MECHANISM works, and not the loopback-socket unit tests
/// that only prove `adapter_agent24`'s own framing is self-consistent.
#[test]
#[ignore = "needs a sibling Agent24 checkout; run explicitly: cargo test --test agent24_mount_blackbox -- --ignored --test-threads=1"]
fn sin90_mounts_under_a_real_agent24_daemon() {
    let Some(checkout) = agent24_checkout() else {
        eprintln!(
            "skipping: no Agent24 checkout found (set AGENT24_CHECKOUT or place it at ../Agent24)"
        );
        return;
    };
    let agent24d_bin = build_agent24d(&checkout);
    let sin90_bin = PathBuf::from(env!("CARGO_BIN_EXE_sin90"));

    let home = tmp_home("home");

    // First lifetime: nothing installed. Proves the mount that follows is
    // caused by the install-then-restart sequence below, not some other way
    // the daemon might have picked the package up.
    let d1 = start_daemon(&home, &agent24d_bin, &[]);
    let (status, body) = http_get(d1.port, Some(&d1.token), "/api/v1/os").unwrap();
    assert_eq!(status, 200, "{body}");
    let before: serde_json::Value = serde_json::from_str(&body).unwrap();
    if let Some(m) = before["modules"]
        .as_array()
        .unwrap()
        .iter()
        .find(|m| m["name"] == "sin90")
    {
        // T11's OTHER half — "内核里删掉 agent24-sin90-os 后，装上独立仓库
        // 产出的包" (PLAN-OOP-OS-AND-BACKLOG.md) — has not landed: `agent24d`
        // still compiles in `agent24-sin90-os` (see `apps/agent24d/Cargo.toml`)
        // as an IN-PROCESS module, so it mounts under the name "sin90" with
        // zero packages installed and zero `HOME` isolation possible. This
        // test cannot prove the STANDALONE out-of-process package mounts
        // end-to-end while that in-kernel module is still claiming the same
        // name/namespace — installing this repo's package alongside it would
        // either collide on the name (if the kernel's duplicate-name guard
        // catches it) or, worse, silently validate against the WRONG module.
        // Not a bug in this test: it is the real signal that T11 is only
        // half done. Fix on the Agent24 side (remove the
        // `agent24-sin90-os` dependency + its `mount_all` wiring), not here.
        panic!(
            "cannot run this test: agent24d still has an in-process \"sin90\" \
             module compiled in (found before installing anything: {m}) — T11 \
             has not removed `agent24-sin90-os` from the kernel yet. See this \
             test's module doc for what that means and why this file can't \
             route around it."
        );
    }
    drop(d1);

    install_sin90(&home.join(".agent24/packages"), &sin90_bin);

    // Second lifetime: real restart, same already-built agent24d binary, no
    // rebuild from here on for either binary.
    let d2 = start_daemon(&home, &agent24d_bin, &[]);
    let events = spawn_ws_subscriber(d2.port, &d2.token);

    // ── A1 mount / A2 routing proxy ──────────────────────────────────────
    // `os list` can report `mounted` slightly before the module has finished
    // handshaking and is actually ready to serve a proxied request —
    // `me3f_blackbox.rs` hits the identical race and retries the real HTTP
    // call rather than gating on the list; this does the same.
    let deadline = Instant::now() + Duration::from_secs(30);
    let today = loop {
        if let Some((status, body)) = http_get(d2.port, Some(&d2.token), "/api/v1/sin90/today") {
            if status == 200 || Instant::now() >= deadline {
                assert_eq!(status, 200, "daemon log:\n{}", d2.combined_log());
                break body;
            }
        }
        assert!(
            Instant::now() < deadline,
            "sin90 never answered through the real proxy; daemon log:\n{}",
            d2.combined_log()
        );
        std::thread::sleep(Duration::from_millis(100));
    };
    let today: serde_json::Value = serde_json::from_str(&today).unwrap();
    for key in ["must_do", "deep_block", "inbox", "carry_over_candidates"] {
        assert!(
            today.get(key).is_some(),
            "GET /today through the real proxy is missing `{key}`: {today}"
        );
    }
    assert_eq!(os_list_entry(&d2, "sin90")["state"], "mounted");

    // ── A11: a representative sample of the OTHER routes, through the real
    //    proxy. Not exhaustive — Sin90's own `http::tests` already establish
    //    correctness for all of them in-process; what THIS file adds is
    //    proof the constrained proxy (header handling, connection framing)
    //    does not change that for a sample spanning every gate type this
    //    design distinguishes: no-gate (`today` above), `require_any_actor`
    //    (`/capture`), and `require_human` (`/areas`, `/tasks`).
    let human_key = d2.read_actor_key(&home, "human", Duration::from_secs(10));

    let (status, body) = http_call(
        d2.port,
        "GET",
        "/api/v1/sin90/areas",
        Some(&d2.token),
        None,
        None,
    )
    .unwrap();
    assert_eq!(status, 200, "{body}");
    assert!(
        serde_json::from_str::<serde_json::Value>(&body).unwrap()["areas"]
            .as_array()
            .unwrap()
            .is_empty(),
        "a fresh install must start with zero areas: {body}"
    );

    let (status, body) = http_call(
        d2.port,
        "POST",
        "/api/v1/sin90/areas",
        Some(&d2.token),
        Some(&human_key),
        Some(r#"{"title":"Work"}"#),
    )
    .unwrap();
    assert_eq!(status, 201, "POST /areas through the real proxy: {body}");
    let area: serde_json::Value = serde_json::from_str(&body).unwrap();
    assert_eq!(area["title"], "Work");

    let (status, body) = http_call(
        d2.port,
        "POST",
        "/api/v1/sin90/tasks",
        Some(&d2.token),
        Some(&human_key),
        Some(r#"{"title":"ship it","direction_id":null,"parent_task_id":null}"#),
    )
    .unwrap();
    assert_eq!(status, 201, "POST /tasks through the real proxy: {body}");
    let task: serde_json::Value = serde_json::from_str(&body).unwrap();
    let task_id = task["id"].as_str().unwrap().to_owned();

    let (status, body) = http_call(
        d2.port,
        "PATCH",
        &format!("/api/v1/sin90/tasks/{task_id}"),
        Some(&d2.token),
        Some(&human_key),
        Some(r#"{"to":"planned"}"#),
    )
    .unwrap();
    assert_eq!(
        status, 200,
        "PATCH /tasks/{{id}} through the real proxy: {body}"
    );

    // `POST /capture` accepts EITHER actor key by design (§7.1) — call it
    // with no ACTOR key (but still the daemon token, so this exercises
    // Sin90's own `require_any_actor` gate specifically, not the kernel's
    // separate `Authorization` check) to confirm `require_any_actor`
    // genuinely means "some recognized key", not "no gate", the way
    // `today`/`areas` GETs above have none at all.
    let no_key_capture = http_call(
        d2.port,
        "POST",
        "/api/v1/sin90/capture",
        Some(&d2.token),
        None,
        Some(r#"{"text":"note"}"#),
    );
    assert_eq!(
        no_key_capture.as_ref().map(|(s, _)| *s),
        // 403, not 401: Sin90's own `http::actor::forbidden` (design §7.1)
        // uses FORBIDDEN for "no recognized actor key", distinct from the
        // kernel's own 401 for a missing/wrong DAEMON token — two different
        // gates, two different status codes, on purpose.
        Some(403),
        "capture with no actor key at all must still be rejected: {no_key_capture:?}"
    );

    // ── A3: this is NOT the "seven routes" T11 was originally scoped
    //    against (`Agent24/docs/agent/PLAN-OOP-OS-AND-BACKLOG.md` T11's
    //    "七条路由行为不变" predates M0/M1/M2). Recorded here as a fact, not
    //    fixed in this test: 18 distinct paths / 24 (method, path) pairs are
    //    registered in `src/http/mod.rs` as of this file's writing — the
    //    number the coordinator should use going forward, not "seven".
    let (status, body) = http_call(
        d2.port,
        "POST",
        "/api/v1/sin90/capture",
        Some(&d2.token),
        Some(&human_key),
        Some(r#"{"text":"a real note, captured through the real proxy"}"#),
    )
    .unwrap();
    assert_eq!(status, 201, "{body}");

    // ── Event forwarding, at the real WS consumer boundary ──────────────
    // The writes above emitted several `module`/`sin90` events in sequence
    // (`area.created`, `task.created`, `task.transitioned`, two
    // `capture`-triggered `task.created`s) — the subscriber started before
    // any of them, so it sees all of them in order. Matching specifically on
    // `task.created` for THIS test's `task_id` (not just "the first sin90
    // event") is what actually proves forwarding works, not merely that
    // some event arrived.
    let overall_deadline = Instant::now() + Duration::from_secs(30);
    let event = loop {
        let remaining = overall_deadline.saturating_duration_since(Instant::now());
        assert!(
            !remaining.is_zero(),
            "never observed sin90's task.created event (id {task_id}) on the real WS boundary; \
             daemon log:\n{}",
            d2.combined_log()
        );
        match events.recv_timeout(remaining.min(Duration::from_secs(5))) {
            Ok(event)
                if event["type"] == "module"
                    && event["payload"]["module"] == "sin90"
                    && event["payload"]["kind"] == "task.created"
                    && event["payload"]["payload"]["id"] == task_id =>
            {
                break event
            }
            Ok(_) => continue,
            Err(std::sync::mpsc::RecvTimeoutError::Timeout) => continue,
            Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => {
                panic!(
                    "WS subscriber thread ended; daemon log:\n{}",
                    d2.combined_log()
                )
            }
        }
    };
    assert_eq!(event["payload"]["kind"], "task.created", "{event}");
}

/// **T3.2.3 — Offer acceptance under a real mount.** Proves the real
/// `Offer` this real `agent24d` grants Sin90 actually lets its typed kernel
/// clients (`adapter_agent24::clients::Clients`) round-trip for real, one
/// capability at a time: memory (`_a24/memory/private/*`), approval
/// (`_a24/approval/*`), and scheduler (`_a24/scheduler/*`).
/// `sin90_mounts_under_a_real_agent24_daemon` above already proves the
/// mount/proxy/event-forwarding mechanism itself; this test's only job is
/// the three capabilities' actual wire round-trip, which needs its own
/// specially-built `sin90` binary (`build_sin90_with_test_hooks`) because
/// the production binary has no route an HTTP test client could use to
/// drive those typed clients at all — see
/// `src/adapter_agent24/kernel_roundtrip.rs`'s own module doc for why that
/// route exists, why it lives in `adapter_agent24` and not `http`, and why
/// it is `test-hooks`-only.
///
/// **No negative control in THIS test.** The obvious one — a Sin90 manifest
/// that does not request `scheduler`, proving `Offer.provides` then omits
/// `_a24/scheduler/` and a call comes back `forbidden` — would need a
/// second `domain-os.yml` and a second full install/mount cycle standing up
/// a whole second copy of this file's own harness, which was judged not
/// worth the cost for this round (see the task report). That exact negative
/// control already exists on the Agent24 side, at the capability-grant
/// layer this test's positive path exercises: `agent24d`'s own
/// `domain.rs::scheduler_callback_forbidden_without_grant_and_offered_and_working_with_it`
/// (and its neighboring cases) prove an ungranted module's
/// `gate`/`advise`/`status` all come back `forbidden` there.
///
/// Unlike the test above (which `eprintln!`s and returns when no checkout is
/// configured — a legitimate "optional without a sibling repo" skip),
/// a missing Agent24 checkout HERE is a hard FAILURE, per this task's own
/// requirement: `#[ignore]` is already this whole file's mechanism for
/// "never runs under a plain `cargo test`"; a test that ALSO quietly no-ops
/// the moment someone explicitly asks for it with `--ignored` would never
/// fail even when genuinely broken.
#[test]
#[ignore = "needs a sibling Agent24 checkout; run explicitly: cargo test --test agent24_mount_blackbox -- --ignored --test-threads=1"]
fn kernel_clients_roundtrip() {
    let checkout = agent24_checkout().unwrap_or_else(|| {
        panic!(
            "no Agent24 checkout found (set AGENT24_CHECKOUT or place it at ../Agent24) — this \
             test must FAIL, not silently skip, when its prerequisite is missing"
        )
    });
    let agent24d_bin = build_agent24d(&checkout);
    let sin90_bin = build_sin90_with_test_hooks();

    let home = tmp_home("kclients");

    // Same "prove there is nothing already mounted" guard
    // `sin90_mounts_under_a_real_agent24_daemon` uses, and for the same
    // reason (see that test's own comment for the full T11 context).
    let d1 = start_daemon(&home, &agent24d_bin, &[]);
    let (status, body) = http_get(d1.port, Some(&d1.token), "/api/v1/os").unwrap();
    assert_eq!(status, 200, "{body}");
    let before: serde_json::Value = serde_json::from_str(&body).unwrap();
    assert!(
        before["modules"]
            .as_array()
            .unwrap()
            .iter()
            .all(|m| m["name"] != "sin90"),
        "cannot run this test: agent24d still has an in-process \"sin90\" module compiled in \
         (T11 not fully landed — see sin90_mounts_under_a_real_agent24_daemon's own doc): {body}"
    );
    drop(d1);

    install_sin90(&home.join(".agent24/packages"), &sin90_bin);

    // Second lifetime: real restart, same already-built binaries, no
    // rebuild from here on.
    let d2 = start_daemon(&home, &agent24d_bin, &[]);

    // ── A1: wait for the mount (same list-vs-ready race
    //    `sin90_mounts_under_a_real_agent24_daemon` retries around).
    let deadline = Instant::now() + Duration::from_secs(30);
    loop {
        if os_list_entry(&d2, "sin90")["state"] == "mounted" {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "sin90 never reached state \"mounted\"; daemon log:\n{}",
            d2.combined_log()
        );
        std::thread::sleep(Duration::from_millis(100));
    }

    let human_key = d2.read_actor_key(&home, "human", Duration::from_secs(10));

    // The mount can report "mounted" slightly before the module is actually
    // ready to serve a proxied request (`sin90_mounts_under_a_real_agent24_daemon`
    // hits the identical race). Unlike that test's own `/today` probe, THIS
    // readiness wait must poll a side-effect-free route, never the debug
    // route itself: `/debug/kernel-roundtrip` is not idempotent (every
    // successful call inserts a real, never-cleaned-up approval row and
    // memory entry — that route's own module doc) — retrying it in a
    // readiness loop would leave one leftover row per retry, and would
    // change the very thing the assertions below check (e.g.
    // `upsert_outcome == "Created"` would go `"Updated"` on a second real
    // call to it within the same install). Poll `/today` (no side effects,
    // same probe the other test already trusts) to completion FIRST, then
    // call the debug route exactly once.
    let deadline = Instant::now() + Duration::from_secs(30);
    loop {
        if let Some((status, body)) = http_get(d2.port, Some(&d2.token), "/api/v1/sin90/today") {
            if status == 200 {
                break;
            }
            assert!(
                Instant::now() < deadline,
                "sin90 never answered /today through the real proxy (last status {status}): \
                 {body}; daemon log:\n{}",
                d2.combined_log()
            );
        } else {
            assert!(
                Instant::now() < deadline,
                "sin90 never answered /today through the real proxy; daemon log:\n{}",
                d2.combined_log()
            );
        }
        std::thread::sleep(Duration::from_millis(100));
    }

    // Exactly one call — this route has real, uncleaned-up side effects
    // (module doc), so it must not be retried.
    let (status, body) = http_call(
        d2.port,
        "POST",
        "/api/v1/sin90/debug/kernel-roundtrip",
        Some(&d2.token),
        Some(&human_key),
        Some("{}"),
    )
    .unwrap_or_else(|| {
        panic!(
            "no response at all from the real kernel-roundtrip debug route; daemon log:\n{}",
            d2.combined_log()
        )
    });
    assert_eq!(
        status,
        200,
        "POST /debug/kernel-roundtrip through the real proxy: {body}; daemon log:\n{}",
        d2.combined_log()
    );
    let result: serde_json::Value = serde_json::from_str(&body).unwrap();

    // ── Offer.provides — the real handshake's own grant, read back out of
    //    the response (`kernel_roundtrip`'s handler echoes
    //    `KernelClients::offer()` verbatim) — must cover every prefix this
    //    test is about to exercise. This is the assertion the task's own
    //    "变异验证" flips (to a wrong prefix) to prove this test can go red.
    let provides = result["offer"]
        .as_array()
        .unwrap_or_else(|| panic!("response has no \"offer\" array: {result}"))
        .iter()
        .map(|v| v.as_str().unwrap().to_owned())
        .collect::<Vec<_>>();
    // Exact membership (L1, review): the real kernel's own `Offer.provides`
    // entries for these three capabilities are already the identical
    // strings Sin90 declares in `SIN90_CAPABILITY_PREFIXES`
    // (`adapter_agent24::clients::{memory,approval,scheduler}::PREFIX`), not
    // some looser/shorter form — pinning the exact string here catches the
    // kernel ever granting a differently-shaped prefix, which a
    // starts_with-either-way check would silently paper over.
    for want in ["_a24/scheduler/", "_a24/memory/private/", "_a24/approval/"] {
        assert!(
            provides.contains(&want.to_string()),
            "Offer.provides {provides:?} does not cover {want}"
        );
    }

    // ── memory: remember, then recall it back ------------------------------
    assert_eq!(
        result["memory"]["found_in_recall"], true,
        "remember-then-recall round trip failed: {result}"
    );
    assert!(
        result["memory"]["remembered_id"]
            .as_str()
            .is_some_and(|id| id.starts_with("osmem:")),
        "kernel-minted memory id has an unexpected shape: {result}"
    );

    // ── approval: gate the one closed-set action ---------------------------
    assert_eq!(
        result["approval"]["decision"], "Pending",
        "gate round trip did not come back Pending: {result}"
    );
    assert!(
        result["approval"]["approval_id"].as_str().is_some(),
        "{result}"
    );

    // ── scheduler: upsert, list, delete --------------------------------------
    assert_eq!(
        result["scheduler"]["upsert_outcome"], "Created",
        "a fresh install's first upsert of this key must be Created: {result}"
    );
    assert_eq!(
        result["scheduler"]["found_in_list"], true,
        "upserted key was not visible in list: {result}"
    );
    assert_eq!(
        result["scheduler"]["delete_outcome"], "Deleted",
        "delete of a key list just proved exists must be Deleted, not Absent: {result}"
    );
    // L3 (review): the delete must actually have taken — not just answered
    // `Deleted` — proven by a THIRD list call (inside the same debug-route
    // request) no longer finding the key.
    assert_eq!(
        result["scheduler"]["found_after_delete"], false,
        "key was still visible in list after delete claimed \"Deleted\": {result}"
    );
}

/// **T3.5.1 — M3 real-mount acceptance** (`docs/DESIGN-LIFEOS.md` §M3's own
/// acceptance text; `docs/agent/tasks.md` T3.5.1). Every daemon in this test
/// is started with `A24_SCHEDULER_TICK_SECS=1` (design's own instruction —
/// `agent24d/src/server.rs`'s tick-interval env read, default 10s) so the
/// real-tick assertion below (§4) does not have to wait a whole default
/// cycle on top of the cron slot itself.
///
/// Covers, in order, EVERY numbered point tasks.md T3.5.1 lists:
/// 1. create "每周 3 次运动" -> kernel `GET /api/v1/schedules` has 1 row for it.
/// 2. restart the daemon -> still 1 row, not disabled.
/// 3. positive control: `adapter_agent24::reconciler_debug`'s test hook sends
///    the kernel TWO real back-to-back upserts for the same Routine -> still
///    1 row (the kernel's own idempotency-by-key, not merely Sin90's outbox
///    dedup, which is what would otherwise hide a real kernel bug here).
/// 4. a second Routine, cron pinned to a real near-future minute -> waits for
///    an ACTUAL kernel tick to reach it (no `At`, per the task's own
///    instruction: Sin90's `Routine` only ever has `cron`) -> exactly one
///    internal `routine.fired` row. `run_now` is then used as an EXTRA
///    positive control (a second, distinct fire, not folded into the "exactly
///    one" count above).
/// 5. a client posting a forged `POST /_a24/scheduler/fired` directly at a
///    STANDALONE Sin90 (no Agent24 in front of it at all) gets 404 — proving
///    `crate::http::router`'s own documented guard: the route simply does not
///    exist outside mounted mode, so there is nothing a forged
///    `X-A24-Fire-Id` could ever reach.
/// 6. `POST .../transition {"to":"paused"}` -> the kernel row's `enabled`
///    flips to `false`.
/// 7. resume it (back to `enabled: true`), then a HUMAN suspends the SAME
///    kernel row directly via the kernel's OWN `POST
///    /api/v1/schedules/{id}/suspend` (bypassing Sin90 entirely) -> restart
///    the daemon -> the row is STILL suspended (`adapter_agent24::reconciler`'s
///    own "user_suspended 的行不去碰" rule: the startup full-reconcile must
///    not fight a human's kernel-side suspension just because Sin90 itself
///    still considers the Routine `active`).
/// 8. retire it -> the kernel row disappears entirely.
#[test]
#[ignore = "needs a sibling Agent24 checkout; run explicitly: cargo test --test agent24_mount_blackbox -- --ignored --test-threads=1"]
fn routine_m3_real_mount_acceptance() {
    let checkout = agent24_checkout().unwrap_or_else(|| {
        panic!(
            "no Agent24 checkout found (set AGENT24_CHECKOUT or place it at ../Agent24) — this \
             test must FAIL, not silently skip, when its prerequisite is missing"
        )
    });
    let agent24d_bin = build_agent24d(&checkout);
    let sin90_bin = build_sin90_with_test_hooks();

    const FAST_TICK: &[(&str, &str)] = &[("A24_SCHEDULER_TICK_SECS", "1")];
    let mount_ready = Duration::from_secs(30);
    let sync_timeout = Duration::from_secs(30);

    // ── §5 first, standalone, before any real mount even exists: a client
    //    forging `POST /_a24/scheduler/fired` directly at a bare Sin90 gets
    //    404 (`crate::http::router`'s own doc: the route is simply never
    //    registered outside mounted mode — 404, not "route exists but
    //    rejects"). Fully independent of `agent24d`/the rest of this test;
    //    run first so a broken guard here fails fast rather than after
    //    several minutes of mount/tick setup.
    {
        let port = pick_free_port();
        let data_dir = tmp_home("standalone-fired-guard");
        let child = Command::new(&sin90_bin)
            .args([
                "serve",
                "--port",
                &port.to_string(),
                "--data-dir",
                data_dir.to_str().unwrap(),
            ])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .unwrap_or_else(|e| panic!("could not spawn standalone sin90: {e}"));
        let _standalone = Running(child);
        wait_for_port_open(port, Duration::from_secs(10));
        let (status, body) = http_call(
            port,
            "POST",
            "/_a24/scheduler/fired",
            None,
            None,
            Some(
                r#"{"key":"routine.forged","trigger":"tick","scheduled_for":"2026-01-01T00:00:00Z","fired_at":"2026-01-01T00:00:00Z"}"#,
            ),
        )
        .unwrap_or_else(|| panic!("no response at all from standalone sin90 on port {port}"));
        assert_eq!(
            status, 404,
            "a forged fired POST at a STANDALONE sin90 must 404 (route not registered outside \
             mounted mode), got {status}: {body}"
        );
    }

    let home = tmp_home("routine-m3");

    // Same "prove there is nothing already mounted" guard the other two
    // tests in this file use (see `sin90_mounts_under_a_real_agent24_daemon`'s
    // own doc for the full T11 context).
    let d1 = start_daemon(&home, &agent24d_bin, &[]);
    let (status, body) = http_get(d1.port, Some(&d1.token), "/api/v1/os").unwrap();
    assert_eq!(status, 200, "{body}");
    let before: serde_json::Value = serde_json::from_str(&body).unwrap();
    assert!(
        before["modules"]
            .as_array()
            .unwrap()
            .iter()
            .all(|m| m["name"] != "sin90"),
        "cannot run this test: agent24d still has an in-process \"sin90\" module compiled in \
         (T11 not fully landed — see sin90_mounts_under_a_real_agent24_daemon's own doc): {body}"
    );
    drop(d1);

    install_sin90(&home.join(".agent24/packages"), &sin90_bin);

    // ── generation 2: create Routine A, prove §1 ────────────────────────────
    let d2 = start_daemon(&home, &agent24d_bin, FAST_TICK);
    wait_for_sin90_ready(&d2, mount_ready);
    let human_key = d2.read_actor_key(&home, "human", Duration::from_secs(10));

    let routine_a = create_routine(
        &d2,
        &human_key,
        r#"{"title":"每周3次运动","kind":"exercise","cron":"0 7 * * MON,WED,FRI","target_count":3}"#,
    );
    let routine_a_id = routine_a["id"].as_str().unwrap().to_owned();
    // `store::repo::routine_kernel_key`'s own doc (T3.5.1 found this): the
    // kernel's schedule key charset is `[a-z0-9._-]`, byte-exact, while
    // `routine_a_id` is an uppercase `ulid()` — the ACTUAL key the kernel
    // stores this Routine under is the lower-cased form.
    let key_a = format!("routine.{}", routine_a_id.to_lowercase());

    let schedule_a = wait_for_one_schedule_row(&d2, &key_a, sync_timeout);
    assert_eq!(schedule_a["enabled"], true, "{schedule_a}");
    assert_eq!(schedule_a["spec"]["type"], "cron", "{schedule_a}");
    assert_eq!(
        schedule_a["spec"]["expr"], "0 7 * * MON,WED,FRI",
        "{schedule_a}"
    );
    drop(d2);

    // ── generation 3: §2 restart -> still 1 row, not disabled ──────────────
    let d3 = start_daemon(&home, &agent24d_bin, FAST_TICK);
    wait_for_sin90_ready(&d3, mount_ready);
    let human_key = d3.read_actor_key(&home, "human", Duration::from_secs(10));

    let schedule_a =
        wait_for_one_schedule_row_where(&d3, &key_a, sync_timeout, |row| row["enabled"] == true);
    assert_eq!(
        schedule_a["system_disabled_reason"],
        serde_json::Value::Null,
        "a routine that never drifted must not come back disabled after a restart: {schedule_a}"
    );
    assert_eq!(schedule_a["user_suspended"], false, "{schedule_a}");

    // ── §3: positive control — force TWO real, distinct upserts for the SAME
    //    Routine straight at the kernel, bypassing Sin90's own outbox dedup
    //    entirely (`adapter_agent24::reconciler_debug`'s own module doc) —
    //    still exactly 1 kernel row afterward.
    let (status, body) = http_call(
        d3.port,
        "POST",
        "/api/v1/sin90/debug/reconciler/force-upsert",
        Some(&d3.token),
        Some(&human_key),
        Some(&format!(r#"{{"routine_id":"{routine_a_id}"}}"#)),
    )
    .unwrap();
    assert_eq!(status, 200, "force-upsert: {body}");
    let force_result: serde_json::Value = serde_json::from_str(&body).unwrap();
    for outcome_field in ["first_outcome", "second_outcome"] {
        let outcome = force_result[outcome_field].as_str().unwrap();
        assert!(
            outcome == "Updated" || outcome == "Unchanged",
            "expected a real upsert outcome (Updated or Unchanged) for {outcome_field}, got \
             {outcome}: {force_result}"
        );
    }
    // The actual assertion: still exactly one row for this key — panics
    // inside `wait_for_one_schedule_row` if the double upsert ever produced a
    // second row.
    wait_for_one_schedule_row(&d3, &key_a, sync_timeout);

    // ── §4: Routine B, cron pinned to a real near-future minute, waited out
    //    with a real tick — no `At`, per the task's own instruction.
    //
    // `lead_secs = 75`: `target = now + 75s`'s OWN minute boundary
    // (`target - target.second()` seconds) lands somewhere between `now +
    // 16s` and `now + 75s` (`target.second()` ranges 0..59) — always
    // strictly in the future, and bounded close to the task's own "至多约
    // 90s" polling guidance without cutting it so fine that scheduler-tick
    // jitter (`A24_SCHEDULER_TICK_SECS=1`) could plausibly miss it.
    let cron_computed_at = Instant::now();
    let target = chrono::Utc::now() + chrono::Duration::seconds(75);
    let cron_b = format!("{} {} * * *", target.minute(), target.hour());
    let routine_b = create_routine(
        &d3,
        &human_key,
        &format!(r#"{{"title":"T3.5.1 tick probe","kind":"other","cron":"{cron_b}"}}"#),
    );
    let routine_b_id = routine_b["id"].as_str().unwrap().to_owned();
    let key_b = format!("routine.{}", routine_b_id.to_lowercase());
    let schedule_b = wait_for_one_schedule_row(&d3, &key_b, sync_timeout);
    assert_eq!(schedule_b["enabled"], true, "{schedule_b}");

    // Budget anchored to when the cron target was computed, not to "now" —
    // the schedule-row wait above already spent part of the 75s lead.
    let fire_deadline = cron_computed_at + Duration::from_secs(75 + 30);
    let tick_wait = fire_deadline.saturating_duration_since(Instant::now());
    wait_for_fired_count_at_least(&d3, &routine_b_id, 1, tick_wait.max(Duration::from_secs(5)));
    // The kernel's own delivery contract never retries a 2xx response
    // (`http::mod::scheduler_fired`'s own doc: "every outcome below answers
    // 2xx"), so a short settle-and-recheck is enough to prove no SECOND fire
    // ever lands, not just that the first one hasn't yet.
    std::thread::sleep(Duration::from_secs(3));
    let fired = routine_fired_events(&d3, &routine_b_id);
    assert_eq!(
        fired.len(),
        1,
        "expected exactly 1 routine.fired from the real tick, got {fired:?}"
    );
    assert_eq!(fired[0]["payload"]["trigger"], "tick", "{fired:?}");

    // `run_now` — an EXTRA positive control, a second and DISTINCT fire, not
    // folded into the "exactly 1" tick assertion above.
    let (status, body) = http_call(
        d3.port,
        "POST",
        &format!(
            "/api/v1/schedules/{}/run_now",
            schedule_b["id"].as_str().unwrap()
        ),
        Some(&d3.token),
        None,
        None,
    )
    .unwrap();
    assert_eq!(status, 202, "POST run_now: {body}");
    let fired = wait_for_fired_count_at_least(&d3, &routine_b_id, 2, Duration::from_secs(30));
    assert_eq!(fired.len(), 2, "{fired:?}");
    let triggers: Vec<&str> = fired
        .iter()
        .map(|e| e["payload"]["trigger"].as_str().unwrap())
        .collect();
    assert!(
        triggers.contains(&"tick") && triggers.contains(&"run_now"),
        "expected one \"tick\" and one \"run_now\" fire, got {triggers:?}"
    );

    // ── §6: pause Routine A -> kernel row's `enabled` flips to `false` ─────
    let (status, body) = http_call(
        d3.port,
        "POST",
        &format!("/api/v1/sin90/routines/{routine_a_id}/transition"),
        Some(&d3.token),
        Some(&human_key),
        Some(r#"{"to":"paused"}"#),
    )
    .unwrap();
    assert_eq!(status, 200, "pause transition: {body}");
    wait_for_one_schedule_row_where(&d3, &key_a, sync_timeout, |row| row["enabled"] == false);

    // Resume it — back to `enabled: true` — so §7 below suspends an
    // otherwise-`active` Routine (the meaningful case: Sin90 itself thinks
    // this row should be enabled, and the human's kernel-side suspension has
    // to win anyway).
    let (status, body) = http_call(
        d3.port,
        "POST",
        &format!("/api/v1/sin90/routines/{routine_a_id}/transition"),
        Some(&d3.token),
        Some(&human_key),
        Some(r#"{"to":"active"}"#),
    )
    .unwrap();
    assert_eq!(status, 200, "resume transition: {body}");
    let schedule_a =
        wait_for_one_schedule_row_where(&d3, &key_a, sync_timeout, |row| row["enabled"] == true);
    let schedule_a_kernel_id = schedule_a["id"].as_str().unwrap().to_owned();

    // ── §7a: a human suspends the SAME row directly via the kernel's OWN
    //    REST, bypassing Sin90 entirely.
    let (status, body) = http_call(
        d3.port,
        "POST",
        &format!("/api/v1/schedules/{schedule_a_kernel_id}/suspend"),
        Some(&d3.token),
        None,
        None,
    )
    .unwrap();
    assert_eq!(status, 200, "kernel suspend: {body}");
    let suspended: serde_json::Value = serde_json::from_str(&body).unwrap();
    assert_eq!(suspended["user_suspended"], true, "{suspended}");
    assert_eq!(suspended["effective_enabled"], false, "{suspended}");
    drop(d3);

    // ── generation 4: §7b restart -> STILL suspended ────────────────────────
    let d4 = start_daemon(&home, &agent24d_bin, FAST_TICK);
    wait_for_sin90_ready(&d4, mount_ready);
    let human_key = d4.read_actor_key(&home, "human", Duration::from_secs(10));
    let schedule_a = wait_for_one_schedule_row_where(&d4, &key_a, sync_timeout, |row| {
        row["user_suspended"] == true
    });
    assert_eq!(
        schedule_a["effective_enabled"], false,
        "a human's kernel-side suspension must survive a restart+reconcile even though Sin90 \
         itself still considers this Routine active: {schedule_a}"
    );

    // ── §8: retire -> kernel row disappears entirely ───────────────────────
    let (status, body) = http_call(
        d4.port,
        "POST",
        &format!("/api/v1/sin90/routines/{routine_a_id}/transition"),
        Some(&d4.token),
        Some(&human_key),
        Some(r#"{"to":"retired"}"#),
    )
    .unwrap();
    assert_eq!(status, 200, "retire transition: {body}");
    wait_for_schedule_row_absent(&d4, &key_a, sync_timeout);
}

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

use std::collections::BTreeMap;
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

/// Sin90's own `GET /today` through the real proxy — no gate, side-effect
/// free (`sin90_mounts_under_a_real_agent24_daemon`'s own readiness probe).
fn today(d: &Daemon) -> serde_json::Value {
    let (status, body) = http_get(d.port, Some(&d.token), "/api/v1/sin90/today")
        .unwrap_or_else(|| panic!("no response from GET /today"));
    assert_eq!(status, 200, "{body}");
    serde_json::from_str(&body).unwrap()
}

/// T3.5.1 review (H1): a restart's own reconcile pass, for a routine the
/// human just suspended directly on the kernel, is only PROVEN to have run
/// (and to have taken the `user_suspended` branch, and to have correctly
/// mapped the kernel's lower-cased key back onto this routine's real,
/// uppercase id) by an actual, ONLY-reconcile-produced side effect —
/// `store::Sin90Store::sync_kernel_suspended_routines`, called exclusively
/// from `adapter_agent24::reconciler::reconcile_full`'s `user_suspended`
/// branch, is the ONE thing that ever populates `/today`'s
/// `kernel_suspended_routines`. Polling the KERNEL schedule row's own
/// `user_suspended` flag instead (as an earlier draft of this test did)
/// would be nearly vacuous: the kernel's `upsert` never clears
/// `user_suspended` in the first place (`agent24_protocol::types::Schedule`'s
/// own doc — only `.../suspend`/`.../resume` do), so that flag would read
/// `true` after a restart whether or not the reconciler ever ran, or even
/// whether it ran CORRECTLY.
fn wait_for_today_kernel_suspended(
    d: &Daemon,
    routine_id: &str,
    timeout: Duration,
) -> serde_json::Value {
    let deadline = Instant::now() + timeout;
    loop {
        let t = today(d);
        if t["kernel_suspended_routines"]
            .as_array()
            .unwrap()
            .iter()
            .any(|r| r["id"] == routine_id)
        {
            return t;
        }
        assert!(
            Instant::now() < deadline,
            "sin90's own /today never listed {routine_id} under kernel_suspended_routines after \
             a restart (i.e. the startup full reconcile never ran, never took the \
             user_suspended branch, or never mapped the kernel's key back to this routine): \
             {t}; daemon log:\n{}",
            d.combined_log()
        );
        std::thread::sleep(Duration::from_millis(200));
    }
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

// ---------------------------------------------------------------------------
// T5.5.1 — M5 (AI v1) real-mount acceptance additions. Everything below this
// point (down to `t551_ai_v1_m5_real_mount_acceptance` itself) is shared
// ONLY by that one test.
// ---------------------------------------------------------------------------

/// A minimal, single-purpose OpenAI-compat HTTP stub standing in for a local
/// oMLX server: Agent24's own `agent24-models::OpenAiCompatProvider` posts
/// to `{base}/v1/chat/completions` regardless of which local backend is
/// configured (`ai::ports::MODEL_ACCESS` is compiled-in `LocalOnly` for the
/// OFFICIAL binary this test mounts — `ai_classify::trigger_classify` reads
/// that constant, not an env var, T5.1.2's own doc), so answering that one
/// path is enough to stand in for "`OMLX_URL` 指向本地桩" (the task's own
/// wording). Always replies with the SAME fixed `choices[0].message.content`
/// regardless of the request body — this test controls its own fixture (one
/// real Direction candidate, `"d1"`), so the reply never needs to depend on
/// what was actually asked — and counts how many requests actually arrived,
/// so a test can assert the local model was genuinely DIALED, not merely
/// configured. Mirrors Agent24's own `agent24-models::router::tests::
/// thread_stub` (same single-`read`-then-reply shape, same justification: a
/// real loopback HTTP client's one small JSON POST arrives in a single
/// `read()` in practice).
struct ModelStub {
    port: u16,
    hits: Arc<std::sync::atomic::AtomicUsize>,
}

impl ModelStub {
    fn start(content: &str) -> Self {
        let listener = std::net::TcpListener::bind(("127.0.0.1", 0)).unwrap();
        let port = listener.local_addr().unwrap().port();
        let hits = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let hits2 = hits.clone();
        let body = serde_json::json!({
            "model": "stub-model",
            "choices": [{"message": {"role": "assistant", "content": content}}],
            "usage": {"prompt_tokens": 1, "completion_tokens": 1, "total_tokens": 2},
        })
        .to_string();
        std::thread::spawn(move || {
            for stream in listener.incoming() {
                let Ok(mut s) = stream else { continue };
                hits2.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                let _ = s.set_read_timeout(Some(Duration::from_millis(2000)));
                let mut buf = [0u8; 65536];
                let _ = Read::read(&mut s, &mut buf);
                let resp = format!(
                    "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\n\
                     connection: close\r\n\r\n{}",
                    body.len(),
                    body
                );
                let _ = Write::write_all(&mut s, resp.as_bytes());
            }
        });
        Self { port, hits }
    }

    fn hits(&self) -> usize {
        self.hits.load(std::sync::atomic::Ordering::SeqCst)
    }
}

/// A throwaway READ-ONLY connection pool straight at a mounted daemon's OWN
/// `sin90.db` (located via `find_file`, the same technique
/// `Daemon::read_actor_key` already uses for `actor-keys.json`) — SQLite's
/// WAL mode lets any number of readers see committed data alongside the
/// daemon's own live writer connection, so this needs no coordination with
/// the daemon process at all, and `read_only(true)` guarantees this side of
/// the test can never itself corrupt the live db. A tiny dedicated
/// single-thread runtime: `sqlx` is async, this whole file is otherwise
/// synchronous (mirrors `spawn_ws_subscriber`'s own justification).
///
/// This does the SAME "snapshot every table" job as `store::test_hooks::
/// snapshot_all_tables` (`src/store/mod.rs`) — deliberately re-implemented
/// here rather than imported: that module is gated behind `#[cfg(any(test,
/// feature = "test-hooks"))]` on the LIBRARY, and this acceptance run's own
/// command (`cargo test --test agent24_mount_blackbox -- --ignored
/// --test-threads=1`, the task's own text) does not pass `--features
/// test-hooks`, so the library this integration test links against was
/// built WITHOUT that cfg — `sin90::store::test_hooks` is simply not there
/// to import. Kept in exact lockstep with that function's own logic
/// (including the `sin90_events` entity=proposal/¬proposal split) so both
/// copies encode the identical "只这三样能变" judgment.
struct Db {
    rt: tokio::runtime::Runtime,
    pool: sqlx::SqlitePool,
}

impl Db {
    fn open_readonly(home: &Path) -> Self {
        let path = find_file(home, "sin90.db")
            .unwrap_or_else(|| panic!("sin90.db not found anywhere under {}", home.display()));
        let rt = tokio::runtime::Runtime::new().unwrap();
        let pool = rt.block_on(async {
            let opts = sqlx::sqlite::SqliteConnectOptions::new()
                .filename(&path)
                .read_only(true);
            sqlx::sqlite::SqlitePoolOptions::new()
                .max_connections(1)
                .connect_with(opts)
                .await
                .unwrap_or_else(|e| panic!("could not open {} read-only: {e}", path.display()))
        });
        Self { rt, pool }
    }

    fn scalar_i64(&self, sql: &str) -> i64 {
        let pool = &self.pool;
        self.rt
            .block_on(async move {
                let row: (i64,) = sqlx::query_as(sql).fetch_one(pool).await?;
                Ok::<_, sqlx::Error>(row.0)
            })
            .unwrap()
    }

    /// Every table in the db, snapshotted as one ordered `Vec<String>` of
    /// every-column-quoted rows — see this struct's own doc for why this is
    /// a re-implementation of `store::test_hooks::snapshot_all_tables`
    /// rather than an import of it.
    fn snapshot_all_tables(&self) -> BTreeMap<String, Vec<String>> {
        let pool = &self.pool;
        self.rt.block_on(async move {
            let tables: Vec<String> = sqlx::query_scalar(
                "SELECT name FROM sqlite_master WHERE type = 'table' AND name NOT LIKE 'sqlite_%' \
                 ORDER BY name",
            )
            .fetch_all(pool)
            .await
            .unwrap();
            let mut out = BTreeMap::new();
            for t in tables {
                let cols: Vec<String> =
                    sqlx::query_scalar(&format!("SELECT name FROM pragma_table_info('{t}')"))
                        .fetch_all(pool)
                        .await
                        .unwrap();
                let expr = cols
                    .iter()
                    .map(|c| format!("quote({c})"))
                    .collect::<Vec<_>>()
                    .join(" || '|' || ");
                if t == "sin90_events" {
                    for (key, where_clause) in [
                        ("sin90_events(entity=proposal)", "WHERE entity = 'proposal'"),
                        (
                            "sin90_events(entity<>proposal)",
                            "WHERE entity <> 'proposal'",
                        ),
                    ] {
                        let rows: Vec<String> = sqlx::query_scalar(&format!(
                            "SELECT {expr} AS r FROM {t} {where_clause} ORDER BY rowid"
                        ))
                        .fetch_all(pool)
                        .await
                        .unwrap();
                        out.insert(key.to_string(), rows);
                    }
                    continue;
                }
                let rows: Vec<String> =
                    sqlx::query_scalar(&format!("SELECT {expr} AS r FROM {t} ORDER BY rowid"))
                        .fetch_all(pool)
                        .await
                        .unwrap();
                out.insert(t, rows);
            }
            out
        })
    }
}

/// Which snapshot keys changed, `before` -> `after` — a plain `BTreeMap`
/// compare, factored out as its own function so its discriminating power can
/// be exercised directly against hand-built maps (this file's own mutation
/// verification of the CHECK itself), not just implicitly through one real
/// run that happens to produce no diff.
fn diff_snapshot_keys(
    before: &BTreeMap<String, Vec<String>>,
    after: &BTreeMap<String, Vec<String>>,
) -> Vec<String> {
    before
        .keys()
        .filter(|k| before.get(*k) != after.get(*k))
        .cloned()
        .collect()
}

/// Polls `GET /ai/runs/{run_id}` until it reaches a terminal state
/// (`"done"`/`"aborted"`) — `"running"`/`"unknown"` keep waiting.
fn wait_for_ai_run_done(d: &Daemon, run_id: &str, timeout: Duration) -> serde_json::Value {
    let deadline = Instant::now() + timeout;
    loop {
        if let Some((status, body)) = http_get(
            d.port,
            Some(&d.token),
            &format!("/api/v1/sin90/ai/runs/{run_id}"),
        ) {
            if status == 200 {
                let rec: serde_json::Value = serde_json::from_str(&body).unwrap();
                let state = rec["state"].as_str().unwrap_or("unknown");
                if state == "done" || state == "aborted" {
                    return rec;
                }
            }
        }
        assert!(
            Instant::now() < deadline,
            "ai run {run_id} never reached a terminal state within {timeout:?}; daemon log:\n{}",
            d.combined_log()
        );
        std::thread::sleep(Duration::from_millis(100));
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
/// 2. inject REAL drift straight on the kernel side (`DELETE
///    /api/v1/schedules/{id}` — no module-ownership restriction, unlike
///    PATCH), THEN restart the daemon -> the row REAPPEARS, with a fresh
///    kernel schedule id, `enabled: true`, `system_disabled_reason: null`
///    (review round 2, M1: an assertion that the row is merely still
///    `enabled: true` after a restart would be true whether or not the
///    startup full reconcile ever ran at all — the kernel's own schedule
///    state persists across an `agent24d` restart regardless; deleting it
///    first makes the row's very reappearance an actual, reconcile-only
///    observable effect. A PATCH-based drift injection was tried first and
///    rejected: for a module row, the kernel's own `enabled`-only PATCH is
///    secretly `suspend`/`resume` — see the test body's own comment).
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
///    `X-A24-Fire-Id` could ever reach. Positive control: the same standalone
///    port answers an ordinary route (`GET /today`) with 200. Review round 2,
///    H2 ALSO covers the mounted-mode half of this: the identical forgery,
///    with a valid daemon token and routine_b's own REAL key, through the
///    real daemon -> still 404 (the kernel's OWN reserved-path judgement,
///    ME4-1.3.2 §7.1 — a different layer than Sin90's router, which in
///    mounted mode DOES register this route but can never be reached through
///    the public proxy) — and routine_b's `routine.fired` count is unchanged.
/// 6. `POST .../transition {"to":"paused"}` -> the kernel row's `enabled`
///    flips to `false`.
/// 7. resume it (back to `enabled: true`), then a HUMAN suspends the SAME
///    kernel row directly via the kernel's OWN `POST
///    /api/v1/schedules/{id}/suspend` (bypassing Sin90 entirely) -> restart
///    the daemon -> wait for SIN90'S OWN `/today` to list this routine under
///    `kernel_suspended_routines` (review round 2, H1: the kernel's
///    `user_suspended` flag alone is nearly vacuous here — `upsert` never
///    clears it, so it would read `true` whether or not the reconciler ran,
///    or ran correctly; `/today`'s own list is populated ONLY by the startup
///    full reconcile's `user_suspended` branch, which also proves the
///    kernel's lower-cased key was correctly mapped back to this routine's
///    real, uppercase id) -> THEN the kernel row is still suspended
///    (`adapter_agent24::reconciler`'s own "user_suspended 的行不去碰" rule:
///    the startup full-reconcile must not fight a human's kernel-side
///    suspension just because Sin90 itself still considers the Routine
///    `active`).
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
        // H2 positive control: the 404 above is the ROUTE genuinely not
        // existing, not this standalone process being broken/unreachable —
        // the SAME port answers an ordinary, always-registered route fine.
        // Standalone routes are un-nested (`main.rs`'s own doc), so bare
        // `/today`, not `/api/v1/sin90/today`.
        let (status, body) = http_call(port, "GET", "/today", None, None, None)
            .unwrap_or_else(|| panic!("no response from standalone sin90 on port {port}"));
        assert_eq!(
            status, 200,
            "positive control: GET /today on the SAME standalone port must succeed: {body}"
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
    let key_a = format!("routine.{}", routine_a_id.to_ascii_lowercase());

    let schedule_a = wait_for_one_schedule_row(&d2, &key_a, sync_timeout);
    assert_eq!(schedule_a["enabled"], true, "{schedule_a}");
    assert_eq!(schedule_a["spec"]["type"], "cron", "{schedule_a}");
    assert_eq!(
        schedule_a["spec"]["expr"], "0 7 * * MON,WED,FRI",
        "{schedule_a}"
    );
    let schedule_a_kernel_id = schedule_a["id"].as_str().unwrap().to_owned();

    // T3.5.1 review (M1): before restarting, inject REAL drift straight on
    // the kernel side — DELETE the row entirely (`DELETE /api/v1/schedules/
    // {id}` has no module-ownership restriction, unlike PATCH: `Scheduler::
    // delete`'s own doc). A PATCH of `{"enabled": false}` was tried first and
    // rejected in review: for a MODULE row, `agent24-scheduler`'s own
    // `module_row_patch` routes ANY enabled-only PATCH straight to
    // `suspend`/`resume` — it never touches the raw `enabled` column at all,
    // so it would have silently turned this into a `user_suspended` case
    // (H1's territory) while leaving `enabled` itself completely untouched.
    // Deleting the row outright is unambiguous: nothing but a REAL, running
    // full reconcile can make it reappear.
    let (status, body) = http_call(
        d2.port,
        "DELETE",
        &format!("/api/v1/schedules/{schedule_a_kernel_id}"),
        Some(&d2.token),
        None,
        None,
    )
    .unwrap();
    assert_eq!(status, 204, "DELETE schedule (inject drift): {body}");
    drop(d2);

    // ── generation 3: §2 restart -> the deleted row reappears ──────────────
    let d3 = start_daemon(&home, &agent24d_bin, FAST_TICK);
    wait_for_sin90_ready(&d3, mount_ready);
    let human_key = d3.read_actor_key(&home, "human", Duration::from_secs(10));

    // M1: the startup full reconcile must notice the row is genuinely GONE
    // (`local` has an active Routine, `kernel.get(key)` is `None` —
    // `reconcile_full`'s own "本地有、内核缺 → 补上" branch) and re-create it —
    // an observable effect (a NEW kernel schedule id) that proves reconcile
    // genuinely ran, not merely that nothing needed to change.
    let schedule_a =
        wait_for_one_schedule_row_where(&d3, &key_a, sync_timeout, |row| row["enabled"] == true);
    assert_ne!(
        schedule_a["id"], schedule_a_kernel_id,
        "a re-created row must get a fresh kernel schedule id, proving it was genuinely \
         recreated rather than the deleted one somehow surviving: {schedule_a}"
    );
    assert_eq!(
        schedule_a["system_disabled_reason"],
        serde_json::Value::Null,
        "a freshly re-created row must not come back disabled: {schedule_a}"
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
    // `first_outcome` may legitimately be either: this debug route's own
    // upsert call (a plain `ModuleSpec::Cron`/`enabled`/no `label`) is not
    // necessarily byte-identical to whatever the reconcile-on-restart path
    // above (§2) already wrote for this same key, so the kernel may still
    // see a real field-level change on the FIRST of these two calls.
    let first_outcome = force_result["first_outcome"].as_str().unwrap();
    assert!(
        first_outcome == "Updated" || first_outcome == "Unchanged",
        "expected a real upsert outcome (Updated or Unchanged) for first_outcome, got \
         {first_outcome}: {force_result}"
    );
    // T3.5.1 review (PR #63, Low 1/2): `second_outcome` must NOT be allowed
    // to also read `Updated` — it is the SECOND of two back-to-back calls
    // with the EXACT SAME key/spec/enabled/label/request_id, no time and no
    // other write in between, so the kernel's own idempotent-by-key `upsert`
    // has nothing left to change. Allowing `Updated` here would have let a
    // regression that made `upsert` non-idempotent (re-writing on every call
    // regardless of whether anything differs) slip through as if it were
    // the harmless "first call still had something to fix" case above.
    assert_eq!(
        force_result["second_outcome"], "Unchanged",
        "the second of two immediately-repeated upserts for an UNCHANGED Routine must be a \
         true no-op (Unchanged), not Updated: {force_result}"
    );
    // T3.5.1 review (PR #63, Low 2/2): "内核侧幂等" must compare the SAME
    // kernel schedule id across the double upsert, not merely "still exactly
    // one row" — the row-count check alone would not catch a bug where the
    // kernel silently deleted-and-recreated a row under the same key on a
    // redundant upsert (still exactly 1 row, but a DIFFERENT id, i.e. NOT
    // actually idempotent by the design's own "行数不变" reading of "幂等").
    // `schedule_a` here is §2's own already-fetched row for this key — no
    // extra round trip needed to capture the "before" id.
    let before_id = schedule_a["id"].clone();
    let after = wait_for_one_schedule_row(&d3, &key_a, sync_timeout);
    assert_eq!(
        after["id"], before_id,
        "kernel-side idempotency must preserve the SAME schedule id across a redundant \
         double upsert, not just \"still exactly one row\": before {before_id}, after {after}"
    );

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
    let key_b = format!("routine.{}", routine_b_id.to_ascii_lowercase());
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

    // T3.5.1 review (H2): the SAME forgery as §5's standalone check, this
    // time through the REAL mounted daemon — a client can never reach
    // `POST /_a24/scheduler/fired` through the public proxy either, even
    // with a valid daemon token and a REAL (routine_b's own) key. The 404
    // comes from a DIFFERENT layer than §5's: the kernel's own
    // reserved-path judgement (`agent24_os_proto::proxy::judge`/
    // `PathVerdict::Reserved` — ME4-1.3.2, design §7.1: the first path
    // segment under a module's namespace named `_a24` is refused BEFORE
    // admission, module dispatch, or even the daemon-token check), not
    // Sin90's own router (which, in mounted mode, DOES register this route
    // — see `crate::http::router`'s own doc; a client simply can never get
    // a request to it through the public proxy at all).
    let (status, body) = http_call(
        d3.port,
        "POST",
        "/api/v1/sin90/_a24/scheduler/fired",
        Some(&d3.token),
        None,
        Some(&format!(
            r#"{{"key":"{key_b}","trigger":"tick","scheduled_for":"2026-01-01T00:00:00Z","fired_at":"2026-01-01T00:00:00Z"}}"#
        )),
    )
    .unwrap();
    assert_eq!(
        status, 404,
        "a forged fired POST through the REAL mounted daemon must 404 (kernel reserved-path \
         judgement, ME4-1.3.2): {body}"
    );
    let fired_after_forgery = routine_fired_events(&d3, &routine_b_id);
    assert_eq!(
        fired_after_forgery.len(),
        2,
        "a forged fired POST through the real daemon must not create a new routine.fired: \
         {fired_after_forgery:?}"
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
    // T3.5.1 review (H1): the kernel's own `user_suspended` flag alone would
    // read `true` here regardless of whether the reconciler ran, or ran
    // correctly — `upsert` never clears it (`wait_for_today_kernel_suspended`'s
    // own doc). Wait for SIN90'S OWN observable side effect first: this
    // proves the startup full reconcile actually ran, took the
    // `user_suspended` branch, and correctly mapped the kernel's
    // lower-cased key back onto routine_a_id's real, uppercase form.
    wait_for_today_kernel_suspended(&d4, &routine_a_id, sync_timeout);
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

/// **T4.4.1 — real-mount acceptance**: a finalized Review's derived-summary
/// `memory.remember` outbox row actually lands in the real kernel's private
/// memory, through the REAL `adapter_agent24::reconciler` pump — and is
/// findable again via `_a24/memory/private/recall` on its exact `dedup_key`
/// (`review:<id>`), the task's own acceptance bar.
///
/// The WRITE side is entirely ordinary production code, no test-hooks
/// involved: `POST /reviews` -> `PATCH /reviews/{id}` -> `POST
/// /reviews/{id}/finalize` through the real proxy, which is
/// `store::repo::finalize_review` enqueueing onto `sin90_outbox`, drained by
/// the real `spawn_pump_loop` this same mounted `sin90` process is already
/// running (`main.rs::run_as_agent24_module`) — nothing about landing the
/// memory needs `test-hooks` at all. Only the READ-back verification does:
/// the shipped binary has no other route an ordinary HTTP client could use
/// to drive `_a24/memory/private/recall` with an arbitrary query, so this
/// reuses `POST /debug/kernel-roundtrip` (same route
/// `kernel_clients_roundtrip` above uses) via its `memory_recall_query`
/// field (`adapter_agent24::kernel_roundtrip`, added alongside this test) —
/// polled in a loop because the pump lands the row asynchronously
/// (`outbox_notify` wakes it promptly, but this test does not assume a
/// specific latency). Each poll also re-runs that route's OWN fixed probe
/// (remember/approval/scheduler) as a side effect — accepted, same as
/// `kernel_clients_roundtrip`'s own "not cleaned up, throwaway `$HOME` only"
/// posture (that route's module doc); this test asserts nothing about those
/// fields, only about `memory.recall_extra`.
///
/// **T4.4.1 review M4**: beyond "found at least one," this test also
/// asserts the `dedup_key` query returns EXACTLY one match, and — after
/// dropping this mounted `sin90` process and starting a genuinely fresh one
/// against the SAME `$HOME`/`sin90.db` (a real restart, not just a second
/// call) — that the SAME query still returns exactly one match with the
/// SAME kernel-minted id. This is the idempotency claim's real end-to-end
/// proof: `store::repo::finalize_review` can only ever enqueue this
/// `dedup_key` once (that function's own doc), but a restart exercises the
/// reconciler's OWN pump starting fresh against already-`done` rows, which
/// is exactly the scenario `adapter_agent24::reconciler::remember_review_summary`'s
/// `recall` pre-check exists to keep safe.
#[test]
#[ignore = "needs a sibling Agent24 checkout; run explicitly: cargo test --test agent24_mount_blackbox -- --ignored --test-threads=1"]
fn t441_finalized_review_summary_is_recallable_from_kernel_memory() {
    let checkout = agent24_checkout().unwrap_or_else(|| {
        panic!(
            "no Agent24 checkout found (set AGENT24_CHECKOUT or place it at ../Agent24) — this \
             test must FAIL, not silently skip, when its prerequisite is missing"
        )
    });
    let agent24d_bin = build_agent24d(&checkout);
    let sin90_bin = build_sin90_with_test_hooks();

    let home = tmp_home("t441");

    // Same "nothing already mounted" guard the other two tests use.
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
        "cannot run this test: agent24d still has an in-process \"sin90\" module compiled in: \
         {body}"
    );
    drop(d1);

    install_sin90(&home.join(".agent24/packages"), &sin90_bin);

    let d2 = start_daemon(&home, &agent24d_bin, &[]);
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

    let deadline = Instant::now() + Duration::from_secs(30);
    loop {
        if let Some((status, _)) = http_get(d2.port, Some(&d2.token), "/api/v1/sin90/today") {
            if status == 200 {
                break;
            }
        }
        assert!(
            Instant::now() < deadline,
            "sin90 never answered /today through the real proxy; daemon log:\n{}",
            d2.combined_log()
        );
        std::thread::sleep(Duration::from_millis(100));
    }

    // ---- create, write, finalize a real Review through the real proxy ----
    let (status, body) = http_call(
        d2.port,
        "POST",
        "/api/v1/sin90/reviews",
        Some(&d2.token),
        Some(&human_key),
        Some(r#"{"kind":"daily","period":"2026-09-24"}"#),
    )
    .unwrap();
    assert_eq!(status, 201, "POST /reviews through the real proxy: {body}");
    let review: serde_json::Value = serde_json::from_str(&body).unwrap();
    let review_id = review["id"].as_str().unwrap().to_owned();

    let (status, body) = http_call(
        d2.port,
        "PATCH",
        &format!("/api/v1/sin90/reviews/{review_id}"),
        Some(&d2.token),
        Some(&human_key),
        Some(r#"{"body":"T4.4.1 real-mount acceptance summary"}"#),
    )
    .unwrap();
    assert_eq!(
        status, 200,
        "PATCH /reviews/{{id}} through the real proxy: {body}"
    );

    let (status, body) = http_call(
        d2.port,
        "POST",
        &format!("/api/v1/sin90/reviews/{review_id}/finalize"),
        Some(&d2.token),
        Some(&human_key),
        Some("{}"),
    )
    .unwrap();
    assert_eq!(
        status, 200,
        "POST /reviews/{{id}}/finalize through the real proxy: {body}"
    );

    // ---- poll the debug route's `memory.recall_extra` until the real
    //      reconciler pump has landed the memory (or the deadline passes) --
    let dedup_key = format!("review:{review_id}");
    let recall_body = format!(r#"{{"memory_recall_query":"{dedup_key}"}}"#);

    // T4.4.1 review M4: every matching item this `dedup_key` query returns,
    // queried against whichever daemon `d` is currently up — used both to
    // find the FIRST landing (poll loop below) and, after a restart, to
    // prove the SAME query still returns EXACTLY one match (a local
    // closure, not a new top-level helper, so this stays entirely inside
    // this one test).
    let matches_for = |d: &Daemon, human_key: &str| -> Vec<serde_json::Value> {
        let (status, body) = http_call(
            d.port,
            "POST",
            "/api/v1/sin90/debug/kernel-roundtrip",
            Some(&d.token),
            Some(human_key),
            Some(&recall_body),
        )
        .unwrap_or_else(|| {
            panic!(
                "no response from the debug kernel-roundtrip route; daemon log:\n{}",
                d.combined_log()
            )
        });
        assert_eq!(
            status,
            200,
            "POST /debug/kernel-roundtrip through the real proxy: {body}; daemon log:\n{}",
            d.combined_log()
        );
        let result: serde_json::Value = serde_json::from_str(&body).unwrap();
        result["memory"]["recall_extra"]["items"]
            .as_array()
            .unwrap_or_else(|| panic!("no memory.recall_extra.items in response: {result}"))
            .iter()
            .filter(|it| it["body"]["dedup_key"] == dedup_key)
            .cloned()
            .collect()
    };

    let deadline = Instant::now() + Duration::from_secs(20);
    let found = loop {
        let matches = matches_for(&d2, &human_key);
        if let Some(item) = matches.first() {
            break item.clone();
        }
        assert!(
            Instant::now() < deadline,
            "the real reconciler pump never landed the memory.remember row for {dedup_key} \
             within the deadline; daemon log:\n{}",
            d2.combined_log()
        );
        std::thread::sleep(Duration::from_millis(500));
    };

    // ── exact-id association (this test's own acceptance bar) ──────────────
    assert_eq!(found["body"]["review_id"], review_id);
    assert_eq!(found["body"]["review_kind"], "daily");
    assert_eq!(found["body"]["period"], "2026-09-24");
    assert_eq!(
        found["body"]["summary"],
        "T4.4.1 real-mount acceptance summary"
    );
    assert!(
        found["id"]
            .as_str()
            .is_some_and(|id| id.starts_with("osmem:")),
        "kernel-minted memory id has an unexpected shape: {found}"
    );

    // ── T4.4.1 review M4: exactly ONE memory for this dedup_key — not two,
    //    not more (each poll above also re-ran the debug route's own fixed
    //    probe, which never touches THIS dedup_key, so it cannot have
    //    inflated this count). ────────────────────────────────────────────
    let matches = matches_for(&d2, &human_key);
    assert_eq!(
        matches.len(),
        1,
        "expected exactly one memory for dedup_key {dedup_key}, found {matches:?}"
    );

    // ── T4.4.1 review M4: restart sin90 (a fresh process, same `$HOME`/
    //    packages/keys/`sin90.db` — the Review is already `finalized` and
    //    its outbox row already `done` from BEFORE the restart, so nothing
    //    re-enqueues or re-sends `remember`) and prove the SAME query still
    //    finds EXACTLY one match — a restart must not somehow duplicate the
    //    memory a previous generation already landed. ─────────────────────
    drop(d2);
    let d3 = start_daemon(&home, &agent24d_bin, &[]);
    let deadline = Instant::now() + Duration::from_secs(30);
    loop {
        if os_list_entry(&d3, "sin90")["state"] == "mounted" {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "sin90 never re-mounted after restart; daemon log:\n{}",
            d3.combined_log()
        );
        std::thread::sleep(Duration::from_millis(100));
    }
    let human_key3 = d3.read_actor_key(&home, "human", Duration::from_secs(10));
    let deadline = Instant::now() + Duration::from_secs(30);
    loop {
        if let Some((status, _)) = http_get(d3.port, Some(&d3.token), "/api/v1/sin90/today") {
            if status == 200 {
                break;
            }
        }
        assert!(
            Instant::now() < deadline,
            "sin90 never answered /today after restart; daemon log:\n{}",
            d3.combined_log()
        );
        std::thread::sleep(Duration::from_millis(100));
    }
    let matches_after_restart = matches_for(&d3, &human_key3);
    assert_eq!(
        matches_after_restart.len(),
        1,
        "restart must not duplicate the memory: expected exactly one match for {dedup_key}, \
         found {matches_after_restart:?}"
    );
    assert_eq!(
        matches_after_restart[0]["id"], found["id"],
        "same memory, same kernel-minted id"
    );
}

/// **T5.5.1 — M5 real-mount acceptance** (DESIGN-LIFEOS.md §6 M5 / §11.7's
/// J23-J25 spirit, narrowed to the task's own literal text since the
/// OFFICIAL binary this test mounts is compiled `ai::ports::MODEL_ACCESS ==
/// LocalOnly` unconditionally — `domain-os.yml` never declares
/// `model_access: remote_allowed`, §2 #26's hard constraint — so there is no
/// `executive` path to exercise here at all; that is package B's job,
/// J16/J23, out of scope for this task).
///
/// Two generations of the SAME mounted daemon, sharing one Direction
/// candidate fixture (`"Quarterly Budget Review"`, no Area) but each its own
/// inbox Task and its own `OMLX_URL`/`OLLAMA_URL`, so each is a genuine,
/// independent trigger of the real `POST /ai/classify` -> real `_a24/model/
/// complete` -> real local model provider path — through the real
/// `agent24d` proxy end to end, not an in-process fake `ModelPort`:
///
/// - **generation "up"**: `OMLX_URL` points at a real local HTTP stub
///   ([`ModelStub`]) that answers the classify schema with the fixture's
///   only candidate key (`"d1"`) — task's own wording "在挂载黑盒里把
///   `OMLX_URL` 指向本地桩". `OLLAMA_URL` points at a SECOND, also-reachable
///   stub that would happily answer too, standing in for "a remote-ish
///   second provider" — asserting it receives ZERO hits is this run's own
///   "远端桩收到的请求数为 0" positive-path proof: even with a second live
///   endpoint configured, the ladder's own provider order (`agent24-models::
///   ModelRouter::from_env`: oMLX before Ollama) means it is never dialed
///   once the first Local-tier provider already answered.
/// - **generation "down"**: BOTH `OMLX_URL`/`OLLAMA_URL` point at ports
///   nothing listens on (`pick_free_port()`, dropped — task's own "环境变量
///   指向 127.0.0.1 的一个关闭端口") — every `_a24/model/complete` call this
///   generation makes therefore comes back `ModelError::Unavailable` (a
///   connect failure), which `model_callback::map_model_error` (Agent24)
///   maps to the wire's `{cause: no_provider, retryable: true}` closed set
///   (verified by reading that function directly, not assumed) — i.e. this
///   generation is the task's own explicit mutation instruction ("让
///   ModelClient 永远返回 Unavailable") realized as a real, running
///   scenario, not a hypothetical: `ai::ladder::ModelFailure::action()`
///   degrades `Unavailable` to the next step (§11.3.4), which for `classify`
///   is reflex fallback R2 — the fixture's task title is chosen to overlap
///   the ONE candidate's title unambiguously (`r2_reflex`'s own "unique
///   winner" rule) so R2 is guaranteed decisive, proving "classify 仍产出
///   提议" (task's own wording) genuinely holds when the network is down,
///   not merely that nothing crashed.
///
/// Both generations' produced proposal must additionally satisfy, checked
/// directly against the real mounted daemon's OWN `sin90.db` file (not the
/// HTTP-serialized view — the task's own literal SQL):
/// `SELECT count(*) FROM sin90_proposals WHERE source NOT IN
/// ('local_brain','executive','rule')` = 0 — plus a table-snapshot diff
/// bracketing each classify run (this test's own stand-in for the task's
/// "AI 运行期间，直写路由的调用次数为 0": DESIGN-LIFEOS.md §11.7's own J25
/// entry notes that literal call-count is vacuously true for an in-process
/// AI module and prescribes the table-snapshot instead — this test brings
/// that judgment to the REAL mount, not just the in-process unit test
/// `store::ai_port::ai_boundary_tables_unchanged` already covers).
///
/// Non-vacuousness (task's own "不能是永真断言"): the very end of this test
/// deliberately corrupts a COPY of the real data (a `sqlite` `TEMP TABLE`,
/// opened only after every daemon here has already exited — DESIGN-LIFEOS.md
/// §11.7's own J24 prescribes exactly this "在库的副本里改掉一行 source ->
/// 同一查询 = 1" technique) and re-runs the SAME SQL, and separately proves
/// [`diff_snapshot_keys`] genuinely detects a changed table against
/// hand-built maps — both checks are shown capable of failing, not just
/// shown to currently pass.
#[test]
#[ignore = "needs a sibling Agent24 checkout; run explicitly: cargo test --test agent24_mount_blackbox -- --ignored --test-threads=1"]
fn t551_ai_v1_m5_real_mount_acceptance() {
    let checkout = agent24_checkout().unwrap_or_else(|| {
        panic!(
            "no Agent24 checkout found (set AGENT24_CHECKOUT or place it at ../Agent24) — this \
             test must FAIL, not silently skip, when its prerequisite is missing"
        )
    });
    let agent24d_bin = build_agent24d(&checkout);
    let sin90_bin = PathBuf::from(env!("CARGO_BIN_EXE_sin90"));

    let classify_choice_d1 =
        serde_json::json!({"choice": "d1", "confidence": "high", "reason": "matches Finance"})
            .to_string();

    let home = tmp_home("m5-ai");

    // Same "nothing already mounted" guard every other test in this file
    // uses (T11 context — see `sin90_mounts_under_a_real_agent24_daemon`'s
    // own doc).
    let d0 = start_daemon(&home, &agent24d_bin, &[]);
    let (status, body) = http_get(d0.port, Some(&d0.token), "/api/v1/os").unwrap();
    assert_eq!(status, 200, "{body}");
    let before: serde_json::Value = serde_json::from_str(&body).unwrap();
    assert!(
        before["modules"]
            .as_array()
            .unwrap()
            .iter()
            .all(|m| m["name"] != "sin90"),
        "cannot run this test: agent24d still has an in-process \"sin90\" module compiled in: \
         {body}"
    );
    drop(d0);

    install_sin90(&home.join(".agent24/packages"), &sin90_bin);

    // ── generation "up": OMLX_URL -> a real local stub ──────────────────────
    let omlx_up = ModelStub::start(&classify_choice_d1);
    // A second, ALSO reachable stub (would answer if ever asked) standing in
    // for "a remote-ish second provider" — see this test's own doc for why
    // asserting zero hits on THIS is the real, positive-path half of "远端桩
    // 收到的请求数为 0".
    let ollama_up = ModelStub::start(&classify_choice_d1);
    let omlx_up_url = format!("http://127.0.0.1:{}", omlx_up.port);
    let ollama_up_url = format!("http://127.0.0.1:{}", ollama_up.port);
    let d_up = start_daemon(
        &home,
        &agent24d_bin,
        &[
            ("OMLX_URL", omlx_up_url.as_str()),
            ("OLLAMA_URL", ollama_up_url.as_str()),
        ],
    );
    wait_for_sin90_ready(&d_up, Duration::from_secs(30));
    let human_key = d_up.read_actor_key(&home, "human", Duration::from_secs(10));

    // One shared Direction candidate for BOTH generations (same `home`, same
    // `sin90.db` — a real restart of the SAME data, same convention every
    // other multi-generation test in this file already uses).
    let (status, body) = http_call(
        d_up.port,
        "POST",
        "/api/v1/sin90/directions",
        Some(&d_up.token),
        Some(&human_key),
        Some(r#"{"title":"Quarterly Budget Review","target_window":"2026-Q4"}"#),
    )
    .unwrap();
    assert_eq!(status, 201, "POST /directions: {body}");
    let direction: serde_json::Value = serde_json::from_str(&body).unwrap();
    // Not read back directly (the fixture only needs the candidate to
    // EXIST — `classify` resolves it internally via `AiReadModel::
    // direction_candidates`); kept named, not `_`, so a future assertion
    // that DOES want to check the accepted op's `direction_id` has an
    // obvious variable to reach for instead of re-parsing `direction` again.
    let _direction_id = direction["id"].as_str().unwrap().to_owned();

    // Task 1: R2's own "unique winner" fixture (`ai::classify::tests::
    // r2_reflex_ascii_word_overlap_picks_unique_winner`) — "quarterly"/
    // "budget" both length >= 3 with a letter, both appear in the ONE
    // candidate's title, so reflex R2 (used by generation "down" below)
    // would ALSO decide this one correctly; generation "up"'s stub answers
    // regardless of title, so this fixture serves both generations without
    // needing two different Direction candidates.
    let (status, body) = http_call(
        d_up.port,
        "POST",
        "/api/v1/sin90/tasks",
        Some(&d_up.token),
        Some(&human_key),
        Some(r#"{"title":"Draft the quarterly budget numbers","direction_id":null,"parent_task_id":null}"#),
    )
    .unwrap();
    assert_eq!(status, 201, "POST /tasks (task 1): {body}");
    let task1: serde_json::Value = serde_json::from_str(&body).unwrap();
    let task1_id = task1["id"].as_str().unwrap().to_owned();

    // Snapshot brackets ONLY the classify run itself — fixture creation
    // above (Direction + Task) is real, EXPECTED direct writes and must not
    // be counted against "AI 运行期间直写路由调用数为 0".
    let db = Db::open_readonly(&home);
    let before_up = db.snapshot_all_tables();

    let (status, body) = http_call(
        d_up.port,
        "POST",
        "/api/v1/sin90/ai/classify",
        Some(&d_up.token),
        Some(&human_key),
        Some(&format!(r#"{{"task_ids":["{task1_id}"]}}"#)),
    )
    .unwrap();
    assert_eq!(status, 202, "POST /ai/classify (task 1): {body}");
    let run1: serde_json::Value = serde_json::from_str(&body).unwrap();
    let run1_id = run1["run_id"].as_str().unwrap().to_owned();

    let rec1 = wait_for_ai_run_done(&d_up, &run1_id, Duration::from_secs(30));
    assert_eq!(rec1["state"], "done", "{rec1}");
    assert_eq!(rec1["items"][0]["target"], task1_id, "{rec1}");
    assert_eq!(
        rec1["items"][0]["result"], "proposed",
        "classify with a reachable local stub must produce a proposal: {rec1}"
    );
    let calls1 = rec1["calls"].as_array().unwrap();
    // `plan()` always schedules `Step::ReflexDecisive` FIRST for `classify`
    // regardless of model presence (`ai::ladder::plan`'s own doc) — with no
    // prior classification history for this brand-new title, R1 is
    // "undecided" (`ok=0`, not a hard failure, §11.3.5 L5), THEN the
    // reachable local model step actually decides it. Two rows, not one.
    assert_eq!(
        calls1.len(),
        2,
        "reflex-decisive (undecided) + one reachable model step -> exactly two call rows: \
         {calls1:?}"
    );
    assert_eq!(calls1[0]["engine"], "reflex", "{calls1:?}");
    assert_eq!(calls1[0]["ok"], false, "{calls1:?}");
    assert_eq!(calls1[0]["error_kind"], "undecided", "{calls1:?}");
    assert_eq!(calls1[1]["ok"], true, "{calls1:?}");
    assert_eq!(calls1[1]["engine"], "local", "{calls1:?}");
    assert_eq!(calls1[1]["served_tier"], "local", "{calls1:?}");
    let proposal1_id = calls1[1]["proposal_id"].as_str().unwrap().to_owned();

    let after_up = db.snapshot_all_tables();

    // ── "OMLX_URL 指向本地桩" was genuinely dialed, not just configured ────
    assert!(
        omlx_up.hits() > 0,
        "the local model stub must have received at least one request"
    );
    // ── the positive-path half of "远端桩收到的请求数为 0" ──────────────────
    assert_eq!(
        ollama_up.hits(),
        0,
        "the second (reachable) stub must never be dialed once the first Local-tier provider \
         already answered"
    );

    // ── source check (task's own literal SQL) ───────────────────────────────
    assert_eq!(
        db.scalar_i64(
            "SELECT count(*) FROM sin90_proposals \
             WHERE source NOT IN ('local_brain','executive','rule')"
        ),
        0
    );
    // `scalar_i64` takes no bind params of its own — inline the id directly;
    // a proposal id is this crate's own `ulid()`-derived string (see
    // `core::ulid`), never attacker-controlled input in this test.
    assert_eq!(
        db.scalar_i64(&format!(
            "SELECT count(*) FROM sin90_proposals WHERE id = '{proposal1_id}' AND source = \
             'local_brain'"
        )),
        1,
    );

    // ── table-snapshot stand-in for "直写路由调用数为 0" ─────────────────────
    let allowed_up: std::collections::HashSet<&str> = [
        "sin90_proposals",
        "sin90_ai_calls",
        "sin90_events(entity=proposal)",
    ]
    .into_iter()
    .collect();
    let diff_up = diff_snapshot_keys(&before_up, &after_up);
    assert!(
        !diff_up.is_empty(),
        "the classify run must have changed SOMETHING (a proposal + a call row) — an empty \
         diff here would mean the snapshot itself never actually saw the run's writes"
    );
    assert!(
        diff_up.iter().all(|k| allowed_up.contains(k.as_str())),
        "classify (generation \"up\") touched a table outside {{sin90_proposals, \
         sin90_ai_calls, sin90_events(entity=proposal)}} — a direct write, not a Proposal: \
         {diff_up:?}"
    );

    drop(db);
    drop(d_up);

    // ── generation "down": both providers genuinely unreachable ────────────
    let dead_omlx = pick_free_port();
    let dead_ollama = pick_free_port();
    let dead_omlx_url = format!("http://127.0.0.1:{dead_omlx}");
    let dead_ollama_url = format!("http://127.0.0.1:{dead_ollama}");
    let d_down = start_daemon(
        &home,
        &agent24d_bin,
        &[
            ("OMLX_URL", dead_omlx_url.as_str()),
            ("OLLAMA_URL", dead_ollama_url.as_str()),
        ],
    );
    wait_for_sin90_ready(&d_down, Duration::from_secs(30));
    let human_key = d_down.read_actor_key(&home, "human", Duration::from_secs(10));

    // Task 2: a DIFFERENT title, same "unique winner" property against the
    // SAME one Direction candidate (`direction_id` from generation "up",
    // persisted in the same `sin90.db`) — "budget" is the shared word.
    let (status, body) = http_call(
        d_down.port,
        "POST",
        "/api/v1/sin90/tasks",
        Some(&d_down.token),
        Some(&human_key),
        Some(r#"{"title":"Second look at the budget plan","direction_id":null,"parent_task_id":null}"#),
    )
    .unwrap();
    assert_eq!(status, 201, "POST /tasks (task 2): {body}");
    let task2: serde_json::Value = serde_json::from_str(&body).unwrap();
    let task2_id = task2["id"].as_str().unwrap().to_owned();

    let db = Db::open_readonly(&home);
    let before_down = db.snapshot_all_tables();

    let (status, body) = http_call(
        d_down.port,
        "POST",
        "/api/v1/sin90/ai/classify",
        Some(&d_down.token),
        Some(&human_key),
        Some(&format!(r#"{{"task_ids":["{task2_id}"]}}"#)),
    )
    .unwrap();
    assert_eq!(status, 202, "POST /ai/classify (task 2): {body}");
    let run2: serde_json::Value = serde_json::from_str(&body).unwrap();
    let run2_id = run2["run_id"].as_str().unwrap().to_owned();

    let rec2 = wait_for_ai_run_done(&d_down, &run2_id, Duration::from_secs(30));
    assert_eq!(rec2["state"], "done", "{rec2}");
    assert_eq!(rec2["items"][0]["target"], task2_id, "{rec2}");
    assert_eq!(
        rec2["items"][0]["result"], "proposed",
        "the task's own acceptance bar — classify must STILL produce a proposal with both \
         local providers unreachable (falls back to reflex R2): {rec2}"
    );
    let calls2 = rec2["calls"].as_array().unwrap();
    // Reflex-decisive (undecided, same reasoning as generation "up") + one
    // degraded model step (ok=0) + one successful reflex FALLBACK (ok=1):
    // three rows, not two.
    assert_eq!(
        calls2.len(),
        3,
        "reflex-decisive (undecided) + one degraded model step (ok=0) + one successful reflex \
         fallback (ok=1): {calls2:?}"
    );
    assert_eq!(calls2[0]["engine"], "reflex", "{calls2:?}");
    assert_eq!(calls2[0]["ok"], false, "{calls2:?}");
    assert_eq!(calls2[0]["error_kind"], "undecided", "{calls2:?}");
    assert_eq!(calls2[1]["engine"], "local", "{calls2:?}");
    assert_eq!(calls2[1]["ok"], false, "{calls2:?}");
    assert_eq!(
        calls2[1]["error_kind"], "unavailable.no_provider",
        "a genuine connect failure to BOTH local providers must map to the SAME closed-set \
         wire cause J1's own in-process fixture uses: {calls2:?}"
    );
    assert_eq!(calls2[2]["engine"], "reflex", "{calls2:?}");
    assert_eq!(calls2[2]["fallback_from"], "local", "{calls2:?}");
    assert_eq!(calls2[2]["ok"], true, "{calls2:?}");
    let proposal2_id = calls2[2]["proposal_id"].as_str().unwrap().to_owned();

    let after_down = db.snapshot_all_tables();

    assert_eq!(
        db.scalar_i64(
            "SELECT count(*) FROM sin90_proposals \
             WHERE source NOT IN ('local_brain','executive','rule')"
        ),
        0
    );
    // Sharper than the aggregate NOT-IN check above: `source_for(Reflex, _)`
    // (`ai::ladder`) must specifically derive `'rule'`, not merely
    // "something in the allowed set" — a bug that mapped a reflex-produced
    // proposal to `'local_brain'` instead would still pass the aggregate
    // check above but must fail THIS one (verified by hand: flipping that
    // arm and re-running this test turns exactly this assertion red, see
    // the PR notes).
    assert_eq!(
        db.scalar_i64(&format!(
            "SELECT count(*) FROM sin90_proposals WHERE id = '{proposal2_id}' AND source = 'rule'"
        )),
        1,
        "the reflex-fallback-produced proposal must have source = 'rule', not just some \
         allowed value"
    );

    let diff_down = diff_snapshot_keys(&before_down, &after_down);
    assert!(!diff_down.is_empty());
    assert!(
        diff_down.iter().all(|k| allowed_up.contains(k.as_str())),
        "classify (generation \"down\", reflex-only) touched a table outside the allowed set: \
         {diff_down:?}"
    );

    drop(db);
    drop(d_down);

    // ── non-vacuousness (task's own "不能是永真断言") ────────────────────────
    //
    // (1) diff_snapshot_keys against hand-built maps: proves the comparison
    //     itself can see a real difference, not just "always empty".
    let mut fake_before = BTreeMap::new();
    fake_before.insert("sin90_tasks".to_string(), vec!["row-1".to_string()]);
    let mut fake_after = fake_before.clone();
    fake_after.insert("sin90_tasks".to_string(), vec!["row-1-mutated".to_string()]);
    assert_eq!(
        diff_snapshot_keys(&fake_before, &fake_before),
        Vec::<String>::new()
    );
    assert_eq!(
        diff_snapshot_keys(&fake_before, &fake_after),
        vec!["sin90_tasks".to_string()]
    );

    // (2) the source-membership SQL against a corrupted COPY of the REAL
    //     data — DESIGN-LIFEOS.md §11.7's own J24 technique. Opened only
    //     AFTER every daemon above has already exited: a plain (non-
    //     read-only) connection to the SAME file, but every write below
    //     lands in a `TEMP TABLE` — SQLite's temp storage is a private,
    //     separate database even on an ordinary connection, so this can
    //     never touch `sin90.db`'s own real tables.
    let db_path = find_file(&home, "sin90.db").unwrap();
    let rt = tokio::runtime::Runtime::new().unwrap();
    let pool = rt.block_on(async {
        let opts = <sqlx::sqlite::SqliteConnectOptions as std::str::FromStr>::from_str(&format!(
            "sqlite://{}",
            db_path.display()
        ))
        .unwrap();
        sqlx::sqlite::SqlitePoolOptions::new()
            .max_connections(1)
            .connect_with(opts)
            .await
            .unwrap()
    });
    rt.block_on(async {
        sqlx::query("CREATE TEMP TABLE mut_proposals AS SELECT * FROM sin90_proposals")
            .execute(&pool)
            .await
            .unwrap();
        // Corrupt exactly the row this test itself created in generation
        // "up" — known-good before mutation (already asserted `source =
        // 'local_brain'` above), so this is a controlled, single-row flip.
        sqlx::query("UPDATE mut_proposals SET source = 'bogus_source' WHERE id = ?")
            .bind(&proposal1_id)
            .execute(&pool)
            .await
            .unwrap();
    });
    let bad_count: (i64,) = rt
        .block_on(
            sqlx::query_as(
                "SELECT count(*) FROM mut_proposals \
                 WHERE source NOT IN ('local_brain','executive','rule')",
            )
            .fetch_one(&pool),
        )
        .unwrap();
    assert_eq!(
        bad_count.0, 1,
        "the source-membership query must be able to see a real violation on a corrupted copy, \
         not just always answer 0"
    );
}

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
    /// into the run (see `wait_for_generated_key`), not just at startup.
    stdout: Arc<Mutex<Vec<String>>>,
    stderr: Arc<Mutex<Vec<String>>>,
}

impl Daemon {
    fn combined_log(&self) -> String {
        let out = self.stdout.lock().unwrap().join("\n");
        let err = self.stderr.lock().unwrap().join("\n");
        format!("--- stdout ---\n{out}\n--- stderr ---\n{err}")
    }

    /// Sin90's `ActorKeys::from_env_or_generate` prints
    /// `"...generated for this run: <key>"` to its OWN stderr on first use
    /// when `SIN90_HUMAN_KEY` is unset — which it always is here, because
    /// Agent24's module launch only inherits a fixed env allowlist
    /// (`agent24_os_proto::launch::INHERITED_ENV`: PATH/HOME/LANG/TZ/TMPDIR/
    /// USER) and does not pass arbitrary `SIN90_*` variables through. The
    /// daemon pipes and re-logs every module stdout/stderr line
    /// (`agent24_os_proto::launch::drain_output`), which is where this test
    /// reads it back from — there is no other channel to learn it.
    fn wait_for_generated_key(&self, which: &str, timeout: Duration) -> String {
        let marker = format!("SIN90_{which}_KEY not set — generated for this run: ");
        let deadline = Instant::now() + timeout;
        loop {
            for line in self
                .stdout
                .lock()
                .unwrap()
                .iter()
                .chain(self.stderr.lock().unwrap().iter())
            {
                if let Some(pos) = line.find(&marker) {
                    // The key itself has no internal whitespace, but the REST of
                    // this log line does not end there: Agent24's daemon
                    // re-logs each captured stdout/stderr line with its own
                    // trailing `module="sin90" stream="stderr" cut=false`
                    // tracing fields appended on the SAME line — `.trim()`
                    // alone slurped those in as part of the "key", which then
                    // failed every actor-key check downstream with no error
                    // clearer than a generic 401/400. Take only the first
                    // whitespace-delimited token after the marker.
                    return line[pos + marker.len()..]
                        .split_whitespace()
                        .next()
                        .unwrap_or_default()
                        .to_owned();
                }
            }
            assert!(
                Instant::now() < deadline,
                "never saw a generated SIN90_{which}_KEY in the daemon's log; log:\n{}",
                self.combined_log()
            );
            std::thread::sleep(Duration::from_millis(50));
        }
    }
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

fn start_daemon(home: &Path, agent24d_bin: &Path) -> Daemon {
    let mut child = Command::new(agent24d_bin)
        .env_clear()
        .env("PATH", std::env::var_os("PATH").unwrap_or_default())
        .env("HOME", home)
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
    // capture (needed for `wait_for_generated_key`).
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
    let d1 = start_daemon(&home, &agent24d_bin);
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
    let d2 = start_daemon(&home, &agent24d_bin);
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
    let human_key = d2.wait_for_generated_key("HUMAN", Duration::from_secs(10));

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

//! Real-process black-box test: spawns the ACTUAL compiled `sin90` binary
//! (`env!("CARGO_BIN_EXE_sin90")`, not a function call into the library) in
//! `--standalone` mode, talks to it over real HTTP, and restarts it to prove
//! persistence — the parts of M0 §6's judgements that don't require a live
//! Agent24 daemon.
//!
//! This is NOT the full ME-3f-style black box (that needs a real Agent24
//! daemon, a real out-of-process mount, and a real callback socket — out of
//! this port's scope; see the PR description for what remains). What this
//! file DOES prove for real, through an actual OS process boundary and an
//! actual SQLite file on disk: the binary starts, serves real HTTP, and a
//! second process pointed at the same `--data-dir` sees what the first wrote.

use std::process::{Child, Command, Stdio};
use std::time::Duration;

struct Server {
    child: Child,
    port: u16,
}

impl Server {
    async fn spawn(data_dir: &std::path::Path) -> Self {
        let port = pick_free_port();
        let child = Command::new(env!("CARGO_BIN_EXE_sin90"))
            .args([
                "serve",
                "--port",
                &port.to_string(),
                "--data-dir",
                data_dir.to_str().unwrap(),
            ])
            .env("SIN90_HUMAN_KEY", "blackbox-human-key-0123456789abcdef")
            .env(
                "SIN90_AUTOMATION_KEY",
                "blackbox-automation-key-0123456789abcdef",
            )
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .expect("spawn the real sin90 binary");
        let server = Self { child, port };
        server.wait_until_ready().await;
        server
    }

    async fn wait_until_ready(&self) {
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        loop {
            if reqwest::get(format!("http://127.0.0.1:{}/areas", self.port))
                .await
                .is_ok()
            {
                return;
            }
            if std::time::Instant::now() > deadline {
                panic!("sin90 --standalone did not become ready in time");
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    }

    fn base(&self) -> String {
        format!("http://127.0.0.1:{}", self.port)
    }
}

impl Drop for Server {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

fn pick_free_port() -> u16 {
    // Bind to port 0 to ask the OS for a free one, then drop the listener —
    // there is a race if something else grabs it before the child binds, but
    // that is the same race every "find a free port for a test" trick has.
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    listener.local_addr().unwrap().port()
}

#[tokio::test]
async fn a_real_process_serves_http_and_a_second_process_sees_what_the_first_wrote() {
    let dir = std::env::temp_dir().join(format!("sin90-blackbox-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();

    let area_id: String;
    {
        let server = Server::spawn(&dir).await;
        let client = reqwest::Client::new();

        let resp = client
            .post(format!("{}/areas", server.base()))
            .header("x-sin90-actor-key", "blackbox-human-key-0123456789abcdef")
            .json(&serde_json::json!({"title": "Work"}))
            .send()
            .await
            .unwrap();
        assert_eq!(
            resp.status(),
            reqwest::StatusCode::CREATED,
            "{:?}",
            resp.text().await
        );
        let body: serde_json::Value = resp.json().await.unwrap();
        area_id = body["id"].as_str().unwrap().to_string();
    } // server dropped here -> process killed, exactly like a daemon restart

    // Second process, same --data-dir, no code change, no rebuild: the SAME
    // area must still be there.
    let server2 = Server::spawn(&dir).await;
    let areas: serde_json::Value = reqwest::get(format!("{}/areas", server2.base()))
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let ids: Vec<&str> = areas["areas"]
        .as_array()
        .unwrap()
        .iter()
        .map(|a| a["id"].as_str().unwrap())
        .collect();
    assert!(ids.contains(&area_id.as_str()), "{areas:?}");

    let _ = std::fs::remove_dir_all(&dir);
}

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

#[tokio::test]
async fn l1_edit_routes_persist_across_real_process_restart() {
    let dir = std::env::temp_dir().join(format!("sin90-l1-blackbox-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let client = reqwest::Client::new();
    let key = "blackbox-human-key-0123456789abcdef";
    let area_id;
    let direction_id;
    {
        let server = Server::spawn(&dir).await;
        let area: serde_json::Value = client
            .post(format!("{}/areas", server.base()))
            .header("x-sin90-actor-key", key)
            .json(&serde_json::json!({"title":"Health"}))
            .send()
            .await
            .unwrap()
            .error_for_status()
            .unwrap()
            .json()
            .await
            .unwrap();
        area_id = area["id"].as_str().unwrap().to_string();
        let resp = client
            .patch(format!("{}/areas/{area_id}", server.base()))
            .header("x-sin90-actor-key", key)
            .json(&serde_json::json!({"current_state":"现在", "ideal_state":"理想"}))
            .send()
            .await
            .unwrap();
        assert_eq!(resp.status(), 200);
        let direction: serde_json::Value = client
            .post(format!("{}/directions", server.base()))
            .header("x-sin90-actor-key", key)
            .json(&serde_json::json!({"title":"Run", "target_window":"2026-Q4"}))
            .send()
            .await
            .unwrap()
            .error_for_status()
            .unwrap()
            .json()
            .await
            .unwrap();
        direction_id = direction["id"].as_str().unwrap().to_string();
        let uri = format!("{}/directions/{direction_id}", server.base());
        let resp = client
            .patch(&uri)
            .header("x-sin90-actor-key", key)
            .json(&serde_json::json!({"status":"achieved"}))
            .send()
            .await
            .unwrap();
        assert_eq!(resp.status(), 409);
        let resp = client.patch(&uri).header("x-sin90-actor-key", key).json(&serde_json::json!({"title":"Jog", "status":"active", "target_window":"2027-Q1", "area_id":area_id})).send().await.unwrap();
        assert_eq!(resp.status(), 200);
    }
    {
        let server = Server::spawn(&dir).await;
        let areas: serde_json::Value = client
            .get(format!("{}/areas", server.base()))
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        let area = areas["areas"]
            .as_array()
            .unwrap()
            .iter()
            .find(|a| a["id"] == area_id)
            .unwrap();
        assert_eq!(area["current_state"], "现在");
        assert_eq!(area["ideal_state"], "理想");
        let directions: serde_json::Value = client
            .get(format!("{}/directions", server.base()))
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        let d = directions["directions"]
            .as_array()
            .unwrap()
            .iter()
            .find(|d| d["id"] == direction_id)
            .unwrap();
        assert_eq!(d["title"], "Jog");
        assert_eq!(d["status"], "active");
        assert_eq!(d["area_id"], area_id);
        assert_eq!(d["target_window"], "2027-Q1");
    }
    std::fs::remove_dir_all(dir).unwrap();
}

#[tokio::test]
async fn l2a_real_http_proposals_protect_edits_and_cascade_survives_restart() {
    use serde_json::{json, Value};
    async fn write(
        client: &reqwest::Client,
        method: reqwest::Method,
        url: String,
        body: Value,
        expected: u16,
    ) -> Value {
        let response = client
            .request(method, url)
            .header("x-sin90-actor-key", "blackbox-human-key-0123456789abcdef")
            .json(&body)
            .send()
            .await
            .unwrap();
        let status = response.status().as_u16();
        let result: Value = response.json().await.unwrap();
        assert_eq!(status, expected, "{result}");
        result
    }
    const PATCH: reqwest::Method = reqwest::Method::PATCH;
    const POST: reqwest::Method = reqwest::Method::POST;
    let dir = std::env::temp_dir().join(format!("sin90-l2a-blackbox-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let client = reqwest::Client::new();
    let direction_id;
    let human_year_id;
    {
        let server = Server::spawn(&dir).await;
        let base = server.base();
        let direction = write(
            &client,
            POST,
            format!("{base}/directions"),
            json!({"title":"Run", "target_window":"2026"}),
            201,
        )
        .await;
        direction_id = direction["id"].as_str().unwrap().to_string();
        write(
            &client,
            PATCH,
            format!("{base}/directions/{direction_id}"),
            json!({"goal_3y":"Marathon", "goal_from_year":2026}),
            200,
        )
        .await;
        let year = write(&client, POST, format!("{base}/period-plans"), json!({"direction_id":direction_id, "kind":"year", "period":"2026", "target":"Human annual goal"}), 201).await;
        human_year_id = year["id"].as_str().unwrap().to_string();
        let quarter = write(&client, POST, format!("{base}/period-plans"), json!({"direction_id":direction_id, "kind":"quarter", "period":"2026-Q1", "parent_id":human_year_id, "target":"Quarter"}), 201).await;
        write(&client, POST, format!("{base}/period-plans"), json!({"direction_id":direction_id, "kind":"month", "period":"2026-01", "parent_id":quarter["id"], "target":"Month"}), 201).await;
        write(&client, POST, format!("{base}/period-plans"), json!({"direction_id":direction_id, "kind":"month", "period":"2026-13", "parent_id":quarter["id"], "target":"Invalid"}), 400).await;
        write(&client, POST, format!("{base}/period-plans"), json!({"direction_id":direction_id, "kind":"month", "period":"2026-02", "parent_id":human_year_id, "target":"Invalid parent"}), 400).await;
        write(
            &client,
            POST,
            format!("{base}/proposals"),
            json!({"id":"seed", "status":"pending", "source":"local_brain", "ops":[
                {"op":"upsert_period_plans", "direction_id":direction_id, "plans":[
                    {"kind":"year", "period":"2026", "target":"Skip human"},
                    {"kind":"year", "period":"2027", "target":"AI annual goal"}
                ]}
            ]}),
            202,
        )
        .await;
        let receipt = write(
            &client,
            POST,
            format!("{base}/proposals/seed/accept"),
            json!({}),
            200,
        )
        .await;
        assert_eq!(receipt["skipped_period_plans"][0]["id"], human_year_id);
        let plans: Value = client
            .get(format!("{base}/period-plans?kind=year&period=2027"))
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        let ai_id = plans["period_plans"][0]["id"].as_str().unwrap();
        write(&client, POST, format!("{base}/proposals"), json!({"id":"pending", "status":"pending", "source":"local_brain", "ops":[
            {"op":"set_direction_goal", "direction_id":direction_id, "goal_3y":"Ultra", "from_year":2027},
            {"op":"upsert_period_plans", "direction_id":direction_id, "plans":[
                {"kind":"year", "period":"2027", "target":"Stale AI goal"},
                {"kind":"year", "period":"2028", "target":"New AI goal"}
            ]}
        ]}), 202).await;
        write(
            &client,
            PATCH,
            format!("{base}/period-plans/{ai_id}"),
            json!({"target":"Human revision"}),
            200,
        )
        .await;
        let receipt = write(
            &client,
            POST,
            format!("{base}/proposals/pending/accept"),
            json!({}),
            200,
        )
        .await;
        assert_eq!(receipt["skipped_period_plans"][0]["id"], ai_id);
        write(
            &client,
            PATCH,
            format!("{base}/period-plans/{human_year_id}"),
            json!({"status":"dropped"}),
            200,
        )
        .await;
    }
    {
        let server = Server::spawn(&dir).await;
        let plans: Value = client
            .get(format!(
                "{}/period-plans?direction_id={direction_id}",
                server.base()
            ))
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        let plans = plans["period_plans"].as_array().unwrap();
        assert_eq!(plans.len(), 5);
        for plan in plans {
            let period = plan["period"].as_str().unwrap();
            if period.starts_with("2026") {
                assert_eq!(plan["status"], "dropped");
            }
            if period == "2027" {
                assert_eq!(plan["target"], "Human revision");
                assert_eq!(plan["source"], "user");
            }
            if period == "2028" {
                assert_eq!(plan["target"], "New AI goal");
                assert_eq!(plan["source"], "ai");
            }
        }
        let directions: Value = client
            .get(format!("{}/directions", server.base()))
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        let d = directions["directions"]
            .as_array()
            .unwrap()
            .iter()
            .find(|d| d["id"] == direction_id)
            .unwrap();
        assert_eq!(d["goal_3y"], "Ultra");
        assert_eq!(d["goal_from_year"], 2027);
    }
    std::fs::remove_dir_all(dir).unwrap();
}

#[tokio::test]
async fn l3_progress_real_http_and_persistent_settings() {
    let dir = std::env::temp_dir().join(format!("sin90-l3-blackbox-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let server = Server::spawn(&dir).await;
    let client = reqwest::Client::new();
    let key = "blackbox-human-key-0123456789abcdef";
    let d: serde_json::Value = client
        .post(format!("{}/directions", server.base()))
        .header("x-sin90-actor-key", key)
        .json(&serde_json::json!({"title":"Run", "target_window":"2026"}))
        .send()
        .await
        .unwrap()
        .error_for_status()
        .unwrap()
        .json()
        .await
        .unwrap();
    let direction = d["id"].as_str().unwrap();
    let mut parent: Option<String> = None;
    for (kind, period) in [
        ("year", "2026"),
        ("quarter", "2026-Q1"),
        ("month", "2026-03"),
    ] {
        let plan: serde_json::Value = client.post(format!("{}/period-plans", server.base()))
            .header("x-sin90-actor-key", key)
            .json(&serde_json::json!({"direction_id":direction, "kind":kind, "period":period,"parent_id":parent,"target":"Run", "metric":"count", "metric_target":31}))
            .send().await.unwrap().error_for_status().unwrap().json().await.unwrap();
        parent = Some(plan["id"].as_str().unwrap().to_owned());
    }
    let progress: serde_json::Value = client
        .get(format!(
            "{}/progress?direction_id={direction}&at=2026-03-15",
            server.base()
        ))
        .send()
        .await
        .unwrap()
        .error_for_status()
        .unwrap()
        .json()
        .await
        .unwrap();
    let month = progress["progress"]
        .as_array()
        .unwrap()
        .iter()
        .find(|p| p["plan_id"].as_str() == parent.as_deref())
        .unwrap();
    assert_eq!(month["status"], "at_risk");
    assert_eq!(month["actual"], 0);
    assert_eq!(month["miss_streak"], 3);
    assert_eq!(month["expected"], 15.0);
    client
        .put(format!("{}/settings/progress", server.base()))
        .header("x-sin90-actor-key", key)
        .json(&serde_json::json!({"streak_weeks":4}))
        .send()
        .await
        .unwrap()
        .error_for_status()
        .unwrap();
    drop(server);
    let server = Server::spawn(&dir).await;
    let settings: serde_json::Value = client
        .get(format!("{}/settings/progress", server.base()))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(settings["streak_weeks"], 4);
    let progress: serde_json::Value = client
        .get(format!(
            "{}/progress?direction_id={direction}&at=2026-03-15",
            server.base()
        ))
        .send()
        .await
        .unwrap()
        .error_for_status()
        .unwrap()
        .json()
        .await
        .unwrap();
    let month = progress["progress"]
        .as_array()
        .unwrap()
        .iter()
        .find(|p| p["plan_id"].as_str() == parent.as_deref())
        .unwrap();
    assert_eq!(month["status"], "behind");
    drop(server);
    std::fs::remove_dir_all(&dir).unwrap();
}

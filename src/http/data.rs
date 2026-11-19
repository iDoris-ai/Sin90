use super::*;
use crate::store::data::{templates, Backup};
use serde_json::{json, Value};
pub(super) async fn status(State(state): State<Sin90State>) -> Response {
    match state.store.data_status().await {
        Ok(v) => Json(v).into_response(),
        Err(e) => map_err(e),
    }
}
pub(super) async fn catalog() -> Json<Value> {
    Json(templates())
}
pub(super) async fn export(State(state): State<Sin90State>, headers: HeaderMap) -> Response {
    if let Err(r) = state.require_human(&headers) {
        return r;
    }
    match state.store.export_data().await {
        Ok(v) => Json(v).into_response(),
        Err(e) => map_err(e),
    }
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct Import {
    backup: Backup,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct Reset {
    template: Option<String>,
    lang: Option<String>,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct Restore {
    name: String,
}
pub(super) async fn import(
    State(state): State<Sin90State>,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    if let Err(r) = state.require_human(&headers) {
        return r;
    }
    let req: Import = match parse(&body, "backup") {
        Ok(v) => v,
        Err(r) => return r,
    };
    finish(
        &state,
        state.store.import_data(&req.backup).await,
        "data.imported",
    )
}
pub(super) async fn reset(
    State(state): State<Sin90State>,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    if let Err(r) = state.require_human(&headers) {
        return r;
    }
    let req: Reset = match parse(&body, "reset") {
        Ok(v) => v,
        Err(r) => return r,
    };
    finish(
        &state,
        state
            .store
            .reset_data(req.template.as_deref(), req.lang.as_deref())
            .await,
        "data.reset",
    )
}
pub(super) async fn restore(
    State(state): State<Sin90State>,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    if let Err(r) = state.require_human(&headers) {
        return r;
    }
    let req: Restore = match parse(&body, "restore") {
        Ok(v) => v,
        Err(r) => return r,
    };
    finish(
        &state,
        state.store.restore_backup(&req.name).await,
        "data.restored",
    )
}
fn finish(state: &Sin90State, result: crate::store::Result<()>, event: &str) -> Response {
    match result {
        Ok(()) => {
            state.emit(event, json!({}));
            Json(json!({"ok":true})).into_response()
        }
        Err(e) => map_err(e),
    }
}

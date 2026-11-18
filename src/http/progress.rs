use super::*;
use crate::store::progress::ProgressSettings;

#[derive(Deserialize)]
pub(super) struct ProgressQuery {
    direction_id: String,
    at: Option<String>,
}
pub(super) async fn get(
    State(state): State<Sin90State>,
    Query(q): Query<ProgressQuery>,
) -> Response {
    match state.store.progress(&q.direction_id, q.at.as_deref()).await {
        Ok(progress) => Json(serde_json::json!({"progress": progress})).into_response(),
        Err(e) => map_err(e),
    }
}
pub(super) async fn settings(State(state): State<Sin90State>) -> Response {
    match state.store.get_progress_settings().await {
        Ok(s) => Json(s).into_response(),
        Err(e) => map_err(e),
    }
}
pub(super) async fn put_settings(
    State(state): State<Sin90State>,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    if let Err(r) = state.require_human(&headers) {
        return r;
    }
    let settings: ProgressSettings = match parse(&body, "progress settings") {
        Ok(s) => s,
        Err(r) => return r,
    };
    match state.store.put_progress_settings(settings).await {
        Ok(s) => {
            state.emit(
                "setting.changed",
                serde_json::json!({"key":"progress.streak_weeks", "value":s.streak_weeks}),
            );
            Json(s).into_response()
        }
        Err(e) => map_err(e),
    }
}

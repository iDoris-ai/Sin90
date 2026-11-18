use super::*;
use crate::store::reviews::ReviewSettings;
pub(super) async fn get(State(state): State<Sin90State>) -> Response {
    match state.store.get_review_settings().await {
        Ok(s) => Json(s).into_response(),
        Err(e) => map_err(e),
    }
}
pub(super) async fn put(
    State(state): State<Sin90State>,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    if let Err(r) = state.require_human(&headers) {
        return r;
    }
    let settings: ReviewSettings = match parse(&body, "review settings") {
        Ok(s) => s,
        Err(r) => return r,
    };
    match state.store.put_review_settings(settings).await {
        Ok(s) => {
            state.emit("settings.review.updated", serde_json::json!(s));
            Json(s).into_response()
        }
        Err(e) => map_err(e),
    }
}

//! `/api/v1/*`: request parsing, dispatch, and the JSON shapes clients see. The shapes are this
//! API's own (camelCase, milliseconds), not the UI's, so the UI contract can keep changing freely.

use std::convert::Infallible;
use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::time::Duration;

use http_body_util::{BodyExt, Full, Limited};
use hyper::body::{Bytes, Incoming};
use hyper::{header, Method, Request, Response, StatusCode};
use innertube::{BrowseItem, Rating, SongItem};
use serde::de::DeserializeOwned;
use serde::Deserialize;
use serde_json::{json, Value};

use super::auth::{self, Denied};
use super::{Ctx, API_VERSION};
use crate::state::{AppState, RepeatMode};

type Body = Full<Bytes>;

/// Search and playlist loads go to YouTube; past this the client gets a 504 instead of hanging.
const HANDLER_TIMEOUT: Duration = Duration::from_secs(15);
const MAX_BODY: usize = 16 * 1024;

#[derive(Debug)]
pub(super) struct ApiError {
    status: StatusCode,
    code: &'static str,
    message: String,
}

impl ApiError {
    fn new(status: StatusCode, code: &'static str, message: impl Into<String>) -> Self {
        Self { status, code, message: message.into() }
    }
    fn bad_request(message: impl Into<String>) -> Self {
        Self::new(StatusCode::BAD_REQUEST, "BAD_REQUEST", message)
    }
    fn not_found(message: impl Into<String>) -> Self {
        Self::new(StatusCode::NOT_FOUND, "NOT_FOUND", message)
    }
    /// YouTube (or the player) said no.
    fn upstream(message: impl Into<String>) -> Self {
        Self::new(StatusCode::BAD_GATEWAY, "UPSTREAM_ERROR", message)
    }
    fn not_logged_in() -> Self {
        Self::new(StatusCode::CONFLICT, "NOT_LOGGED_IN", "Sign in to Limusic first")
    }
}

type Res = Result<Value, ApiError>;

fn respond(status: StatusCode, body: &Value) -> Response<Body> {
    let mut r = Response::new(Full::new(Bytes::from(body.to_string())));
    *r.status_mut() = status;
    r.headers_mut()
        .insert(header::CONTENT_TYPE, header::HeaderValue::from_static("application/json"));
    r.headers_mut().insert(header::CACHE_CONTROL, header::HeaderValue::from_static("no-store"));
    r
}

fn error_response(e: ApiError) -> Response<Body> {
    respond(e.status, &json!({ "error": { "code": e.code, "message": e.message } }))
}

pub(super) async fn serve(
    req: Request<Incoming>,
    state: Arc<AppState>,
    ctx: Ctx,
) -> Result<Response<Body>, Infallible> {
    if ctx.stopped.load(Ordering::SeqCst) {
        return Ok(error_response(ApiError::new(
            StatusCode::SERVICE_UNAVAILABLE,
            "DISABLED",
            "The local API was turned off",
        )));
    }
    let token = ctx.token.read().unwrap().clone();
    if let Err(denied) = auth::check(req.headers(), ctx.port, &token) {
        let e = match denied {
            Denied::Origin => ApiError::new(
                StatusCode::FORBIDDEN,
                "FORBIDDEN_ORIGIN",
                "Browser requests are not allowed",
            ),
            Denied::Host => {
                ApiError::new(StatusCode::FORBIDDEN, "FORBIDDEN_HOST", "Bad Host header")
            }
            Denied::Token => ApiError::new(
                StatusCode::UNAUTHORIZED,
                "UNAUTHORIZED",
                "Missing or wrong bearer token",
            ),
        };
        return Ok(error_response(e));
    }
    let result = match tokio::time::timeout(HANDLER_TIMEOUT, route(req, &state)).await {
        Ok(r) => r,
        Err(_) => {
            Err(ApiError::new(StatusCode::GATEWAY_TIMEOUT, "TIMEOUT", "Limusic took too long"))
        }
    };
    Ok(match result {
        Ok(v) => respond(StatusCode::OK, &v),
        Err(e) => error_response(e),
    })
}

async fn route(req: Request<Incoming>, state: &Arc<AppState>) -> Res {
    let method = req.method().clone();
    let path = req.uri().path().to_owned();
    let query = req.uri().query().unwrap_or("").to_owned();
    let Some(rest) = path.strip_prefix("/api/v1") else {
        return Err(ApiError::not_found("Unknown route"));
    };
    let segs: Vec<&str> = rest.split('/').filter(|s| !s.is_empty()).collect();
    let body = req.into_body();

    match (&method, segs.as_slice()) {
        (&Method::GET, ["status"]) => Ok(json!({
            "version": env!("CARGO_PKG_VERSION"),
            "apiVersion": API_VERSION,
        })),

        // --- player ---
        (&Method::GET, ["player"]) => Ok(player_json(state).await),
        (&Method::POST, ["player", "play"]) => {
            if !is_playing(state).await {
                state.resume_or_toggle().await;
            }
            ok()
        }
        (&Method::POST, ["player", "pause"]) => {
            state.player.pause().map_err(|e| ApiError::upstream(e.to_string()))?;
            ok()
        }
        (&Method::POST, ["player", "toggle"]) => {
            state.resume_or_toggle().await;
            ok()
        }
        (&Method::POST, ["player", "next"]) => {
            state.next_in_queue().await;
            ok()
        }
        (&Method::POST, ["player", "previous"]) => {
            state.prev_in_queue().await;
            ok()
        }
        (&Method::POST, ["player", "seek"]) => {
            #[derive(Deserialize)]
            #[serde(rename_all = "camelCase")]
            struct B {
                position_ms: f64,
            }
            let b: B = read_json(body).await?;
            if !b.position_ms.is_finite() || b.position_ms < 0.0 {
                return Err(ApiError::bad_request("positionMs must be >= 0"));
            }
            state.user_seek(b.position_ms / 1000.0).await.map_err(ApiError::upstream)?;
            ok()
        }
        (&Method::POST, ["player", "volume"]) => {
            #[derive(Deserialize)]
            struct B {
                volume: i64,
            }
            let b: B = read_json(body).await?;
            if !(0..=100).contains(&b.volume) {
                return Err(ApiError::bad_request("volume must be 0-100"));
            }
            crate::commands::apply_volume(state, b.volume).map_err(ApiError::upstream)?;
            ok()
        }
        (&Method::POST, ["player", "shuffle"]) => {
            // No body (or no `enabled`) toggles, which is what a hotkey wants.
            #[derive(Deserialize, Default)]
            struct B {
                enabled: Option<bool>,
            }
            let b: B = read_json_or_default(body).await?;
            let on = queue_value(state).await["shuffle"].as_bool().unwrap_or(false);
            if b.enabled.map_or(true, |want| want != on) {
                state.toggle_shuffle().await;
            }
            ok()
        }
        (&Method::POST, ["player", "repeat"]) => {
            #[derive(Deserialize, Default)]
            struct B {
                mode: Option<RepeatMode>,
            }
            let b: B = read_json_or_default(body).await?;
            match b.mode {
                Some(mode) => state.set_repeat(mode).await,
                None => state.cycle_repeat().await,
            }
            ok()
        }

        // --- search / play ---
        (&Method::GET, ["search"]) => search(state, &query).await,
        (&Method::POST, ["play"]) => {
            #[derive(Deserialize)]
            #[serde(rename_all = "camelCase")]
            struct B {
                #[serde(rename = "type")]
                kind: String,
                id: String,
                #[serde(default)]
                shuffle: bool,
                start_index: Option<usize>,
            }
            let b: B = read_json(body).await?;
            play(state, &b.kind, &b.id, b.shuffle, b.start_index).await?;
            ok()
        }

        // --- queue ---
        (&Method::GET, ["queue"]) => {
            let (items, current) = queue_items(state).await;
            Ok(json!({
                "currentIndex": current,
                "tracks": items.iter().map(track_json).collect::<Vec<_>>(),
            }))
        }
        (&Method::POST, ["queue"]) => {
            #[derive(Deserialize)]
            #[serde(rename_all = "camelCase")]
            struct B {
                video_id: String,
                #[serde(default)]
                position: Position,
            }
            #[derive(Deserialize, Default)]
            #[serde(rename_all = "lowercase")]
            enum Position {
                Next,
                #[default]
                End,
            }
            let b: B = read_json(body).await?;
            let item = song(state, &b.video_id).await?;
            match b.position {
                Position::Next => state.play_next(vec![item], None).await,
                Position::End => state.add_to_queue(vec![item], None, None).await,
            }
            ok()
        }
        (&Method::POST, ["queue", "jump"]) => {
            #[derive(Deserialize)]
            struct B {
                index: usize,
            }
            let b: B = read_json(body).await?;
            queue_index_exists(state, b.index).await?;
            state.play_index(b.index).await;
            ok()
        }
        (&Method::DELETE, ["queue", index]) => {
            let index: usize =
                index.parse().map_err(|_| ApiError::bad_request("index must be a number"))?;
            queue_index_exists(state, index).await?;
            state.remove_from_queue(index).await;
            ok()
        }

        // --- library ---
        (&Method::GET, ["library", "playlists"]) => {
            let items =
                crate::commands::library_playlists(state).await.map_err(ApiError::upstream)?;
            Ok(json!({ "playlists": items.iter().map(browse_json).collect::<Vec<_>>() }))
        }
        (&Method::GET, ["library", "playlists", id]) => {
            let id = decode(id);
            let page =
                crate::commands::playlist_page(state, &id).await.map_err(ApiError::upstream)?;
            Ok(json!({
                "id": id,
                "title": page.title,
                "subtitle": page.subtitle,
                "thumbnailUrl": page.thumbnail.as_deref().map(crate::media::cover_url),
                "tracks": page.items.iter().map(track_json).collect::<Vec<_>>(),
            }))
        }

        // --- rating ---
        (&Method::POST, ["track", "like"]) => {
            #[derive(Deserialize)]
            #[serde(rename_all = "camelCase")]
            struct B {
                /// Defaults to the playing track.
                video_id: Option<String>,
                liked: bool,
            }
            let b: B = read_json(body).await?;
            let video_id = match b.video_id {
                Some(id) => id,
                None => current_item(state)
                    .await
                    .map(|i| i.video_id)
                    .ok_or_else(|| ApiError::not_found("Nothing is playing"))?,
            };
            let rating = if b.liked { Rating::Like } else { Rating::Indifferent };
            crate::commands::rate_track(state, &video_id, rating).await.map_err(|e| {
                if state.it.is_logged_in() {
                    ApiError::upstream(e)
                } else {
                    ApiError::not_logged_in()
                }
            })?;
            ok()
        }

        (_, _) => Err(ApiError::not_found("Unknown route")),
    }
}

fn ok() -> Res {
    Ok(json!({ "ok": true }))
}

// --- body + query helpers ---

async fn read_bytes(body: Incoming) -> Result<Bytes, ApiError> {
    Limited::new(body, MAX_BODY)
        .collect()
        .await
        .map(|c| c.to_bytes())
        .map_err(|_| ApiError::bad_request("Body too large or unreadable"))
}

async fn read_json<T: DeserializeOwned>(body: Incoming) -> Result<T, ApiError> {
    let bytes = read_bytes(body).await?;
    serde_json::from_slice(&bytes).map_err(|e| ApiError::bad_request(format!("Invalid JSON: {e}")))
}

async fn read_json_or_default<T: DeserializeOwned + Default>(
    body: Incoming,
) -> Result<T, ApiError> {
    let bytes = read_bytes(body).await?;
    if bytes.iter().all(u8::is_ascii_whitespace) {
        return Ok(T::default());
    }
    serde_json::from_slice(&bytes).map_err(|e| ApiError::bad_request(format!("Invalid JSON: {e}")))
}

fn decode(s: &str) -> String {
    let s = s.replace('+', " ");
    urlencoding::decode(&s).map(|c| c.into_owned()).unwrap_or(s)
}

fn query_param(query: &str, key: &str) -> Option<String> {
    query
        .split('&')
        .filter_map(|kv| kv.split_once('='))
        .find(|(k, _)| *k == key)
        .map(|(_, v)| decode(v))
}

// --- state readers ---

async fn queue_value(state: &AppState) -> Value {
    state.queue_snapshot().await
}

async fn queue_items(state: &AppState) -> (Vec<SongItem>, usize) {
    let q = queue_value(state).await;
    let items: Vec<SongItem> = serde_json::from_value(q["items"].clone()).unwrap_or_default();
    let current = q["currentIndex"].as_u64().unwrap_or(0) as usize;
    (items, current)
}

async fn current_item(state: &AppState) -> Option<SongItem> {
    let (items, current) = queue_items(state).await;
    items.into_iter().nth(current)
}

async fn queue_index_exists(state: &AppState, index: usize) -> Result<(), ApiError> {
    let (items, _) = queue_items(state).await;
    if index < items.len() {
        Ok(())
    } else {
        Err(ApiError::not_found(format!("No queue entry at index {index}")))
    }
}

async fn is_playing(state: &AppState) -> bool {
    !state.playback_snapshot().await["paused"].as_bool().unwrap_or(true)
}

async fn player_json(state: &AppState) -> Value {
    let pb = state.playback_snapshot().await;
    let q = queue_value(state).await;
    let (items, current) = queue_items(state).await;
    let mut track = items.get(current).map(track_json).unwrap_or(Value::Null);
    // The queue row's duration is YouTube's "3:45"; the player's own is exact once mpv knows it.
    if let (Some(t), Some(secs)) = (track.as_object_mut(), pb["duration"].as_f64()) {
        if secs > 0.0 {
            t.insert("durationMs".into(), json!((secs * 1000.0).round() as i64));
        }
    }
    json!({
        "track": track,
        "positionMs": (pb["position"].as_f64().unwrap_or(0.0) * 1000.0).round() as i64,
        "isPlaying": !pb["paused"].as_bool().unwrap_or(true),
        "volume": pb["volume"],
        "shuffle": q["shuffle"].as_bool().unwrap_or(false),
        "repeat": q["repeat"],
    })
}

// --- YouTube-backed helpers ---

fn metadata_client(state: &AppState) -> Result<&innertube::YouTubeClient, ApiError> {
    state
        .clients
        .get(innertube::METADATA_CLIENT)
        .ok_or_else(|| ApiError::upstream("metadata client missing"))
}

/// A full `SongItem` for a bare videoId; `/next` is the call that answers with its own metadata
/// (same as the `song` command).
async fn song(state: &AppState, video_id: &str) -> Result<SongItem, ApiError> {
    let client = metadata_client(state)?;
    let next = state
        .it
        .next(client, Some(video_id), None)
        .await
        .map_err(|e| ApiError::upstream(e.to_string()))?;
    next.items
        .into_iter()
        .find(|i| i.video_id == video_id)
        .ok_or_else(|| ApiError::not_found(format!("No song {video_id}")))
}

async fn search(state: &AppState, query: &str) -> Res {
    let q = query_param(query, "q").unwrap_or_default();
    let q = q.trim();
    if q.is_empty() {
        return Err(ApiError::bad_request("q is required"));
    }
    let kind = query_param(query, "type").unwrap_or_else(|| "songs".into());
    let client = metadata_client(state)?;
    let results = match kind.as_str() {
        // Not recorded in the account's search history: a launcher types as it searches.
        "songs" => state
            .it
            .search_songs(client, q, false)
            .await
            .map_err(|e| ApiError::upstream(e.to_string()))?
            .items
            .iter()
            .map(|i| {
                let mut v = track_json(i);
                v["type"] = json!("song");
                v
            })
            .collect::<Vec<_>>(),
        "albums" | "artists" | "playlists" => state
            .it
            .search_cards(client, q, &kind)
            .await
            .map_err(|e| ApiError::upstream(e.to_string()))?
            .iter()
            .map(browse_json)
            .collect(),
        other => return Err(ApiError::bad_request(format!("unknown type: {other}"))),
    };
    Ok(json!({ "type": kind, "results": results }))
}

async fn play(
    state: &Arc<AppState>,
    kind: &str,
    id: &str,
    shuffle: bool,
    start: Option<usize>,
) -> Result<(), ApiError> {
    match kind {
        "song" => {
            let item = song(state, id).await?;
            state.play_song(item).await;
        }
        "album" => {
            let page = crate::commands::album_page(state, id).await.map_err(ApiError::upstream)?;
            if page.items.is_empty() {
                return Err(ApiError::not_found("Album has no tracks"));
            }
            state
                .play_tracks(page.items, start, Some(id.to_owned()), page.title, shuffle, None)
                .await;
        }
        "playlist" => {
            let page =
                crate::commands::playlist_page(state, id).await.map_err(ApiError::upstream)?;
            if page.items.is_empty() {
                return Err(ApiError::not_found("Playlist has no tracks"));
            }
            state
                .play_tracks(
                    page.items,
                    start,
                    Some(id.to_owned()),
                    page.title,
                    shuffle,
                    page.continuation,
                )
                .await;
        }
        "artist" => {
            // An artist has no track list of its own; YouTube's answer to "play this artist" is
            // the artist radio, which is what the artist page's play button starts too.
            state.start_radio("artist", id, None).await.map_err(ApiError::upstream)?;
        }
        other => return Err(ApiError::bad_request(format!("unknown type: {other}"))),
    }
    Ok(())
}

// --- output shapes ---

/// "3:45" or "1:02:03" to milliseconds.
fn parse_duration_ms(s: &str) -> Option<i64> {
    let mut total: i64 = 0;
    for part in s.trim().split(':') {
        total = total * 60 + part.parse::<i64>().ok()?;
    }
    Some(total * 1000)
}

fn track_json(i: &SongItem) -> Value {
    json!({
        "id": i.video_id,
        "title": i.title,
        "artists": i.artists,
        "album": i.album,
        "albumId": i.album_id,
        "durationMs": i.duration.as_deref().and_then(parse_duration_ms),
        "thumbnailUrl": i.thumbnail.as_deref().map(crate::media::cover_url),
        "liked": i.rating.map(|r| r == Rating::Like),
    })
}

fn browse_json(b: &BrowseItem) -> Value {
    json!({
        "type": b.kind,
        "id": b.id,
        "title": b.title,
        "subtitle": b.subtitle,
        "thumbnailUrl": b.thumbnail.as_deref().map(crate::media::cover_url),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn durations_parse() {
        assert_eq!(parse_duration_ms("3:45"), Some(225_000));
        assert_eq!(parse_duration_ms("1:02:03"), Some(3_723_000));
        assert_eq!(parse_duration_ms("live"), None);
    }

    #[test]
    fn query_params_decode() {
        let q = "q=daft+punk%20one&type=albums";
        assert_eq!(query_param(q, "q").as_deref(), Some("daft punk one"));
        assert_eq!(query_param(q, "type").as_deref(), Some("albums"));
        assert_eq!(query_param(q, "missing"), None);
    }

    #[test]
    fn errors_serialize_in_the_documented_shape() {
        let r = error_response(ApiError::not_logged_in());
        assert_eq!(r.status(), StatusCode::CONFLICT);
        assert_eq!(r.headers()[header::CONTENT_TYPE], "application/json");
    }

    #[test]
    fn repeat_modes_parse_lowercase() {
        let m: RepeatMode = serde_json::from_str("\"one\"").unwrap();
        assert_eq!(m, RepeatMode::One);
    }
}

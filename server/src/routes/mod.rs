mod blobs;
mod device;
mod floor_plans;
mod groups;
mod media;
mod pairing;
mod playlists;
mod schedule;
mod screens;
mod system;
mod ws;

use axum::{
    extract::DefaultBodyLimit,
    http::Request,
    routing::{get, post, put},
    Router,
};
use tower_http::trace::TraceLayer;

use crate::{error::ApiError, state::AppState};

pub fn router(state: AppState) -> Router {
    Router::new()
        .route("/v1/info", get(system::info))
        .route("/v1/auth/config", get(system::auth_config))
        .route("/v1/me", get(system::me))
        .route("/v1/groups", get(groups::list).post(groups::create))
        .route("/v1/groups/{id}", get(groups::get).put(groups::update).delete(groups::delete))
        .route(
            "/v1/blobs/{sha}",
            get(blobs::get).head(blobs::get).put(blobs::put).layer(DefaultBodyLimit::disable()),
        )
        .route("/v1/media", get(media::list).post(media::create))
        .route("/v1/media/{id}", get(media::get).patch(media::patch).delete(media::delete))
        .route("/v1/playlists", get(playlists::list).post(playlists::create))
        .route(
            "/v1/playlists/{id}",
            get(playlists::get).put(playlists::update).delete(playlists::delete),
        )
        .route("/v1/screens", get(screens::list))
        .route("/v1/screens/{id}", get(screens::get).patch(screens::patch).delete(screens::delete))
        .route("/v1/screens/{id}/command", post(screens::command))
        .route("/v1/screens/{id}/override", put(screens::set_override).delete(screens::clear_override))
        .route("/v1/schedule", get(schedule::list).post(schedule::create))
        .route("/v1/schedule/timeline", get(schedule::timeline))
        .route("/v1/schedule/{id}", put(schedule::update).delete(schedule::delete))
        .route("/v1/floor-plans", get(floor_plans::list).post(floor_plans::create))
        .route("/v1/floor-plans/{id}", put(floor_plans::update).delete(floor_plans::delete))
        .route("/v1/pairing", post(pairing::start))
        .route("/v1/pairing/claim", post(pairing::claim))
        .route("/v1/pairing/{id}", get(pairing::poll))
        .route("/v1/device/manifest", get(device::manifest))
        .route("/v1/device/playlists", get(device::playlists))
        .route("/v1/device/playlist", put(device::select_playlist))
        .route("/v1/ws", get(ws::upgrade))
        .fallback(|| async { ApiError::not_found() })
        .layer(TraceLayer::new_for_http().make_span_with(|req: &Request<_>| {
            tracing::info_span!("http", method = %req.method(), path = %req.uri().path())
        }))
        .with_state(state)
}

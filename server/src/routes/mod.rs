mod blobs;
mod device;
mod floor_plans;
mod folders;
mod notices;
mod tags;
mod teams;
mod media;
mod org;
pub mod nodes;
mod assignments;
mod pairing;
mod playlists;
mod schedule;
mod screens;
mod system;
mod ws;

use axum::{
    extract::DefaultBodyLimit,
    http::Request,
    routing::{get, patch, post, put},
    Router,
};
use tower_http::trace::TraceLayer;

use crate::{error::ApiError, state::AppState};

pub fn router(state: AppState) -> Router {
    Router::new()
        .route("/v1/info", get(system::info))
        .route("/v1/auth/config", get(system::auth_config))
        .route("/v1/me", get(system::me).patch(system::patch_me))
        .route("/v1/me/organizations", get(org::mine))
        .route("/v1/organizations", post(org::create))
        .route("/v1/invites/{id}/accept", post(org::accept_invite))
        .route("/v1/settings", get(org::get_settings).patch(org::patch_settings))
        .route("/v1/teams", get(teams::list).post(teams::create))
        .route("/v1/teams/reach", get(teams::reach_matrix))
        .route("/v1/teams/{id}", get(teams::get).put(teams::update).delete(teams::delete))
        .route("/v1/teams/{id}/tags", put(teams::put_tags))
        .route("/v1/tags", get(tags::list).post(tags::create))
        .route("/v1/tags/{id}", patch(tags::patch).delete(tags::delete))
        .route("/v1/tags/{id}/usage", get(tags::usage))
        .route(
            "/v1/blobs/{sha}",
            get(blobs::get).head(blobs::get).put(blobs::put).layer(DefaultBodyLimit::disable()),
        )
        .route("/v1/media", get(media::list).post(media::create))
        .route("/v1/media/{id}", get(media::get).patch(media::patch).delete(media::delete))
        .route("/v1/playlists", get(playlists::list).post(playlists::create))
        .route(
            "/v1/playlists/{id}",
            get(playlists::get).put(playlists::update).patch(playlists::patch).delete(playlists::delete),
        )
        .route("/v1/playlists/{id}/tags", put(playlists::put_tags))
        .route("/v1/playlist-folders", get(folders::list).post(folders::create))
        .route("/v1/playlist-folders/{id}", patch(folders::patch).delete(folders::delete))
        .route("/v1/nodes", get(nodes::list))
        .route("/v1/nodes/{id}", get(nodes::get).patch(nodes::patch))
        .route("/v1/nodes/{id}/assignments", get(assignments::node_get).put(assignments::node_put))
        .route("/v1/screens/{id}/move", post(assignments::move_screen))
        .route("/v1/screens", get(screens::list))
        .route("/v1/screens/overrides", post(screens::bulk_override))
        .route("/v1/screens/{id}/tags", put(screens::put_tags))
        .route("/v1/screens/{id}/events", get(screens::events))
        .route("/v1/screens/{id}", get(screens::get).patch(screens::patch).delete(screens::delete))
        .route("/v1/screens/{id}/command", post(screens::command))
        .route("/v1/screens/{id}/override", put(screens::set_override).delete(screens::clear_override))
        .route("/v1/schedule", get(schedule::list).post(schedule::create))
        .route("/v1/schedule/timeline", get(schedule::timeline))
        .route("/v1/schedule/preview", post(schedule::preview))
        .route("/v1/notices", get(notices::list))
        .route("/v1/notices/{id}/dismiss", post(notices::dismiss))
        .route("/v1/schedule/{id}", put(schedule::update).delete(schedule::delete))
        .route("/v1/floor-plans", get(floor_plans::list).post(floor_plans::create))
        .route("/v1/floor-plans/{id}", put(floor_plans::update).delete(floor_plans::delete))
        .route("/v1/pairing", post(pairing::start))
        .route("/v1/pairing/claim", post(pairing::claim))
        .route("/v1/pairing/{id}", get(pairing::poll))
        .route("/v1/device/manifest", get(device::manifest))
        .route("/v1/device/playlists", get(device::playlists))
        .route("/v1/device/playlist", put(device::select_playlist))
        .route("/v1/device/node", get(device::node))
        .route("/v1/device/outputs", put(device::put_outputs))
        .route("/v1/device/screens", post(device::create_screen))
        .route("/v1/device/assignments", get(assignments::device_get).put(assignments::device_put))
        .route("/v1/device/mode", put(assignments::device_mode))
        .route("/v1/device/screens/{id}/command", post(assignments::device_command))
        .route("/v1/ws", get(ws::upgrade))
        .fallback(|| async { ApiError::not_found() })
        .layer(TraceLayer::new_for_http().make_span_with(|req: &Request<_>| {
            tracing::info_span!("http", method = %req.method(), path = %req.uri().path())
        }))
        .with_state(state)
}

pub mod assignments;
pub mod auth;
pub mod bonjour;
pub mod config;
pub mod error;
pub mod hub;
pub mod models;
pub mod notices;
pub mod reach;
pub mod routes;
pub mod schedule;
pub mod state;
pub mod util;

pub const BRAND_NAME: &str = env!("BRAND_NAME");
pub const BRAND_SLUG: &str = env!("BRAND_SLUG");
pub const BRAND_BONJOUR_TYPE: &str = env!("BRAND_BONJOUR_TYPE");
pub const BRAND_DEFAULT_PORT: &str = env!("BRAND_DEFAULT_PORT");

pub use routes::router;
pub use state::{build_state, AppState};

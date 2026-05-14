//! Axum app — application state, routes, middleware.

pub mod middleware;
pub mod routes;
pub mod state;

pub use self::state::AppState;

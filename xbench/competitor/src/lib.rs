//! Shared bench helpers — connection strings, schema setup, etc.
//!
//! The bench binaries (latency, throughput, `vector_knn`) and the
//! `smoke` ping-all binary all import `connection_strings` so any
//! port change happens in exactly one place.

pub mod write_shapes;

/// `(label, sqlx_connection_string)` for each competitor DB. Matches
/// the docker-compose.yml ports / credentials.
#[must_use]
pub fn connection_strings() -> Vec<(&'static str, String)> {
    vec![
        (
            "postgres",
            "postgres://bench:bench@127.0.0.1:25432/bench".to_string(),
        ),
        (
            "mysql",
            "mysql://bench:bench@127.0.0.1:23306/bench".to_string(),
        ),
        (
            "mariadb",
            "mysql://bench:bench@127.0.0.1:23307/bench".to_string(),
        ),
    ]
}

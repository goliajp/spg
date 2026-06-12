//! v7.20 test-speed A — spg-sqlx integration tests
//! merged into one target: one link instead of one per file,
//! libtest parallelises modules in-process.

mod e2e_decimal;
mod e2e_describe_real;
mod e2e_uuid_sqlx;
mod e2e_vector_tsvector;
mod fetch;
mod mailrs_round12;
mod mailrs_round20;
mod smoke;
mod snapshot_routing;
mod types;

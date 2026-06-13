//! v7.20 test-speed A — spg-embedded integration tests
//! merged into one target: one link instead of one per file,
//! libtest parallelises modules in-process.

mod e2e_auto_compact;
mod e2e_background_freezer;
mod e2e_chaos;
mod e2e_cold_tier_manifest;
mod e2e_file_lock;
mod e2e_freeze_visible;
mod e2e_metrics;
mod e2e_open_path;
mod e2e_persistence;
mod e2e_pitr_retention;
mod e2e_prepare_bind;
mod e2e_revert;
mod e2e_typed_query;
mod e2e_vector;
mod e2e_wal_v4_pitr;
mod e2e_with_transaction;
mod mailrs_round13;
mod mailrs_round14;
mod mailrs_round15_16;
mod mailrs_round17;
mod mailrs_round21;
mod mailrs_round24;
mod mailrs_round29;

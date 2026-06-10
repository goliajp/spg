//! v7.20 test-speed A — spg-embedded-tokio (group_commit stays standalone: timing-sensitive <128ms fsync-share assertion) integration tests
//! merged into one target: one link instead of one per file,
//! libtest parallelises modules in-process.

mod async_db;
mod prepare_bind;
mod read_handle;

//! redb table definitions (PLAN.md §5.1). All are created in one write txn at open.

use redb::TableDefinition;

/// cached-compatible entries: `key -> rmp([bin(envelope), created_at])`.
pub const ENTRIES: TableDefinition<&str, &[u8]> = TableDefinition::new("cached_disk_cache");
/// Job id (UUIDv7) -> rmp `JobRecord`.
pub const JOBS: TableDefinition<&str, &[u8]> = TableDefinition::new("ucg_jobs");
/// `(job id, row idx)` -> rmp `JobRow`.
pub const JOB_ROWS: TableDefinition<(&str, u32), &[u8]> = TableDefinition::new("ucg_job_rows");
/// `(job id, chunk idx)` -> rmp `ChunkState`.
pub const JOB_CHUNKS: TableDefinition<(&str, u32), &[u8]> = TableDefinition::new("ucg_job_chunks");
/// Counter name (`hits`, `misses`, ..., `hit:{key}`) -> count.
pub const STATS: TableDefinition<&str, u64> = TableDefinition::new("ucg_stats");
/// `benchmarks` | `vintages:{b}` | `layers:{v}` -> rmp `(bin(raw_json), fetched_at)`.
pub const CATALOG: TableDefinition<&str, &[u8]> = TableDefinition::new("ucg_catalog");
/// `(due unix secs, entry key)` -> attempts so far.
pub const RETRY_QUEUE: TableDefinition<(u64, &str), u8> = TableDefinition::new("ucg_retry_queue");
/// Dotted config path -> JSON value.
pub const CONFIG_OVERRIDES: TableDefinition<&str, &str> =
    TableDefinition::new("ucg_config_overrides");
/// `schema_version`, `dict_id`.
pub const META: TableDefinition<&str, &str> = TableDefinition::new("ucg_meta");

pub const SCHEMA_VERSION: &str = "1";

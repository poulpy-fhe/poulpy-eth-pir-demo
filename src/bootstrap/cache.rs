use std::collections::HashMap;
use std::path::{Path, PathBuf};

use alloy::primitives::{Address, B256};
use anyhow::{Context, Result};
use rusqlite::{Connection, OpenFlags, OptionalExtension, Transaction, params};

use crate::chain::Touch;
use crate::map::{BalanceMap, Entry, Reading};
use crate::tokens::{MAINNET_CHAIN_ID, USDC_DEPLOY_BLOCK, USDT_DEPLOY_BLOCK, USDT_INITIAL_OWNER};

pub const SCHEMA_VERSION: u64 = 1;
pub const BOOTSTRAP_IDENTITY: &str = concat!(
    "ethereum-mainnet-usdt-usdc-bootstrap-v1;",
    "usdt=dAC17F958D2ee523a2206206994597C13D831ec7@4634748;",
    "usdc=A0b86991c6218b36c1d19D4a2e9Eb0cE3606eB48@6082465;",
    "seed=36928500Bc1dCd7af6a2B4008875CC336b927D57@4634748;",
    "events=transfer-nonzero-nonself+usdt-destroy+issue-redeem-owner-eob"
);

const CREATE_SCHEMA: &str = r#"
CREATE TABLE metadata (
    singleton          INTEGER PRIMARY KEY CHECK(singleton = 1),
    schema_version     INTEGER NOT NULL CHECK(schema_version > 0),
    bootstrap_identity TEXT NOT NULL,
    phase              TEXT NOT NULL CHECK(phase IN ('Scanning','ReadingBalances','ReadyToCommit','Complete')),
    chain_id           INTEGER NOT NULL,
    state_path         TEXT NOT NULL,
    confirmations      INTEGER NOT NULL CHECK(confirmations > 0),
    target_block       INTEGER NOT NULL CHECK(target_block >= 0),
    target_hash        BLOB NOT NULL CHECK(typeof(target_hash) = 'blob' AND length(target_hash) = 32),
    scan_start         INTEGER NOT NULL CHECK(scan_start >= 0),
    scan_cursor        INTEGER NOT NULL CHECK(scan_cursor >= 0)
);
CREATE TABLE candidates (
    address       BLOB PRIMARY KEY NOT NULL CHECK(typeof(address) = 'blob' AND length(address) = 20),
    usdt_block    INTEGER NULL CHECK(usdt_block IS NULL OR usdt_block >= 0),
    usdc_block    INTEGER NULL CHECK(usdc_block IS NULL OR usdc_block >= 0),
    usdt_balance  BLOB NULL CHECK(usdt_balance IS NULL OR (typeof(usdt_balance) = 'blob' AND length(usdt_balance) = 16)),
    usdc_balance  BLOB NULL CHECK(usdc_balance IS NULL OR (typeof(usdc_balance) = 'blob' AND length(usdc_balance) = 16))
);
CREATE INDEX candidates_unread ON candidates(address)
WHERE usdt_balance IS NULL OR usdc_balance IS NULL;
"#;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Phase {
    Scanning,
    ReadingBalances,
    ReadyToCommit,
    Complete,
}

impl Phase {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Scanning => "Scanning",
            Self::ReadingBalances => "ReadingBalances",
            Self::ReadyToCommit => "ReadyToCommit",
            Self::Complete => "Complete",
        }
    }

    fn parse(value: &str) -> Result<Self> {
        match value {
            "Scanning" => Ok(Self::Scanning),
            "ReadingBalances" => Ok(Self::ReadingBalances),
            "ReadyToCommit" => Ok(Self::ReadyToCommit),
            "Complete" => Ok(Self::Complete),
            _ => anyhow::bail!("unknown bootstrap phase {value:?}"),
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Metadata {
    pub phase: Phase,
    pub confirmations: u64,
    pub target_block: u64,
    pub target_hash: B256,
    pub scan_start: u64,
    pub scan_cursor: u64,
    pub state_path: PathBuf,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum CacheKind {
    Missing,
    Uninitialized,
    Initialized(Metadata),
}

pub struct Projection {
    pub map: BalanceMap,
    pub candidates: usize,
    pub usdt_sum: u128,
    pub usdc_sum: u128,
}

pub struct Cache {
    conn: Connection,
    path: PathBuf,
}

impl Cache {
    pub fn inspect(path: &Path) -> Result<CacheKind> {
        // This is the deletion-authorization boundary. Do not call
        // `configure` here: until the complete user schema is authenticated,
        // even changing journal mode would mutate a potentially unrelated DB.
        let file_metadata = match std::fs::symlink_metadata(path) {
            Ok(metadata) => metadata,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                return Ok(CacheKind::Missing);
            }
            Err(error) => {
                return Err(error)
                    .with_context(|| format!("reading bootstrap cache metadata {path:?}"));
            }
        };
        anyhow::ensure!(
            !file_metadata.file_type().is_symlink(),
            "bootstrap cache {path:?} is a symlink; refusing to remove or open it"
        );
        anyhow::ensure!(
            file_metadata.is_file(),
            "bootstrap cache {path:?} is not a regular file; refusing to remove or open it"
        );
        if file_metadata.len() == 0 {
            return Ok(CacheKind::Uninitialized);
        }
        let conn = Connection::open_with_flags(
            path,
            OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
        )
        .with_context(|| format!("opening existing bootstrap cache read-only {path:?}"))?;
        let objects = user_schema(&conn)
            .with_context(|| format!("inspecting bootstrap cache schema at {path:?}"))?;
        if objects.is_empty() {
            return Ok(CacheKind::Uninitialized);
        }
        validate_schema_shape(&objects)
            .with_context(|| format!("bootstrap cache {path:?} is corrupt or unrelated"))?;
        let metadata = read_metadata(&conn)
            .with_context(|| format!("bootstrap cache {path:?} is corrupt or unrelated"))?;
        Ok(CacheKind::Initialized(metadata))
    }

    pub fn initialize(
        path: &Path,
        state_path: &Path,
        confirmations: u64,
        target_block: u64,
        target_hash: B256,
    ) -> Result<Self> {
        let mut cache = Self::open_or_create(path)?;
        let state_path = state_path
            .to_str()
            .context("normalized --state is not valid UTF-8")?;
        let tx = cache.conn.transaction()?;
        tx.execute_batch(CREATE_SCHEMA)?;
        tx.execute(
            "INSERT INTO metadata (
                singleton,schema_version,bootstrap_identity,phase,chain_id,state_path,
                confirmations,target_block,target_hash,scan_start,scan_cursor
             ) VALUES (1,?1,?2,'Scanning',?3,?4,?5,?6,?7,?8,?9)",
            params![
                to_i64(SCHEMA_VERSION, "schema version")?,
                BOOTSTRAP_IDENTITY,
                to_i64(MAINNET_CHAIN_ID, "chain id")?,
                state_path,
                to_i64(confirmations, "confirmation depth")?,
                to_i64(target_block, "target block")?,
                target_hash.as_slice(),
                to_i64(USDT_DEPLOY_BLOCK, "scan start")?,
                to_i64(USDT_DEPLOY_BLOCK - 1, "initial scan cursor")?,
            ],
        )?;
        seed_constructor_owner(&tx)?;
        tx.commit()?;
        cache.validate_schema()?;
        Ok(cache)
    }

    pub fn open(path: &Path) -> Result<Self> {
        let cache = Self::open_existing(path)?;
        cache.validate_schema()?;
        Ok(cache)
    }

    fn open_or_create(path: &Path) -> Result<Self> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let conn =
            Connection::open(path).with_context(|| format!("creating bootstrap cache {path:?}"))?;
        configure(&conn)?;
        Ok(Self {
            conn,
            path: path.to_path_buf(),
        })
    }

    fn open_existing(path: &Path) -> Result<Self> {
        let conn = Connection::open_with_flags(
            path,
            OpenFlags::SQLITE_OPEN_READ_WRITE | OpenFlags::SQLITE_OPEN_NO_MUTEX,
        )
        .with_context(|| format!("opening bootstrap cache {path:?}"))?;
        configure(&conn)?;
        Ok(Self {
            conn,
            path: path.to_path_buf(),
        })
    }

    fn validate_schema(&self) -> Result<()> {
        validate_schema_shape(&user_schema(&self.conn)?)?;
        validate_integrity(&self.conn)
    }

    pub fn metadata(&self) -> Result<Metadata> {
        read_metadata(&self.conn)
    }

    pub fn commit_scan_range(
        &mut self,
        lo: u64,
        hi: u64,
        touched: &HashMap<Address, Touch>,
    ) -> Result<()> {
        anyhow::ensure!(lo <= hi, "empty cache scan range {lo}..={hi}");
        let previous = lo
            .checked_sub(1)
            .context("scan range cannot begin at block zero")?;
        let tx = self.conn.transaction()?;
        for (address, touch) in touched {
            tx.execute(
                "INSERT INTO candidates(address,usdt_block,usdc_block)
                 VALUES (?1,?2,?3)
                 ON CONFLICT(address) DO UPDATE SET
                   usdt_block = CASE
                     WHEN excluded.usdt_block IS NULL THEN candidates.usdt_block
                     WHEN candidates.usdt_block IS NULL THEN excluded.usdt_block
                     ELSE MAX(candidates.usdt_block, excluded.usdt_block) END,
                   usdc_block = CASE
                     WHEN excluded.usdc_block IS NULL THEN candidates.usdc_block
                     WHEN candidates.usdc_block IS NULL THEN excluded.usdc_block
                     ELSE MAX(candidates.usdc_block, excluded.usdc_block) END",
                params![
                    address.as_slice(),
                    optional_i64(touch.usdt, "USDT event block")?,
                    optional_i64(touch.usdc, "USDC event block")?,
                ],
            )?;
        }
        let changed = tx.execute(
            "UPDATE metadata SET scan_cursor=?1
             WHERE singleton=1 AND phase='Scanning' AND scan_cursor=?2 AND target_block>=?1",
            params![
                to_i64(hi, "scan cursor")?,
                to_i64(previous, "previous scan cursor")?
            ],
        )?;
        anyhow::ensure!(
            changed == 1,
            "cache scan range {lo}..={hi} does not begin at durable scan_cursor + 1"
        );
        tx.commit()?;
        Ok(())
    }

    pub fn finish_scanning(&mut self) -> Result<()> {
        let tx = self.conn.transaction()?;
        let changed = tx.execute(
            "UPDATE metadata SET phase='ReadingBalances'
             WHERE singleton=1 AND phase='Scanning' AND scan_cursor=target_block",
            [],
        )?;
        anyhow::ensure!(
            changed == 1,
            "cannot finish scanning before scan_cursor reaches target"
        );
        tx.commit()?;
        Ok(())
    }

    pub fn unread_batch(&self, limit: usize) -> Result<Vec<Address>> {
        let mut statement = self.conn.prepare(
            "SELECT address FROM candidates
             WHERE usdt_balance IS NULL OR usdc_balance IS NULL
             ORDER BY address LIMIT ?1",
        )?;
        let rows = statement.query_map([i64::try_from(limit)?], |row| row.get::<_, Vec<u8>>(0))?;
        rows.map(|row| {
            let bytes = row?;
            anyhow::ensure!(
                bytes.len() == 20,
                "cached candidate address is {} bytes",
                bytes.len()
            );
            Ok(Address::from_slice(&bytes))
        })
        .collect()
    }

    pub fn commit_balances(&mut self, readings: &HashMap<Address, Reading>) -> Result<()> {
        let tx = self.conn.transaction()?;
        for (address, reading) in readings {
            let changed = tx.execute(
                "UPDATE candidates SET usdt_balance=?1,usdc_balance=?2
                 WHERE address=?3 AND (usdt_balance IS NULL OR usdc_balance IS NULL)",
                params![
                    reading.usdt.to_le_bytes().as_slice(),
                    reading.usdc.to_le_bytes().as_slice(),
                    address.as_slice(),
                ],
            )?;
            anyhow::ensure!(
                changed == 1,
                "balance response contains unknown candidate {address}"
            );
        }
        tx.commit()?;
        Ok(())
    }

    pub fn counts(&self) -> Result<(u64, u64)> {
        let candidates: i64 = self
            .conn
            .query_row("SELECT COUNT(*) FROM candidates", [], |r| r.get(0))?;
        let unread: i64 = self.conn.query_row(
            "SELECT COUNT(*) FROM candidates WHERE usdt_balance IS NULL OR usdc_balance IS NULL",
            [],
            |r| r.get(0),
        )?;
        Ok((
            from_i64(candidates, "candidate count")?,
            from_i64(unread, "unread candidate count")?,
        ))
    }

    pub fn projection(&self) -> Result<Projection> {
        let metadata = self.metadata()?;
        anyhow::ensure!(
            metadata.scan_cursor == metadata.target_block,
            "cache scan stopped at {}, target is {}",
            metadata.scan_cursor,
            metadata.target_block
        );
        let mut statement = self.conn.prepare(
            "SELECT address,usdt_block,usdc_block,usdt_balance,usdc_balance
             FROM candidates ORDER BY address",
        )?;
        let mut rows = statement.query([])?;
        let mut map = BalanceMap::new(metadata.target_block);
        let mut candidates = 0usize;
        let mut usdt_sum = 0u128;
        let mut usdc_sum = 0u128;
        while let Some(row) = rows.next()? {
            candidates = candidates
                .checked_add(1)
                .context("candidate count overflow")?;
            let address = checked_address(row.get(0)?)?;
            let usdt_block = checked_block(
                row.get(1)?,
                "USDT",
                USDT_DEPLOY_BLOCK,
                metadata.target_block,
            )?;
            let usdc_block = checked_block(
                row.get(2)?,
                "USDC",
                USDC_DEPLOY_BLOCK,
                metadata.target_block,
            )?;
            let usdt = checked_balance(row.get(3)?, "USDT", address)?;
            let usdc = checked_balance(row.get(4)?, "USDC", address)?;
            if usdt != 0 {
                anyhow::ensure!(
                    usdt_block.is_some(),
                    "nonzero USDT balance for {address} has no event stamp"
                );
            }
            if usdc != 0 {
                anyhow::ensure!(
                    usdc_block.is_some(),
                    "nonzero USDC balance for {address} has no event stamp"
                );
            }
            usdt_sum = usdt_sum
                .checked_add(usdt)
                .context("USDT holder sum overflow")?;
            usdc_sum = usdc_sum
                .checked_add(usdc)
                .context("USDC holder sum overflow")?;
            if usdt != 0 || usdc != 0 {
                map.seed(
                    address,
                    Entry {
                        usdt,
                        usdt_block: usdt_block.unwrap_or(0),
                        usdc,
                        usdc_block: usdc_block.unwrap_or(0),
                    },
                );
            }
        }
        Ok(Projection {
            map,
            candidates,
            usdt_sum,
            usdc_sum,
        })
    }

    pub fn set_ready_to_commit(&mut self) -> Result<()> {
        let tx = self.conn.transaction()?;
        let changed = tx.execute(
            "UPDATE metadata SET phase='ReadyToCommit'
             WHERE singleton=1 AND phase='ReadingBalances' AND scan_cursor=target_block
               AND NOT EXISTS (
                 SELECT 1 FROM candidates
                 WHERE usdt_balance IS NULL OR usdc_balance IS NULL
               )",
            [],
        )?;
        anyhow::ensure!(
            changed == 1,
            "cannot enter ReadyToCommit before scan and balance reads are complete"
        );
        tx.commit()?;
        Ok(())
    }

    pub fn set_complete(&mut self) -> Result<()> {
        self.set_phase(Phase::ReadyToCommit, Phase::Complete)
    }

    fn set_phase(&mut self, from: Phase, to: Phase) -> Result<()> {
        let tx = self.conn.transaction()?;
        let changed = tx.execute(
            "UPDATE metadata SET phase=?1 WHERE singleton=1 AND phase=?2",
            params![to.as_str(), from.as_str()],
        )?;
        anyhow::ensure!(changed == 1, "cache phase is not {}", from.as_str());
        tx.commit()?;
        Ok(())
    }

    pub fn reset_target(&mut self, target_block: u64, target_hash: B256) -> Result<()> {
        let tx = self.conn.transaction()?;
        tx.execute("DELETE FROM candidates", [])?;
        let changed = tx.execute(
            "UPDATE metadata SET phase='Scanning',target_block=?1,target_hash=?2,
                    scan_start=?3,scan_cursor=?4 WHERE singleton=1",
            params![
                to_i64(target_block, "replacement target")?,
                target_hash.as_slice(),
                to_i64(USDT_DEPLOY_BLOCK, "scan start")?,
                to_i64(USDT_DEPLOY_BLOCK - 1, "scan cursor")?,
            ],
        )?;
        anyhow::ensure!(changed == 1, "bootstrap cache metadata row disappeared");
        seed_constructor_owner(&tx)?;
        tx.commit()?;
        Ok(())
    }

    pub fn checkpoint_wal(&self) -> Result<()> {
        let (busy, log, checkpointed): (i64, i64, i64) =
            self.conn
                .query_row("PRAGMA wal_checkpoint(TRUNCATE)", [], |row| {
                    Ok((row.get(0)?, row.get(1)?, row.get(2)?))
                })?;
        anyhow::ensure!(
            busy == 0 && log == checkpointed,
            "final WAL checkpoint incomplete (busy={busy}, log={log}, checkpointed={checkpointed})"
        );
        Ok(())
    }

    pub fn path(&self) -> &Path {
        &self.path
    }
}

fn read_metadata(conn: &Connection) -> Result<Metadata> {
    let row = conn
        .query_row(
            "SELECT schema_version,bootstrap_identity,phase,chain_id,state_path,
                    confirmations,target_block,target_hash,scan_start,scan_cursor
             FROM metadata WHERE singleton=1",
            [],
            |row| {
                Ok((
                    row.get::<_, i64>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, String>(2)?,
                    row.get::<_, i64>(3)?,
                    row.get::<_, String>(4)?,
                    row.get::<_, i64>(5)?,
                    row.get::<_, i64>(6)?,
                    row.get::<_, Vec<u8>>(7)?,
                    row.get::<_, i64>(8)?,
                    row.get::<_, i64>(9)?,
                ))
            },
        )
        .optional()?
        .context("bootstrap cache has no metadata row")?;
    anyhow::ensure!(
        from_i64(row.0, "schema version")? == SCHEMA_VERSION,
        "bootstrap cache schema version {} is unsupported (expected {SCHEMA_VERSION})",
        row.0
    );
    anyhow::ensure!(
        row.1 == BOOTSTRAP_IDENTITY,
        "bootstrap cache identity is incompatible; remove it only after confirming the state path is absent"
    );
    anyhow::ensure!(
        from_i64(row.3, "chain id")? == MAINNET_CHAIN_ID,
        "bootstrap cache belongs to chain {}, not Ethereum mainnet",
        row.3
    );
    anyhow::ensure!(row.7.len() == 32, "cached target hash is not 32 bytes");
    let target_hash = B256::from_slice(&row.7);
    let metadata = Metadata {
        phase: Phase::parse(&row.2)?,
        state_path: PathBuf::from(row.4),
        confirmations: from_i64(row.5, "confirmation depth")?,
        target_block: from_i64(row.6, "target block")?,
        target_hash,
        scan_start: from_i64(row.8, "scan start")?,
        scan_cursor: from_i64(row.9, "scan cursor")?,
    };
    anyhow::ensure!(
        metadata.confirmations > 0,
        "cached confirmation depth is zero"
    );
    anyhow::ensure!(
        metadata.scan_start == USDT_DEPLOY_BLOCK,
        "cached scan start is incompatible"
    );
    anyhow::ensure!(
        metadata.scan_cursor >= metadata.scan_start - 1
            && metadata.scan_cursor <= metadata.target_block,
        "cached scan cursor {} is outside {}..={} ",
        metadata.scan_cursor,
        metadata.scan_start - 1,
        metadata.target_block
    );
    Ok(metadata)
}

fn configure(conn: &Connection) -> Result<()> {
    conn.busy_timeout(std::time::Duration::from_secs(30))?;
    let mode: String = conn.query_row("PRAGMA journal_mode=WAL", [], |row| row.get(0))?;
    anyhow::ensure!(
        mode.eq_ignore_ascii_case("wal"),
        "SQLite refused WAL mode: {mode}"
    );
    conn.pragma_update(None, "synchronous", "FULL")?;
    let synchronous: i64 = conn.query_row("PRAGMA synchronous", [], |row| row.get(0))?;
    anyhow::ensure!(
        synchronous == 2,
        "SQLite synchronous mode is {synchronous}, expected FULL (2)"
    );
    Ok(())
}

#[derive(Debug, PartialEq, Eq)]
struct SchemaObject {
    kind: String,
    name: String,
    table: String,
    sql: Option<String>,
}

fn user_schema(conn: &Connection) -> rusqlite::Result<Vec<SchemaObject>> {
    let mut statement = conn.prepare(
        "SELECT type,name,tbl_name,sql
         FROM sqlite_schema
         WHERE name NOT GLOB 'sqlite_*'
         ORDER BY type,name",
    )?;
    statement
        .query_map([], |row| {
            Ok(SchemaObject {
                kind: row.get(0)?,
                name: row.get(1)?,
                table: row.get(2)?,
                sql: row.get(3)?,
            })
        })?
        .collect()
}

fn expected_schema() -> Result<Vec<SchemaObject>> {
    let conn = Connection::open_in_memory().context("creating expected bootstrap schema")?;
    conn.execute_batch(CREATE_SCHEMA)
        .context("creating expected bootstrap schema")?;
    user_schema(&conn).context("reading expected bootstrap schema")
}

fn validate_schema_shape(actual: &[SchemaObject]) -> Result<()> {
    let expected = expected_schema()?;
    anyhow::ensure!(
        actual == expected,
        "SQLite user schema does not exactly match the bootstrap schema; found {actual:?}, expected {expected:?}"
    );
    Ok(())
}

fn validate_integrity(conn: &Connection) -> Result<()> {
    let integrity: String = conn.query_row("PRAGMA integrity_check", [], |row| row.get(0))?;
    anyhow::ensure!(
        integrity == "ok",
        "SQLite integrity_check failed: {integrity}"
    );
    Ok(())
}

fn seed_constructor_owner(tx: &Transaction<'_>) -> Result<()> {
    tx.execute(
        "INSERT INTO candidates(address,usdt_block) VALUES (?1,?2)",
        params![
            USDT_INITIAL_OWNER.as_slice(),
            to_i64(USDT_DEPLOY_BLOCK, "constructor block")?
        ],
    )?;
    Ok(())
}

fn checked_address(bytes: Vec<u8>) -> Result<Address> {
    anyhow::ensure!(
        bytes.len() == 20,
        "cached address is {} bytes, expected 20",
        bytes.len()
    );
    let address = Address::from_slice(&bytes);
    anyhow::ensure!(!address.is_zero(), "cached candidate address is zero");
    Ok(address)
}

fn checked_balance(bytes: Option<Vec<u8>>, token: &str, address: Address) -> Result<u128> {
    let bytes = bytes.with_context(|| format!("{token} balance for {address} is unread"))?;
    anyhow::ensure!(
        bytes.len() == 16,
        "cached {token} balance for {address} is {} bytes",
        bytes.len()
    );
    Ok(u128::from_le_bytes(
        bytes.try_into().expect("length checked"),
    ))
}

fn checked_block(value: Option<i64>, token: &str, deploy: u64, target: u64) -> Result<Option<u32>> {
    let Some(value) = value else {
        return Ok(None);
    };
    let value = from_i64(value, &format!("{token} event block"))?;
    anyhow::ensure!(
        (deploy..=target).contains(&value),
        "cached {token} event block {value} is outside {deploy}..={target}"
    );
    Ok(Some(u32::try_from(value).with_context(|| {
        format!("{token} event block exceeds u32")
    })?))
}

fn optional_i64(value: Option<u64>, what: &str) -> Result<Option<i64>> {
    value.map(|value| to_i64(value, what)).transpose()
}

fn to_i64(value: u64, what: &str) -> Result<i64> {
    i64::try_from(value).with_context(|| format!("{what} {value} does not fit SQLite INTEGER"))
}

fn from_i64(value: i64, what: &str) -> Result<u64> {
    u64::try_from(value).with_context(|| format!("cached {what} is negative ({value})"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn root(name: &str) -> PathBuf {
        let path = std::env::temp_dir().join(format!(
            "usdt-pir-bootstrap-cache-{name}-{}",
            std::process::id()
        ));
        std::fs::remove_dir_all(&path).ok();
        std::fs::create_dir_all(&path).unwrap();
        path
    }

    fn initialized(name: &str, target: u64) -> (PathBuf, Cache) {
        let root = root(name);
        let cache_path = root.join("state.snapshot.bootstrap.sqlite");
        let state = root.join("state.snapshot");
        let cache =
            Cache::initialize(&cache_path, &state, 4, target, B256::repeat_byte(0x42)).unwrap();
        (cache_path, cache)
    }

    #[test]
    fn initialization_atomically_stores_metadata_and_constructor_seed() {
        let (path, cache) = initialized("atomic-init", 20_000_000);
        let metadata = cache.metadata().unwrap();
        assert_eq!(metadata.phase, Phase::Scanning);
        assert_eq!(metadata.scan_cursor, USDT_DEPLOY_BLOCK - 1);
        assert_eq!(cache.counts().unwrap(), (1, 1));
        let seeded: i64 = cache
            .conn
            .query_row(
                "SELECT usdt_block FROM candidates WHERE address=?1",
                [USDT_INITIAL_OWNER.as_slice()],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(seeded as u64, USDT_DEPLOY_BLOCK);
        assert!(
            matches!(Cache::inspect(&path).unwrap(), CacheKind::Initialized(_)),
            "read-only inspection must see committed schema still held in WAL"
        );
        drop(cache);
        assert!(matches!(
            Cache::inspect(&path).unwrap(),
            CacheKind::Initialized(_)
        ));
    }

    #[test]
    fn databases_with_no_user_schema_are_uninitialized_but_unknown_tables_are_rejected() {
        let root = root("classification");
        let empty = root.join("empty.sqlite");
        std::fs::File::create(&empty).unwrap();
        assert_eq!(Cache::inspect(&empty).unwrap(), CacheKind::Uninitialized);
        assert_eq!(std::fs::metadata(&empty).unwrap().len(), 0);

        let schema_free = root.join("schema-free.sqlite");
        let conn = Connection::open(&schema_free).unwrap();
        conn.pragma_update(None, "user_version", 7).unwrap();
        drop(conn);
        assert!(std::fs::metadata(&schema_free).unwrap().len() > 0);
        assert_eq!(
            Cache::inspect(&schema_free).unwrap(),
            CacheKind::Uninitialized
        );

        let partial = root.join("partial.sqlite");
        let conn = Connection::open(&partial).unwrap();
        conn.execute("CREATE TABLE mystery(value INTEGER)", [])
            .unwrap();
        drop(conn);
        let error = Cache::inspect(&partial).unwrap_err();
        assert!(format!("{error:#}").contains("corrupt or unrelated"));
    }

    #[test]
    fn view_only_database_is_unrelated_and_inspection_is_read_only() {
        let root = root("view-only");
        let path = root.join("operator.sqlite");
        let conn = Connection::open(&path).unwrap();
        conn.execute_batch("CREATE VIEW operator_data AS SELECT 7 AS value")
            .unwrap();
        let mode: String = conn
            .query_row("PRAGMA journal_mode", [], |row| row.get(0))
            .unwrap();
        assert_eq!(mode, "delete");
        drop(conn);
        let before = std::fs::read(&path).unwrap();

        let error = Cache::inspect(&path).unwrap_err();
        assert!(format!("{error:#}").contains("corrupt or unrelated"));
        assert_eq!(std::fs::read(&path).unwrap(), before);
        assert!(!path.with_file_name("operator.sqlite-wal").exists());
        assert!(!path.with_file_name("operator.sqlite-shm").exists());

        let conn = Connection::open_with_flags(&path, OpenFlags::SQLITE_OPEN_READ_ONLY).unwrap();
        let value: i64 = conn
            .query_row("SELECT value FROM operator_data", [], |row| row.get(0))
            .unwrap();
        let mode: String = conn
            .query_row("PRAGMA journal_mode", [], |row| row.get(0))
            .unwrap();
        assert_eq!(value, 7);
        assert_eq!(mode, "delete");
    }

    #[test]
    fn extra_views_triggers_and_indexes_are_all_rejected() {
        for (name, ddl) in [
            (
                "extra-view",
                "CREATE VIEW operator_view AS SELECT COUNT(*) FROM candidates",
            ),
            (
                "extra-trigger",
                "CREATE TRIGGER operator_trigger AFTER INSERT ON candidates BEGIN SELECT 1; END",
            ),
            (
                "extra-index",
                "CREATE INDEX operator_index ON candidates(usdt_block)",
            ),
        ] {
            let (path, cache) = initialized(name, 20_000_000);
            drop(cache);
            let conn = Connection::open(&path).unwrap();
            conn.execute_batch(ddl).unwrap();
            drop(conn);

            let error = Cache::inspect(&path).unwrap_err();
            assert!(
                format!("{error:#}").contains("does not exactly match"),
                "{name} was not rejected as an extra schema object: {error:#}"
            );
        }
    }

    #[test]
    fn altered_table_constraints_are_rejected_before_opening_read_write() {
        let root = root("altered-constraint");
        let path = root.join("altered.sqlite");
        let altered =
            CREATE_SCHEMA.replacen("CHECK(confirmations > 0)", "CHECK(confirmations >= 0)", 1);
        assert_ne!(altered, CREATE_SCHEMA);
        let conn = Connection::open(&path).unwrap();
        conn.execute_batch(&altered).unwrap();
        drop(conn);
        let before = std::fs::read(&path).unwrap();

        let error = Cache::inspect(&path).unwrap_err();
        assert!(format!("{error:#}").contains("does not exactly match"));
        assert_eq!(std::fs::read(&path).unwrap(), before);
        assert!(!path.with_file_name("altered.sqlite-wal").exists());
        assert!(!path.with_file_name("altered.sqlite-shm").exists());
    }

    #[test]
    fn scan_upserts_take_per_token_maxima_and_cursor_moves_in_the_same_unit() {
        let target = USDT_DEPLOY_BLOCK + 1;
        let (_, mut cache) = initialized("scan-max", target);
        let address = Address::repeat_byte(0x11);
        let mut first = HashMap::new();
        first.insert(
            address,
            Touch {
                usdt: Some(USDT_DEPLOY_BLOCK),
                usdc: None,
            },
        );
        cache
            .commit_scan_range(USDT_DEPLOY_BLOCK, USDT_DEPLOY_BLOCK, &first)
            .unwrap();
        let mut second = HashMap::new();
        second.insert(
            address,
            Touch {
                usdt: Some(target),
                usdc: Some(target),
            },
        );
        cache.commit_scan_range(target, target, &second).unwrap();
        assert_eq!(cache.metadata().unwrap().scan_cursor, target);
        let blocks: (i64, i64) = cache
            .conn
            .query_row(
                "SELECT usdt_block,usdc_block FROM candidates WHERE address=?1",
                [address.as_slice()],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .unwrap();
        assert_eq!(blocks, (target as i64, target as i64));
    }

    #[test]
    fn interrupted_scan_must_restart_at_cursor_plus_one() {
        let target = USDT_DEPLOY_BLOCK + 2;
        let (_, mut cache) = initialized("scan-resume", target);
        cache
            .commit_scan_range(USDT_DEPLOY_BLOCK, USDT_DEPLOY_BLOCK, &HashMap::new())
            .unwrap();
        let error = cache
            .commit_scan_range(target, target, &HashMap::new())
            .unwrap_err();
        assert!(format!("{error:#}").contains("scan_cursor + 1"));
        assert_eq!(cache.metadata().unwrap().scan_cursor, USDT_DEPLOY_BLOCK);
    }

    #[test]
    fn null_is_unread_but_encoded_zero_is_complete_and_zero_rows_are_omitted() {
        let target = 20_000_000;
        let (_, mut cache) = initialized("zero", target);
        let address = Address::repeat_byte(0x22);
        let mut touched = HashMap::new();
        touched.insert(
            address,
            Touch {
                usdt: Some(target - 1),
                usdc: Some(target),
            },
        );
        cache
            .commit_scan_range(USDT_DEPLOY_BLOCK, target, &touched)
            .unwrap();
        cache.finish_scanning().unwrap();
        assert_eq!(cache.counts().unwrap(), (2, 2));
        let readings = HashMap::from([
            (USDT_INITIAL_OWNER, Reading::default()),
            (address, Reading::default()),
        ]);
        cache.commit_balances(&readings).unwrap();
        assert_eq!(cache.counts().unwrap(), (2, 0));
        let projection = cache.projection().unwrap();
        assert_eq!(projection.candidates, 2);
        assert!(projection.map.is_empty());
        cache.set_ready_to_commit().unwrap();
    }

    #[test]
    fn projection_preserves_a_zero_tokens_last_event_stamp() {
        let target = 20_000_000;
        let (_, mut cache) = initialized("one-zero", target);
        let address = Address::repeat_byte(0x33);
        let mut touched = HashMap::new();
        touched.insert(
            address,
            Touch {
                usdt: Some(target - 10),
                usdc: Some(target - 5),
            },
        );
        cache
            .commit_scan_range(USDT_DEPLOY_BLOCK, target, &touched)
            .unwrap();
        cache.finish_scanning().unwrap();
        cache
            .commit_balances(&HashMap::from([
                (USDT_INITIAL_OWNER, Reading::default()),
                (address, Reading { usdt: 7, usdc: 0 }),
            ]))
            .unwrap();
        let entry = cache.projection().unwrap().map.get(&address).unwrap();
        assert_eq!((entry.usdt, entry.usdt_block), (7, (target - 10) as u32));
        assert_eq!((entry.usdc, entry.usdc_block), (0, (target - 5) as u32));
    }

    #[test]
    fn projection_rejects_checked_holder_sum_overflow() {
        let target = 20_000_000;
        let (_, mut cache) = initialized("sum-overflow", target);
        let address = Address::repeat_byte(0x34);
        cache
            .commit_scan_range(
                USDT_DEPLOY_BLOCK,
                target,
                &HashMap::from([(
                    address,
                    Touch {
                        usdt: Some(target),
                        usdc: None,
                    },
                )]),
            )
            .unwrap();
        cache.finish_scanning().unwrap();
        cache
            .commit_balances(&HashMap::from([
                (
                    USDT_INITIAL_OWNER,
                    Reading {
                        usdt: u128::MAX,
                        usdc: 0,
                    },
                ),
                (address, Reading { usdt: 1, usdc: 0 }),
            ]))
            .unwrap();

        let error = cache.projection().err().expect("sum must overflow");
        assert!(format!("{error:#}").contains("USDT holder sum overflow"));
    }

    #[test]
    fn target_reset_clears_work_and_reseeds_atomically() {
        let target = 20_000_000;
        let (_, mut cache) = initialized("reset", target);
        cache
            .commit_scan_range(
                USDT_DEPLOY_BLOCK,
                USDT_DEPLOY_BLOCK,
                &HashMap::from([(
                    Address::repeat_byte(0x44),
                    Touch {
                        usdt: Some(USDT_DEPLOY_BLOCK),
                        usdc: None,
                    },
                )]),
            )
            .unwrap();
        cache
            .reset_target(target + 100, B256::repeat_byte(0x99))
            .unwrap();
        let metadata = cache.metadata().unwrap();
        assert_eq!(metadata.phase, Phase::Scanning);
        assert_eq!(metadata.target_block, target + 100);
        assert_eq!(metadata.scan_cursor, USDT_DEPLOY_BLOCK - 1);
        assert_eq!(cache.counts().unwrap(), (1, 1));
    }

    #[test]
    fn projection_revalidates_blob_widths_despite_sqlite_type_names() {
        let target = 20_000_000;
        let (_, mut cache) = initialized("width", target);
        cache
            .commit_scan_range(USDT_DEPLOY_BLOCK, target, &HashMap::new())
            .unwrap();
        cache.finish_scanning().unwrap();
        cache
            .conn
            .pragma_update(None, "ignore_check_constraints", "ON")
            .unwrap();
        cache
            .conn
            .execute(
                "UPDATE candidates SET usdt_balance=x'00',usdc_balance=zeroblob(16)",
                [],
            )
            .unwrap();
        let error = cache.projection().err().expect("invalid width must fail");
        assert!(format!("{error:#}").contains("1 bytes"));
    }

    #[test]
    fn balance_batch_crashes_and_failed_transactions_resume_from_reopened_database() {
        let target = 20_000_000;
        let (path, mut cache) = initialized("balance-reopen", target);
        let address = Address::repeat_byte(0x88);
        cache
            .commit_scan_range(
                USDT_DEPLOY_BLOCK,
                target,
                &HashMap::from([(
                    address,
                    Touch {
                        usdt: Some(target - 1),
                        usdc: Some(target),
                    },
                )]),
            )
            .unwrap();
        cache.finish_scanning().unwrap();
        let selected_before_crash = cache.unread_batch(400).unwrap();
        assert_eq!(selected_before_crash.len(), 2);
        drop(cache); // RPC may have returned, but no SQLite transaction committed.

        let mut cache = Cache::open(&path).unwrap();
        assert_eq!(cache.unread_batch(400).unwrap(), selected_before_crash);
        let unknown = Address::repeat_byte(0x89);
        let failed = HashMap::from([
            (USDT_INITIAL_OWNER, Reading::default()),
            (address, Reading { usdt: 7, usdc: 9 }),
            (unknown, Reading { usdt: 1, usdc: 1 }),
        ]);
        assert!(cache.commit_balances(&failed).is_err());
        drop(cache); // The whole failed transaction must roll back durably.

        let mut cache = Cache::open(&path).unwrap();
        assert_eq!(cache.counts().unwrap(), (2, 2));
        cache
            .commit_balances(&HashMap::from([
                (USDT_INITIAL_OWNER, Reading::default()),
                (address, Reading { usdt: 7, usdc: 9 }),
            ]))
            .unwrap();
        drop(cache);

        let cache = Cache::open(&path).unwrap();
        assert_eq!(cache.counts().unwrap(), (2, 0));
        let projection = cache.projection().unwrap();
        assert_eq!(projection.map.get(&address).unwrap().usdt, 7);
        assert_eq!(projection.map.get(&address).unwrap().usdc, 9);
    }
}

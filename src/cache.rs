//! Disposable disk cache. One background worker owns the shared SQLite connection.
//! Authenticated data is namespaced by the verified YouTube account and channel.
use std::{
    collections::HashMap,
    path::{Path, PathBuf},
    sync::OnceLock,
    time::{Duration, Instant},
};

use anyhow::Result;
use directories::BaseDirs;
use rusqlite::{Connection, OptionalExtension, TransactionBehavior, params};
use serde::{Serialize, de::DeserializeOwned};
use sha2::{Digest, Sha256};
use tokio::sync::{mpsc, oneshot};

const LIMIT: i64 = 512 * 1024 * 1024;
const ENTRY_LIMIT: usize = 16 * 1024 * 1024;
const TOUCH_INTERVAL: Duration = Duration::from_secs(5);
const RETRY_INTERVAL: Duration = Duration::from_secs(5);
/// Large enough for a feed's burst of thumbnail saves; images are tens of KB each.
const QUEUE_CAPACITY: usize = 256;
pub const DAY: u64 = 24 * 60 * 60;

#[derive(Clone)]
pub struct Cache {
    worker: mpsc::Sender<Request>,
    scope: String,
}

#[derive(Hash, PartialEq, Eq)]
struct Key {
    scope: String,
    kind: String,
    key: String,
}

enum Operation {
    Read(Key),
    Write(Key, Vec<u8>, u64),
    Invalidate(String, String),
}

struct Request {
    operation: Operation,
    reply: Option<oneshot::Sender<Result<Option<Vec<u8>>>>>,
}

impl Cache {
    pub fn new(scope: &str) -> Option<Self> {
        // Keep the worker alive between thumbnail requests and account switches.
        static WORKER: OnceLock<Option<mpsc::Sender<Request>>> = OnceLock::new();
        let worker = WORKER.get_or_init(|| {
            start_worker(
                BaseDirs::new()?.cache_dir().join("slintyt/cache.sqlite3"),
                LIMIT,
            )
            .ok()
        });
        Some(Self {
            worker: worker.as_ref()?.clone(),
            scope: hash(scope),
        })
    }

    fn key(&self, kind: &str, key: &str) -> Key {
        Key {
            scope: self.scope.clone(),
            kind: kind.to_owned(),
            key: hash(key),
        }
    }

    async fn request(&self, operation: Operation) -> Option<Vec<u8>> {
        let (reply, response) = oneshot::channel();
        self.worker
            .send(Request {
                operation,
                reply: Some(reply),
            })
            .await
            .ok()?;
        response.await.ok()?.ok().flatten()
    }

    pub async fn bytes(&self, kind: &str, key: &str) -> Option<Vec<u8>> {
        self.request(Operation::Read(self.key(kind, key))).await
    }

    /// Queue a best-effort save without waiting for queue capacity or disk I/O.
    /// Returns whether the save was accepted, not whether it has been committed.
    pub fn put_bytes(&self, kind: &str, key: &str, bytes: Vec<u8>, ttl: u64) -> bool {
        bytes.len() <= ENTRY_LIMIT
            && self
                .worker
                .try_send(Request {
                    operation: Operation::Write(self.key(kind, key), bytes, ttl),
                    reply: None,
                })
                .is_ok()
    }

    pub async fn get<T: DeserializeOwned>(&self, kind: &str, key: &str) -> Option<T> {
        serde_json::from_slice(&self.bytes(kind, key).await?).ok()
    }

    pub fn put<T: Serialize>(&self, kind: &str, key: &str, value: &T, ttl: u64) -> bool {
        serde_json::to_vec(value).is_ok_and(|bytes| self.put_bytes(kind, key, bytes, ttl))
    }

    pub async fn invalidate(&self, kind: &str) {
        self.request(Operation::Invalidate(self.scope.clone(), kind.to_owned()))
            .await;
    }
}

fn start_worker(path: PathBuf, limit: i64) -> Result<mpsc::Sender<Request>> {
    // Reads/invalidation wait for capacity; optional saves skip a full queue.
    let (sender, mut receiver) = mpsc::channel::<Request>(QUEUE_CAPACITY);
    std::thread::Builder::new()
        .name("slintyt-cache".into())
        .spawn(move || {
            let mut database = None;
            let mut retry_at = Instant::now();
            let mut report_at = Instant::now();
            while let Some(request) = receiver.blocking_recv() {
                let result = (|| {
                    if database.is_none() {
                        anyhow::ensure!(
                            Instant::now() >= retry_at,
                            "Cache temporarily unavailable"
                        );
                        retry_at = Instant::now() + RETRY_INTERVAL;
                        database = Some(Database::open(&path)?);
                    }
                    let db = database.as_mut().unwrap();
                    match request.operation {
                        Operation::Read(key) => db.read(key),
                        Operation::Write(key, bytes, ttl) => {
                            db.write(&key, &bytes, ttl, limit)?;
                            Ok(None)
                        }
                        Operation::Invalidate(scope, kind) => {
                            db.invalidate(&scope, &kind)?;
                            Ok(None)
                        }
                    }
                })();
                if result.is_err() && Instant::now() >= report_at {
                    // Never log account identifiers, cache keys, or payloads.
                    eprintln!("Disk cache operation failed; continuing without cached data");
                    report_at = Instant::now() + Duration::from_secs(60);
                }
                if let Some(reply) = request.reply {
                    let _ = reply.send(result);
                }
            }
            // Only runs when all senders are dropped (e.g. in tests). The app's
            // static sender lives until process exit; pending LRU touches are disposable.
            if let Some(mut db) = database {
                let _ = db.flush_touches();
            }
        })?;
    Ok(sender)
}

struct Database {
    db: Connection,
    touches: HashMap<Key, i64>,
    last_touch_flush: Instant,
}

impl Database {
    fn open(path: &Path) -> Result<Self> {
        let mut dirs = std::fs::DirBuilder::new();
        dirs.recursive(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::DirBuilderExt;
            dirs.mode(0o700);
        }
        dirs.create(path.parent().unwrap())?;
        let mut options = std::fs::OpenOptions::new();
        options.create(true).append(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        options.open(path)?;
        let mut db = Connection::open(path)?;
        db.busy_timeout(Duration::from_secs(2))?;
        // NORMAL is suitable for disposable cache data. Keep automatic WAL checkpoints.
        db.execute_batch("PRAGMA journal_mode=WAL; PRAGMA synchronous=NORMAL;")?;
        let tx = db.transaction_with_behavior(TransactionBehavior::Immediate)?;
        tx.execute_batch(
            "CREATE TABLE IF NOT EXISTS entries (
                scope TEXT NOT NULL, kind TEXT NOT NULL, key TEXT NOT NULL,
                data BLOB NOT NULL, expires INTEGER NOT NULL, touched INTEGER NOT NULL,
                PRIMARY KEY(scope, kind, key)
            );
            CREATE INDEX IF NOT EXISTS cache_expiry ON entries(expires);
            CREATE INDEX IF NOT EXISTS cache_age ON entries(touched);
            CREATE TABLE IF NOT EXISTS cache_totals (
                id INTEGER PRIMARY KEY CHECK(id=1), bytes INTEGER NOT NULL, entries INTEGER NOT NULL
            );
            INSERT INTO cache_totals SELECT 1,coalesce(sum(length(data)),0),count(*) FROM entries
                WHERE NOT EXISTS (SELECT 1 FROM cache_totals)
                ON CONFLICT(id) DO NOTHING;
            CREATE TRIGGER IF NOT EXISTS cache_insert AFTER INSERT ON entries BEGIN
                UPDATE cache_totals SET bytes=bytes+length(new.data), entries=entries+1 WHERE id=1;
            END;
            CREATE TRIGGER IF NOT EXISTS cache_delete AFTER DELETE ON entries BEGIN
                UPDATE cache_totals SET bytes=bytes-length(old.data), entries=entries-1 WHERE id=1;
            END;
            CREATE TRIGGER IF NOT EXISTS cache_resize AFTER UPDATE OF data ON entries BEGIN
                UPDATE cache_totals SET bytes=bytes+length(new.data)-length(old.data) WHERE id=1;
            END;",
        )?;
        tx.commit()?;
        Ok(Self {
            db,
            touches: HashMap::new(),
            last_touch_flush: Instant::now(),
        })
    }

    fn read(&mut self, key: Key) -> Result<Option<Vec<u8>>> {
        let now = crate::history::now() as i64;
        let bytes = self.db.prepare_cached(
            "SELECT data FROM entries WHERE scope=?1 AND kind=?2 AND key=?3 AND expires>?4 AND length(data)<=?5",
        )?.query_row(
            params![key.scope, key.kind, key.key, now, ENTRY_LIMIT as i64],
            |row| row.get(0),
        ).optional()?;
        if bytes.is_some() {
            self.touches.insert(key, now);
            if self.touches.len() >= 128 || self.last_touch_flush.elapsed() >= TOUCH_INTERVAL {
                // An access-time write must never turn a successful read into a cache miss.
                if self.flush_touches().is_err() {
                    self.touches.clear();
                    self.last_touch_flush = Instant::now();
                }
            }
        }
        Ok(bytes)
    }

    fn apply_touches(db: &Connection, touches: &HashMap<Key, i64>) -> Result<()> {
        if !touches.is_empty() {
            let mut update = db.prepare_cached(
                "UPDATE entries SET touched=max(touched,?4) WHERE scope=?1 AND kind=?2 AND key=?3 AND touched<?4",
            )?;
            for (key, touched) in touches {
                update.execute(params![key.scope, key.kind, key.key, touched])?;
            }
        }
        Ok(())
    }

    fn flush_touches(&mut self) -> Result<()> {
        if !self.touches.is_empty() {
            let tx = self
                .db
                .transaction_with_behavior(TransactionBehavior::Immediate)?;
            Self::apply_touches(&tx, &self.touches)?;
            tx.commit()?;
            self.touches.clear();
        }
        self.last_touch_flush = Instant::now();
        Ok(())
    }

    fn write(&mut self, key: &Key, bytes: &[u8], ttl: u64, limit: i64) -> Result<()> {
        if bytes.len() > ENTRY_LIMIT || bytes.len() as i64 > limit {
            return Ok(());
        }
        let tx = self
            .db
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let now = crate::history::now() as i64;
        Self::apply_touches(&tx, &self.touches)?;
        tx.prepare_cached("DELETE FROM entries WHERE expires<=?1")?
            .execute([now])?;
        // UPSERT preserves row identity and lets triggers account for replacements atomically.
        tx.prepare_cached(
            "INSERT INTO entries VALUES (?1,?2,?3,?4,?5,?6)
             ON CONFLICT(scope,kind,key) DO UPDATE SET data=excluded.data, expires=excluded.expires, touched=excluded.touched",
        )?.execute(params![
            key.scope, key.kind, key.key, bytes,
            now.saturating_add(ttl.min(i64::MAX as u64) as i64), now
        ])?;
        let (mut total, mut count): (i64, i64) = tx
            .prepare_cached("SELECT bytes,entries FROM cache_totals WHERE id=1")?
            .query_row([], |row| Ok((row.get(0)?, row.get(1)?)))?;
        while total > limit || count > 10_000 {
            let (row, size): (i64, i64) = tx
                .prepare_cached(
                    "SELECT rowid,length(data) FROM entries ORDER BY touched,rowid LIMIT 1",
                )?
                .query_row([], |r| Ok((r.get(0)?, r.get(1)?)))?;
            tx.prepare_cached("DELETE FROM entries WHERE rowid=?1")?
                .execute([row])?;
            total -= size;
            count -= 1;
        }
        tx.commit()?;
        self.touches.clear();
        self.last_touch_flush = Instant::now();
        Ok(())
    }

    fn invalidate(&mut self, scope: &str, kind: &str) -> Result<()> {
        self.db
            .prepare_cached("DELETE FROM entries WHERE scope=?1 AND kind=?2")?
            .execute(params![scope, kind])?;
        self.touches
            .retain(|key, _| key.scope != scope || key.kind != kind);
        Ok(())
    }
}

fn hash(value: &str) -> String {
    format!("{:x}", Sha256::digest(value.as_bytes()))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key(scope: &str, name: &str) -> Key {
        Key {
            scope: scope.into(),
            kind: "pages".into(),
            key: hash(name),
        }
    }

    fn assert_totals(db: &Database) {
        let expected: (i64, i64) = db
            .db
            .query_row(
                "SELECT coalesce(sum(length(data)),0),count(*) FROM entries",
                [],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .unwrap();
        let actual = db
            .db
            .query_row(
                "SELECT bytes,entries FROM cache_totals WHERE id=1",
                [],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .unwrap();
        assert_eq!(expected, actual);
    }

    #[test]
    fn migration_restart_expiry_eviction_and_accounting() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("cache.db");
        // An existing cache from before WAL/accounting must retain its entries.
        let legacy = Connection::open(&path).unwrap();
        legacy
            .execute_batch(
                "CREATE TABLE entries (
            scope TEXT NOT NULL, kind TEXT NOT NULL, key TEXT NOT NULL,
            data BLOB NOT NULL, expires INTEGER NOT NULL, touched INTEGER NOT NULL,
            PRIMARY KEY(scope,kind,key));",
            )
            .unwrap();
        legacy
            .execute(
                "INSERT INTO entries VALUES ('a','pages',?1,X'6f6c64',?2,0)",
                params![hash("home"), (crate::history::now() + DAY) as i64],
            )
            .unwrap();
        drop(legacy);
        let mut db = Database::open(&path).unwrap();
        assert_eq!(
            db.db
                .query_row("PRAGMA journal_mode", [], |row| row.get::<_, String>(0))
                .unwrap(),
            "wal"
        );
        // Failed bookkeeping must not discard a readable payload.
        db.db.execute_batch("PRAGMA query_only=ON").unwrap();
        db.last_touch_flush = Instant::now() - TOUCH_INTERVAL;
        assert_eq!(db.read(key("a", "home")).unwrap(), Some(b"old".to_vec()));
        db.db.execute_batch("PRAGMA query_only=OFF").unwrap();
        assert!(db.read(key("b", "home")).unwrap().is_none());
        db.write(&key("a", "home"), b"replacement", DAY, 12)
            .unwrap();
        assert_totals(&db);
        db.write(&key("b", "home"), b"new", DAY, 12).unwrap();
        assert!(db.read(key("a", "home")).unwrap().is_none());
        db.write(&key("b", "expired"), b"gone", 0, 12).unwrap();
        assert!(db.read(key("b", "expired")).unwrap().is_none());
        db.write(&key("a", "large"), b"1234567890", DAY, 12)
            .unwrap();
        assert!(db.read(key("b", "home")).unwrap().is_none());
        assert_totals(&db);
        drop(db);
        let mut db = Database::open(&path).unwrap();
        assert_eq!(
            db.read(key("a", "large")).unwrap(),
            Some(b"1234567890".to_vec())
        );
        db.invalidate("a", "pages").unwrap();
        assert_totals(&db);
        assert!(db.read(key("a", "large")).unwrap().is_none());
    }

    #[tokio::test]
    async fn concurrent_requests_and_scoped_invalidation() {
        let root = tempfile::tempdir().unwrap();
        let a = Cache {
            worker: start_worker(root.path().join("cache.db"), LIMIT).unwrap(),
            scope: "a".into(),
        };
        let b = Cache {
            scope: "b".into(),
            ..a.clone()
        };
        let mut tasks = tokio::task::JoinSet::new();
        for i in 0..128 {
            let cache = a.clone();
            tasks.spawn(async move {
                let key = i.to_string();
                let bytes = vec![i as u8; 4096];
                let accepted = cache.put_bytes("images", &key, bytes.clone(), DAY);
                assert_eq!(cache.bytes("images", &key).await, accepted.then_some(bytes));
                accepted.then_some(key)
            });
        }
        let mut retained = None;
        while let Some(result) = tasks.join_next().await {
            if let Some(key) = result.unwrap() {
                retained = Some(key);
            }
        }
        assert!(a.put_bytes("pages", "home", b"broken json".to_vec(), DAY));
        assert!(a.get::<serde_json::Value>("pages", "home").await.is_none());
        assert!(b.put("pages", "home", &vec![1, 2], DAY));
        a.invalidate("pages").await;
        assert!(a.bytes("pages", "home").await.is_none());
        assert_eq!(b.get::<Vec<i32>>("pages", "home").await, Some(vec![1, 2]));
        assert!(a.bytes("images", &retained.unwrap()).await.is_some());
    }

    #[test]
    fn saves_never_wait_for_the_worker_or_queue_capacity() {
        // No worker consumes this queue: both calls must return immediately.
        let (worker, mut receiver) = mpsc::channel(1);
        let cache = Cache {
            worker,
            scope: "a".into(),
        };
        assert!(cache.put_bytes("images", "first", vec![1], DAY));
        assert!(!cache.put_bytes("images", "overflow", vec![2], DAY));
        let request = receiver.try_recv().unwrap();
        assert!(request.reply.is_none());
        assert!(matches!(request.operation, Operation::Write(_, bytes, _) if bytes == [1]));
        assert!(receiver.try_recv().is_err());
        drop(receiver);
        assert!(!cache.put_bytes("images", "closed", vec![3], DAY));
    }
}

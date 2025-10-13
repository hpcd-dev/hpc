use futures_util::TryStreamExt;

use std::{net::IpAddr, path::Path, str::FromStr, time::Duration};

use sqlx::{
    Row, Sqlite, SqlitePool,
    sqlite::{SqliteConnectOptions, SqlitePoolOptions},
};
use thiserror::Error;
use time::{OffsetDateTime, format_description::well_known::Rfc3339};

#[cfg(feature = "serde")]
use serde::{Deserialize, Serialize};

/// Address for a host: either hostname or IP.
#[derive(Debug, Clone, PartialEq, Eq)]
#[cfg_attr(feature = "serde", derive(Serialize, Deserialize))]
pub enum Address {
    Hostname(String),
    Ip(IpAddr),
}

/// Slurm version triplet.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[cfg_attr(feature = "serde", derive(Serialize, Deserialize))]
pub struct SlurmVersion {
    pub major: i64,
    pub minor: i64,
    pub patch: i64,
}

impl std::fmt::Display for SlurmVersion {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}.{}.{}", self.major, self.minor, self.patch)
    }
}

/// Linux distribution info.
#[derive(Debug, Clone, PartialEq, Eq)]
#[cfg_attr(feature = "serde", derive(Serialize, Deserialize))]
pub struct Distro {
    pub name: String,
    pub version: String,
}

/// Payload for creating or upserting a host.
#[derive(Debug, Clone, PartialEq, Eq)]
#[cfg_attr(feature = "serde", derive(Serialize, Deserialize))]
pub struct NewHost {
    /// Short, memorable, globally-unique id (e.g., "gpu01", "c1", "node-a")
    pub hostid: String,
    /// Username for server
    pub username: String,
    /// hostname or IP address
    pub address: Address,
    // ssh port
    pub port: u16,
    // ssh identity path
    pub identity_path: Option<String>,
    // WLM version, TODO: make this more general
    pub slurm: SlurmVersion,
    /// Linux distribution installed on cluster head
    pub distro: Distro,
    /// version of kernel on cluster host
    pub kernel_version: String,
}

/// Full stored host record.
#[derive(Debug, Clone, PartialEq, Eq)]
#[cfg_attr(feature = "serde", derive(Serialize, Deserialize))]
pub struct HostRecord {
    pub id: i64,
    pub hostid: String,
    pub username: String,
    pub address: Address,
    pub port: u16,
    pub identity_path: Option<String>,
    pub slurm: SlurmVersion,
    pub distro: Distro,
    pub kernel_version: String,
    pub created_at: String, // RFC3339
    pub updated_at: String, // RFC3339
}

#[derive(Debug, Error)]
pub enum HostStoreError {
    #[error("sqlx error: {0}")]
    Sqlx(#[from] sqlx::Error),
    #[error("invalid address (both hostname and ip are missing)")]
    InvalidAddress,
    #[error("empty hostid")]
    EmptyHostId,
}

pub type Result<T> = std::result::Result<T, HostStoreError>;

/// Async store that uses a `SqlitePool` (cloneable, Send + Sync).
#[derive(Clone)]
pub struct HostStore {
    pool: SqlitePool,
}

impl HostStore {
    /// Open (or create) a file-backed SQLite DB and run bootstrap/migrations.
    pub async fn open<P: AsRef<Path>>(path: P) -> Result<Self> {
        let url = format!("sqlite://{}", path.as_ref().to_string_lossy());
        let opts = SqliteConnectOptions::from_str(&url)?
            .create_if_missing(true)
            .foreign_keys(true)
            .busy_timeout(Duration::from_secs(5));
        let pool = SqlitePoolOptions::new()
            .max_connections(5)
            .connect_with(opts)
            .await?;
        let store = Self { pool };
        store.bootstrap().await?;
        Ok(store)
    }

    /// Open an in-memory store (handy for tests).
    pub async fn open_memory() -> Result<Self> {
        let opts = SqliteConnectOptions::from_str("sqlite::memory:")?
            .create_if_missing(true)
            .foreign_keys(true)
            .busy_timeout(Duration::from_secs(5));
        let pool = SqlitePoolOptions::new()
            .max_connections(1)
            .connect_with(opts)
            .await?;
        let store = Self { pool };
        store.bootstrap().await?;
        Ok(store)
    }

    async fn bootstrap(&self) -> Result<()> {
        // Improve concurrency for file DBs.
        let _ = sqlx::query("PRAGMA journal_mode=WAL;")
            .execute(&self.pool)
            .await;

        // Initial create (new DBs get hostid from the start).
        sqlx::query(
            r#"
            CREATE TABLE IF NOT EXISTS hosts (
              id INTEGER PRIMARY KEY AUTOINCREMENT,
              hostid TEXT,                       -- unique short id; made UNIQUE/indexed below
              username TEXT NOT NULL,
              hostname TEXT,
              ip TEXT,
              port INTEGER NOT NULL,
              identity_path TEXT,
              slurm_major INTEGER NOT NULL,
              slurm_minor INTEGER NOT NULL,
              slurm_patch INTEGER NOT NULL,
              distro_name TEXT NOT NULL,
              distro_version TEXT NOT NULL,
              kernel_version TEXT NOT NULL,
              created_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ','now')),
              updated_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ','now')),
              CHECK (hostname IS NOT NULL OR ip IS NOT NULL)
            );
            "#,
        )
        .execute(&self.pool)
        .await?;

        // If we're upgrading an existing table, make sure `hostid` exists and is indexed.
        self.ensure_hostid_column_and_index().await?;

        // Other helpful indexes
        sqlx::query(
            r#"
            CREATE UNIQUE INDEX IF NOT EXISTS idx_hosts_user_addr
              ON hosts(username, COALESCE(hostname, ip));
            CREATE INDEX IF NOT EXISTS idx_hosts_hostname ON hosts(hostname);
            CREATE INDEX IF NOT EXISTS idx_hosts_ip ON hosts(ip);
            CREATE INDEX IF NOT EXISTS idx_hosts_username ON hosts(username);
            "#,
        )
        .execute(&self.pool)
        .await?;

        Ok(())
    }

    /// Adds `hostid` if missing, backfills for NULL rows, then enforces uniqueness.
    async fn ensure_hostid_column_and_index(&self) -> Result<()> {
        // Does the column exist?
        let col_exists = sqlx::query("PRAGMA table_info('hosts');")
            .fetch_all(&self.pool)
            .await?
            .iter()
            .any(|r| {
                r.try_get::<String, _>("name")
                    .map(|n| n == "hostid")
                    .unwrap_or(false)
            });

        if !col_exists {
            // Add the column (nullable for existing rows).
            sqlx::query(r#"ALTER TABLE hosts ADD COLUMN hostid TEXT"#)
                .execute(&self.pool)
                .await?;
        }

        // Backfill any NULL hostid with a random short token (16 hex chars).
        // Users can later set their own memorable hostid via update.
        sqlx::query(
            r#"
            UPDATE hosts
               SET hostid = lower(hex(randomblob(8)))
             WHERE hostid IS NULL
            "#,
        )
        .execute(&self.pool)
        .await?;

        // Enforce uniqueness & speed lookups.
        sqlx::query(
            r#"
            CREATE UNIQUE INDEX IF NOT EXISTS idx_hosts_hostid ON hosts(hostid);
            "#,
        )
        .execute(&self.pool)
        .await?;

        Ok(())
    }

    /// Insert a new host. Returns the new row id.
    pub async fn insert_host(&self, host: &NewHost) -> Result<i64> {
        if host.hostid.trim().is_empty() {
            return Err(HostStoreError::EmptyHostId);
        }

        let (hostname, ip) = match &host.address {
            Address::Hostname(h) => (Some(h.as_str()), None),
            Address::Ip(ip) => (None, Some(ip.to_string())),
        };
        if hostname.is_none() && ip.is_none() {
            return Err(HostStoreError::InvalidAddress);
        }

        let rec = sqlx::query(
            r#"
            INSERT INTO hosts(
              hostid,
              username, hostname, ip,
              slurm_major, slurm_minor, slurm_patch,
              distro_name, distro_version, kernel_version, 
              port, identity_path
            ) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)
            RETURNING id
            "#,
        )
        .bind(&host.hostid)
        .bind(&host.username)
        .bind(hostname)
        .bind(ip.as_deref())
        .bind(host.slurm.major)
        .bind(host.slurm.minor)
        .bind(host.slurm.patch)
        .bind(&host.distro.name)
        .bind(&host.distro.version)
        .bind(&host.kernel_version)
        .bind(&host.port)
        .bind(&host.identity_path)
        .fetch_one(&self.pool)
        .await?;

        Ok(rec.try_get::<i64, _>("id")?)
    }

    /// Upsert priority:
    /// 1) If a row with `hostid` exists, update it.
    /// 2) Else, if a row with (username, address) exists, update it and set/replace hostid.
    /// 3) Else, insert a new row.
    pub async fn upsert_host(&self, host: &NewHost) -> Result<i64> {
        if let Some(id) = self.find_id_by_hostid(&host.hostid).await? {
            self.update_host(id, host).await?;
            return Ok(id);
        }
        if let Some(id) = self
            .find_id_by_user_and_address(&host.username, &host.address)
            .await?
        {
            self.update_host(id, host).await?;
            return Ok(id);
        }
        self.insert_host(host).await
    }

    /// Update a host by id with the values from `NewHost`.
    pub async fn update_host(&self, id: i64, host: &NewHost) -> Result<()> {
        if host.hostid.trim().is_empty() {
            return Err(HostStoreError::EmptyHostId);
        }
        let (hostname, ip) = match &host.address {
            Address::Hostname(h) => (Some(h.as_str()), None),
            Address::Ip(ip) => (None, Some(ip.to_string())),
        };
        if hostname.is_none() && ip.is_none() {
            return Err(HostStoreError::InvalidAddress);
        }

        let now = now_rfc3339();
        sqlx::query(
            r#"
            UPDATE hosts SET
              hostid = ?1,
              username = ?2,
              hostname = ?3,
              ip = ?4,
              slurm_major = ?5,
              slurm_minor = ?6,
              slurm_patch = ?7,
              distro_name = ?8,
              distro_version = ?9,
              kernel_version = ?10,
              updated_at = ?11,
              port = ?12,
              identity_path = ?13
            WHERE id = ?14
            "#,
        )
        .bind(&host.hostid)
        .bind(&host.username)
        .bind(hostname)
        .bind(ip.as_deref())
        .bind(host.slurm.major)
        .bind(host.slurm.minor)
        .bind(host.slurm.patch)
        .bind(&host.distro.name)
        .bind(&host.distro.version)
        .bind(&host.kernel_version)
        .bind(now)
        .bind(&host.port)
        .bind(&host.identity_path)
        .bind(id)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    /// Delete a host by numeric id. Returns rows affected (0 or 1).
    pub async fn delete_host(&self, id: i64) -> Result<usize> {
        let res = sqlx::query("DELETE FROM hosts WHERE id = ?")
            .bind(id)
            .execute(&self.pool)
            .await?;
        Ok(res.rows_affected() as usize)
    }

    /// Delete by `hostid`. Returns rows affected (0 or 1).
    pub async fn delete_by_hostid(&self, hostid: &str) -> Result<usize> {
        let res = sqlx::query("DELETE FROM hosts WHERE hostid = ?")
            .bind(hostid)
            .execute(&self.pool)
            .await?;
        Ok(res.rows_affected() as usize)
    }

    /// Get a host by row id.
    pub async fn get_host(&self, id: i64) -> Result<Option<HostRecord>> {
        let row = sqlx::query("SELECT * FROM hosts WHERE id = ?")
            .bind(id)
            .fetch_optional(&self.pool)
            .await?;
        Ok(row.map(row_to_host))
    }

    /// Get a host by `hostid` (fast path via unique index).
    pub async fn get_by_hostid(&self, hostid: &str) -> Result<Option<HostRecord>> {
        let row = sqlx::query("SELECT * FROM hosts WHERE hostid = ?")
            .bind(hostid)
            .fetch_optional(&self.pool)
            .await?;
        Ok(row.map(row_to_host))
    }

    /// Get by (username, hostname) or (username, ip).
    pub async fn get_by_user_and_address(
        &self,
        username: &str,
        address: &Address,
    ) -> Result<Option<HostRecord>> {
        let row = match address {
            Address::Hostname(h) => {
                sqlx::query("SELECT * FROM hosts WHERE username = ? AND hostname = ?")
                    .bind(username)
                    .bind(h)
                    .fetch_optional(&self.pool)
                    .await?
            }
            Address::Ip(ip) => {
                let ip_s = ip.to_string();
                sqlx::query("SELECT * FROM hosts WHERE username = ? AND ip = ?")
                    .bind(username)
                    .bind(&ip_s)
                    .fetch_optional(&self.pool)
                    .await?
            }
        };
        Ok(row.map(row_to_host))
    }

    /// List all hosts (optionally filter by username).
    pub async fn list_hosts(&self, username: Option<&str>) -> Result<Vec<HostRecord>> {
        let mut out = Vec::new();
        let mut rows = if let Some(u) = username {
            sqlx::query("SELECT * FROM hosts WHERE username = ? ORDER BY id ASC")
                .bind(u)
                .fetch(&self.pool)
        } else {
            sqlx::query("SELECT * FROM hosts ORDER BY id ASC").fetch(&self.pool)
        };

        use futures_util::TryStreamExt;
        while let Some(row) = rows.try_next().await? {
            out.push(row_to_host(row));
        }
        Ok(out)
    }

    // --- internals ---

    async fn find_id_by_hostid(&self, hostid: &str) -> Result<Option<i64>> {
        let row = sqlx::query("SELECT id FROM hosts WHERE hostid = ? LIMIT 1")
            .bind(hostid)
            .fetch_optional(&self.pool)
            .await?;
        Ok(row.map(|r| r.try_get::<i64, _>("id").unwrap()))
    }

    async fn find_id_by_user_and_address(
        &self,
        username: &str,
        address: &Address,
    ) -> Result<Option<i64>> {
        let row = match address {
            Address::Hostname(h) => {
                sqlx::query("SELECT id FROM hosts WHERE username = ? AND hostname = ? LIMIT 1")
                    .bind(username)
                    .bind(h)
                    .fetch_optional(&self.pool)
                    .await?
            }
            Address::Ip(ip) => {
                let ip_s = ip.to_string();
                sqlx::query("SELECT id FROM hosts WHERE username = ? AND ip = ? LIMIT 1")
                    .bind(username)
                    .bind(&ip_s)
                    .fetch_optional(&self.pool)
                    .await?
            }
        };
        Ok(row.map(|r| r.try_get::<i64, _>("id").unwrap()))
    }
}

// --- Helpers ---

fn now_rfc3339() -> String {
    OffsetDateTime::now_utc()
        .format(&Rfc3339)
        .unwrap_or_else(|_| "1970-01-01T00:00:00Z".into())
}

fn row_to_host(row: sqlx::sqlite::SqliteRow) -> HostRecord {
    let hostname: Option<String> = row.try_get("hostname").ok().flatten();
    let ip_str: Option<String> = row.try_get("ip").ok().flatten();

    let address = if let Some(h) = hostname {
        Address::Hostname(h)
    } else if let Some(s) = ip_str {
        match s.parse::<IpAddr>() {
            Ok(ip) => Address::Ip(ip),
            Err(_) => Address::Hostname(s),
        }
    } else {
        Address::Hostname("<unknown>".into())
    };

    HostRecord {
        id: row.try_get("id").unwrap(),
        hostid: row.try_get("hostid").unwrap(),
        username: row.try_get("username").unwrap(),
        address,
        slurm: SlurmVersion {
            major: row.try_get("slurm_major").unwrap(),
            minor: row.try_get("slurm_minor").unwrap(),
            patch: row.try_get("slurm_patch").unwrap(),
        },
        distro: Distro {
            name: row.try_get("distro_name").unwrap(),
            version: row.try_get("distro_version").unwrap(),
        },
        kernel_version: row.try_get("kernel_version").unwrap(),
        created_at: row.try_get("created_at").unwrap(),
        updated_at: row.try_get("updated_at").unwrap(),
        port: row.try_get("port").unwrap(),
        identity_path: row.try_get("identity_path").unwrap(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use sqlx::sqlite::{SqliteConnectOptions, SqlitePoolOptions};
    use std::net::IpAddr;
    use std::str::FromStr;

    fn make_host(hostid: &str, username: &str, addr: Address) -> NewHost {
        NewHost {
            hostid: hostid.into(),
            username: username.into(),
            address: addr,
            slurm: SlurmVersion {
                major: 23,
                minor: 11,
                patch: 5,
            },
            distro: Distro {
                name: "Ubuntu".into(),
                version: "22.04".into(),
            },
            kernel_version: "6.5.0-41-generic".into(),
            port: 22,
            identity_path: Some("/home/jeff/.ssh/id_ed25519".to_string()),
        }
    }

    #[tokio::test]
    async fn round_trip_by_hostid() {
        let db = HostStore::open_memory().await.unwrap();
        let host = make_host("gpu01", "bob", Address::Hostname("node-a".into()));
        let id = db.insert_host(&host).await.unwrap();
        let got = db.get_by_hostid("gpu01").await.unwrap().unwrap();
        assert_eq!(got.id, id);
        assert_eq!(got.hostid, "gpu01");
    }

    #[tokio::test]
    async fn upsert_prefers_hostid() {
        let db = HostStore::open_memory().await.unwrap();
        let ip: IpAddr = "10.0.0.42".parse().unwrap();

        let first = make_host("c1", "carol", Address::Ip(ip));
        let id1 = db.upsert_host(&first).await.unwrap();

        // Change fields and upsert with same hostid; should update same row.
        let mut second = first.clone();
        second.kernel_version = "6.1.0-20-amd64".into();
        let id2 = db.upsert_host(&second).await.unwrap();

        assert_eq!(id1, id2);
        let got = db.get_by_hostid("c1").await.unwrap().unwrap();
        assert_eq!(got.kernel_version, "6.1.0-20-amd64");
    }

    // ----------------------
    // Edge cases start here.
    // ----------------------

    #[tokio::test]
    async fn empty_hostid_rejected_on_insert() {
        let db = HostStore::open_memory().await.unwrap();
        let host = make_host("", "alice", Address::Hostname("h1".into()));
        let err = db.insert_host(&host).await.unwrap_err();
        matches!(err, HostStoreError::EmptyHostId);
    }

    #[tokio::test]
    async fn duplicate_hostid_rejected_on_insert() {
        let db = HostStore::open_memory().await.unwrap();
        let h1 = make_host("dup1", "u1", Address::Hostname("h1".into()));
        let h2 = make_host("dup1", "u2", Address::Hostname("h2".into()));

        db.insert_host(&h1).await.unwrap();
        let err = db.insert_host(&h2).await.unwrap_err();

        // Should surface a UNIQUE constraint error from SQLite via sqlx
        match err {
            HostStoreError::Sqlx(e) => {
                assert!(e.to_string().to_lowercase().contains("unique"));
            }
            other => panic!("expected sqlx unique-constraint error, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn upsert_by_user_addr_replaces_hostid_if_new() {
        let db = HostStore::open_memory().await.unwrap();

        // Initially store with hostid "old"
        let h_old = make_host("old", "u", Address::Hostname("same-node".into()));
        let id = db.insert_host(&h_old).await.unwrap();

        // Upsert same (username, address) but with a new hostid "new"
        let h_new = make_host("new", "u", Address::Hostname("same-node".into()));
        let id2 = db.upsert_host(&h_new).await.unwrap();
        assert_eq!(id, id2);

        // Old id should disappear; new hostid should work.
        assert!(db.get_by_hostid("old").await.unwrap().is_none());
        let got = db.get_by_hostid("new").await.unwrap().unwrap();
        assert_eq!(got.id, id);
        assert_eq!(got.hostid, "new");
    }

    #[tokio::test]
    async fn get_by_hostid_not_found_returns_none() {
        let db = HostStore::open_memory().await.unwrap();
        assert!(db.get_by_hostid("nope").await.unwrap().is_none());
    }

    #[tokio::test]
    async fn delete_by_hostid_returns_0_when_missing_and_1_when_deleted() {
        let db = HostStore::open_memory().await.unwrap();
        assert_eq!(db.delete_by_hostid("nope").await.unwrap(), 0);

        let h = make_host("d1", "u", Address::Hostname("h".into()));
        db.insert_host(&h).await.unwrap();
        assert_eq!(db.delete_by_hostid("d1").await.unwrap(), 1);
        assert!(db.get_by_hostid("d1").await.unwrap().is_none());
    }

    #[tokio::test]
    async fn update_hostid_conflict_is_rejected() {
        let db = HostStore::open_memory().await.unwrap();

        let a = make_host("a", "u", Address::Hostname("h1".into()));
        let b = make_host("b", "u", Address::Hostname("h2".into()));

        let id_a = db.insert_host(&a).await.unwrap();
        let id_b = db.insert_host(&b).await.unwrap();

        // Try to change B's hostid to "a" (already taken)
        let mut b2 = b.clone();
        b2.hostid = "a".into();
        let err = db.update_host(id_b, &b2).await.unwrap_err();

        match err {
            HostStoreError::Sqlx(e) => {
                assert!(e.to_string().to_lowercase().contains("unique"));
            }
            other => panic!("expected unique constraint failure, got {other:?}"),
        }

        // Ensure A unaffected.
        let a_fresh = db.get_by_hostid("a").await.unwrap().unwrap();
        assert_eq!(a_fresh.id, id_a);
    }

    #[tokio::test]
    async fn ip_roundtrip_and_lookup() {
        let db = HostStore::open_memory().await.unwrap();
        let ip: IpAddr = "2001:db8::1".parse().unwrap();
        let h = make_host("v6node", "net", Address::Ip(ip));
        db.insert_host(&h).await.unwrap();

        // Lookup by hostid
        let got = db.get_by_hostid("v6node").await.unwrap().unwrap();
        match got.address {
            Address::Ip(parsed) => assert_eq!(parsed, ip),
            _ => panic!("expected IP address"),
        }

        // Lookup by (username, ip)
        let got2 = db
            .get_by_user_and_address("net", &Address::Ip(ip))
            .await
            .unwrap();
        assert!(got2.is_some());
    }
}

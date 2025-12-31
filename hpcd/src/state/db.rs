use sqlx::{
    Row, SqlitePool,
    sqlite::{SqliteConnectOptions, SqlitePoolOptions},
};
use std::{net::IpAddr, path::Path, str::FromStr, time::Duration};
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

#[derive(Debug)]
pub enum ParseSlurmVersionError {
    WrongFormat,              // not exactly 3 dot-separated parts
    NotANumber, // one part isn’t an integer
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
impl FromStr for SlurmVersion {
    type Err = ParseSlurmVersionError;
    fn from_str(s: &str) -> std::result::Result<Self, Self::Err> {
        let mut parts = s.split(".");

        let major = parts
            .next()
            .ok_or(ParseSlurmVersionError::WrongFormat)?
            .parse::<i64>()
            .map_err(|_| ParseSlurmVersionError::NotANumber)?;
        let minor = parts
            .next()
            .ok_or(ParseSlurmVersionError::WrongFormat)?
            .parse::<i64>()
            .map_err(|_| ParseSlurmVersionError::NotANumber)?;
        let patch = parts
            .next()
            .ok_or(ParseSlurmVersionError::WrongFormat)?
            .parse::<i64>()
            .map_err(|_| ParseSlurmVersionError::NotANumber)?;

        if parts.next().is_some() {
            return Err(ParseSlurmVersionError::WrongFormat);
        }
        Ok(SlurmVersion { major, minor, patch })
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

    /// availability of accounting (to be used for more fine-grained control over jobs)
    pub accounting_available: bool,

    pub default_base_path: Option<String>,
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
    pub accounting_available: bool,
    pub default_base_path: Option<String>,
}

#[derive(Debug, Error)]
pub enum HostStoreError {
    #[error("sqlx error: {0}")]
    Sqlx(#[from] sqlx::Error),
    #[error("invalid address (both hostname and ip are missing)")]
    InvalidAddress,
    #[error("empty hostid")]
    EmptyHostId,
    #[error("host not found: {0}")]
    HostNotFound(String),
}

// This structure is used for inserting data into the db when data first appears
#[derive(Debug, Clone, PartialEq, Eq)]
#[cfg_attr(feature = "serde", derive(Serialize, Deserialize))]
pub struct NewPartition {
    /// Slurm partition name
    pub name: String,
    /// Arbitrary metadata
    pub info: Option<serde_json::Value>,
}

// This structure is returned from the db
#[derive(Debug, Clone, PartialEq, Eq)]
#[cfg_attr(feature = "serde", derive(Serialize, Deserialize))]
pub struct PartitionRecord {
    pub id: i64,
    pub host_id: i64,
    pub name: String,
    pub info: Option<serde_json::Value>,
    pub created_at: String,
    pub updated_at: String,
}
pub type Result<T> = std::result::Result<T, HostStoreError>;

#[derive(Debug, Clone, PartialEq, Eq)]
#[cfg_attr(feature = "serde", derive(Serialize, Deserialize))]
pub struct NewJob {
    /// Slurm job ID; host-specific. Database will additionally keep its own, internal id.
    pub slurm_id: Option<i64>,
    /// Host ID on which the job is submitted.
    pub host_id: i64,
    pub local_path: String,
    pub remote_path: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
#[cfg_attr(feature = "serde", derive(Serialize, Deserialize))]
pub struct JobRecord {
    pub id: i64,
    pub slurm_id: Option<i64>,
    pub host_id: String,
    pub created_at: String,
    pub finished_at: Option<String>,
    pub is_completed: bool,
    pub terminal_state: Option<String>,
    pub local_path: String,
    pub remote_path: String,
}
/// Async store
/// TODO: since it stores not only hosts but also partitions, jobs etc., this needs to be renamed.
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
    #[allow(dead_code)]
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
              accounting_available INTEGER NOT NULL,
              default_base_path TEXT,
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

        self.ensure_partitions_table().await?;
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
        self.ensure_jobs_table().await?;
        Ok(())
    }
    async fn ensure_partitions_table(&self) -> Result<()> {
        sqlx::query(
            r#"
        CREATE TABLE IF NOT EXISTS partitions (
          id INTEGER PRIMARY KEY AUTOINCREMENT,
          host_id INTEGER NOT NULL
            REFERENCES hosts(id) ON DELETE CASCADE,
          name TEXT NOT NULL,
          info TEXT, -- JSON (stringified)
          created_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ','now')),
          updated_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ','now')),
          UNIQUE(host_id, name)
        );
        CREATE INDEX IF NOT EXISTS idx_partitions_host_id ON partitions(host_id);
        CREATE INDEX IF NOT EXISTS idx_partitions_name    ON partitions(name);
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
    async fn ensure_jobs_table(&self) -> Result<()> {
        sqlx::query(
            r#"
            create table if not exists jobs (
            id integer primary key autoincrement,
            slurm_id integer,
            host_id integer not null references hosts(id) on delete cascade,
            local_path TEXT NOT NULL,
            remote_path TEXT NOT NULL,
            is_completed boolean default 0,
            created_at text not null default (strftime('%Y-%m-%dT%H:%M:%fZ','now')),
            completed_at text,
            terminal_state text);
    "#,
        )
        .execute(&self.pool)
        .await?;
        let has_terminal_state = sqlx::query("PRAGMA table_info('jobs');")
            .fetch_all(&self.pool)
            .await?
            .iter()
            .any(|r| {
                r.try_get::<String, _>("name")
                    .map(|n| n == "terminal_state")
                    .unwrap_or(false)
            });
        if !has_terminal_state {
            sqlx::query("ALTER TABLE jobs ADD COLUMN terminal_state TEXT")
                .execute(&self.pool)
                .await?;
        }
        sqlx::query("CREATE INDEX IF NOT EXISTS idx_jobs_host_id ON jobs(host_id)")
            .execute(&self.pool)
            .await?;
        sqlx::query(
            "CREATE INDEX IF NOT EXISTS idx_jobs_slurm_id_host_id ON jobs(slurm_id,host_id)",
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
              port, identity_path,accounting_available, default_base_path
            ) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)
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
        .bind(host.port)
        .bind(&host.identity_path)
        .bind(host.accounting_available)
        .bind(&host.default_base_path)
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
              identity_path = ?13,
              accounting_available = ?14,
              default_base_path = ?15
            WHERE id = ?16
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
        .bind(host.port)
        .bind(&host.identity_path)
        .bind(host.accounting_available)
        .bind(&host.default_base_path)
        .bind(id)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    /// Delete a host by numeric id. Returns rows affected (0 or 1).
    #[allow(dead_code)]
    pub async fn delete_host(&self, id: i64) -> Result<usize> {
        let res = sqlx::query("DELETE FROM hosts WHERE id = ?")
            .bind(id)
            .execute(&self.pool)
            .await?;
        Ok(res.rows_affected() as usize)
    }

    /// Delete by `hostid`. Returns rows affected (0 or 1).
    #[allow(dead_code)]
    pub async fn delete_by_hostid(&self, hostid: &str) -> Result<usize> {
        let res = sqlx::query("DELETE FROM hosts WHERE hostid = ?")
            .bind(hostid)
            .execute(&self.pool)
            .await?;
        Ok(res.rows_affected() as usize)
    }

    /// Get a host by row id.
    #[allow(dead_code)]
    pub async fn get_host(&self, id: i64) -> Result<Option<HostRecord>> {
        let row = sqlx::query("select * from hosts where id = ?")
            .bind(id)
            .fetch_optional(&self.pool)
            .await?;
        Ok(row.map(row_to_host))
    }

    /// Get a host by human-readable `hostid` (fast, uses unique index).
    pub async fn get_by_hostid(&self, hostid: &str) -> Result<Option<HostRecord>> {
        let row = sqlx::query("select * from hosts where hostid = ?")
            .bind(hostid)
            .fetch_optional(&self.pool)
            .await?;
        Ok(row.map(row_to_host))
    }

    /// Get by (username, hostname) or (username, ip).
    #[allow(dead_code)]
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
    #[allow(dead_code)]
    pub async fn upsert_partition_by_hostid(
        &self,
        hostid: &str,
        spec: &NewPartition,
    ) -> Result<i64> {
        let host_id = self
            .find_id_by_hostid(hostid)
            .await?
            .ok_or_else(|| HostStoreError::HostNotFound(hostid.into()))?;
        let info_text = spec.info.as_ref().map(|v| v.to_string());
        let rec = sqlx::query(r#"
        INSERT INTO partitions(host_id, name, info)
        VALUES (?1, ?2, ?3)
        ON CONFLICT (host_id, name)
        DO UPDATE SET
            info = excluded.info, -- excluded is sqlite name for table with values that would be inserted if conflict didn't happen
            updated_at = strftime('%Y-%m-%dT%H:%M:%fZ','now')
        RETURNING id;"#).bind(host_id).bind(&spec.name).bind(&info_text).fetch_one(&self.pool).await?;
        Ok(rec.try_get::<i64, _>("id")?)
    }
    #[allow(dead_code)]
    pub async fn get_partition_by_hostid_and_name(
        &self,
        hostid: &str,
        name: &str,
    ) -> Result<Option<PartitionRecord>> {
        let row = sqlx::query(
            r#"
        SELECT p.*
        FROM partitions AS p
        JOIN hosts AS h ON h.id = p.host_id
        WHERE h.hostid = ?1 AND p.name = ?2
        LIMIT 1
    "#,
        )
        .bind(hostid)
        .bind(name)
        .fetch_optional(&self.pool)
        .await?;
        Ok(row.map(row_to_partition))
    }
    #[allow(dead_code)]
    pub async fn list_partitions_by_hostid(&self, hostid: &str) -> Result<Vec<PartitionRecord>> {
        let rows = sqlx::query(
            r#"
        SELECT p.*
        FROM partitions p
        JOIN hosts h ON h.id = p.host_id
        WHERE h.hostid = ?1
        ORDER BY p.name ASC
    "#,
        )
        .bind(hostid)
        .fetch_all(&self.pool)
        .await?;

        Ok(rows.into_iter().map(row_to_partition).collect())
    }
    #[allow(dead_code)]
    pub async fn replace_partitions_by_hostid(
        &self,
        hostid: &str,
        parts: &[NewPartition],
    ) -> Result<()> {
        let mut tx = self.pool.begin().await?;

        let host_id = self
            .find_id_by_hostid(hostid)
            .await?
            .ok_or_else(|| HostStoreError::HostNotFound(hostid.to_string()))?;

        sqlx::query("DELETE FROM partitions WHERE host_id = ?")
            .bind(host_id)
            .execute(&mut *tx)
            .await?;

        for p in parts {
            let info_text = p.info.as_ref().map(|v| v.to_string());
            sqlx::query(
                r#"
            INSERT INTO partitions(host_id, name, info)
            VALUES (?1, ?2, ?3)
        "#,
            )
            .bind(host_id)
            .bind(&p.name)
            .bind(info_text)
            .execute(&mut *tx)
            .await?;
        }

        tx.commit().await?;
        Ok(())
    }
    /// fetch a host (by hostid) along with all its partitions
    #[allow(dead_code)]
    pub async fn get_host_with_partitions_by_hostid(
        &self,
        hostid: &str,
    ) -> Result<Option<(HostRecord, Vec<PartitionRecord>)>> {
        let host = self.get_by_hostid(hostid).await?;
        let Some(host) = host else { return Ok(None) };
        let parts = self.list_partitions_by_hostid(hostid).await?;
        Ok(Some((host, parts)))
    }

    pub async fn insert_job(&self, job: &NewJob) -> Result<i64> {
        let rec = sqlx::query(
            r#"
        insert into jobs(slurm_id, host_id, local_path, remote_path)
        values (?1, ?2, ?3, ?4)
        returning id;
    "#,
        )
        .bind(job.slurm_id)
        .bind(job.host_id)
        .bind(job.local_path.clone())
        .bind(job.remote_path.clone())
        .fetch_one(&self.pool)
        .await?;
        Ok(rec.try_get::<i64, _>("id")?)
    }

    pub async fn list_jobs_for_host(&self, host_id: i64) -> Result<Vec<JobRecord>> {
        let rows = sqlx::query(
            r#"
            with all_jobs as (
                select * from jobs where host_id = ?1
            )
            select aj.id as id, aj.slurm_id as slurm_id,aj.is_completed as is_completed,aj.created_at as created_at,aj.completed_at as completed_at,aj.terminal_state as terminal_state,aj.local_path as local_path,aj.remote_path as remote_path,h.hostid as hostid
            from all_jobs aj
            join hosts h
              on aj.host_id = h.id;
            "#,
        )
        .bind(host_id)
        .fetch_all(&self.pool)
        .await?;
        Ok(rows.into_iter().map(row_to_job).collect())
    }
    pub async fn get_job_by_job_id(&self, id: i64) -> Result<Option<JobRecord>> {
        let row = sqlx::query(
            r#"
            select aj.id as id,
                   aj.slurm_id as slurm_id,
                   aj.is_completed as is_completed,
                   aj.created_at as created_at,
                   aj.completed_at as completed_at,
                   aj.terminal_state as terminal_state,
                   aj.local_path as local_path,
                   aj.remote_path as remote_path,
                   h.hostid as hostid
            from jobs aj
            join hosts h on aj.host_id = h.id
            where aj.id = ?1
            "#,
        )
        .bind(id)
        .fetch_optional(&self.pool)
        .await?;
        Ok(row.map(row_to_job))
    }

    pub async fn list_all_jobs(&self) -> Result<Vec<JobRecord>> {
        let rows = sqlx::query(
            r#"
            with all_jobs as (
                select * from jobs
            )
            select aj.id as id, aj.slurm_id as slurm_id,aj.is_completed as is_completed,aj.created_at as created_at,aj.completed_at as completed_at,aj.terminal_state as terminal_state,aj.local_path as local_path,aj.remote_path as remote_path,h.hostid as hostid
            from all_jobs aj
            join hosts h
              on aj.host_id = h.id;
            "#,
        )
        .fetch_all(&self.pool)
        .await?;
        Ok(rows.into_iter().map(row_to_job).collect())
    }

    pub async fn list_running_jobs(&self) -> Result<Vec<JobRecord>> {
        let rows = sqlx::query(
            r#"
            select aj.id as id, aj.slurm_id as slurm_id,aj.is_completed as is_completed,aj.created_at as created_at,aj.completed_at as completed_at,aj.terminal_state as terminal_state,aj.local_path as local_path,aj.remote_path as remote_path,h.hostid as hostid
            from jobs aj
            join hosts h
              on aj.host_id = h.id
            where aj.is_completed = 0;
            "#,
        )
        .fetch_all(&self.pool)
        .await?;
        Ok(rows.into_iter().map(row_to_job).collect())
    }

    pub async fn mark_job_completed(&self, id: i64, terminal_state: Option<&str>) -> Result<()> {
        let now = now_rfc3339();
        sqlx::query(
            r#"
            update jobs
            set is_completed = 1,
                completed_at = ?1,
                terminal_state = ?2
            where id = ?3
            "#,
        )
        .bind(now)
        .bind(terminal_state)
        .bind(id)
        .execute(&self.pool)
        .await?;
        Ok(())
    }
}

// -- helpers

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

    let accounting_available = row.try_get::<i64, _>("accounting_available").unwrap_or(0) != 0;
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
        accounting_available,
        default_base_path: row.try_get("default_base_path").unwrap(),
    }
}

fn row_to_partition(row: sqlx::sqlite::SqliteRow) -> PartitionRecord {
    let info_text: Option<String> = row.try_get("info").ok().flatten();
    let info = info_text.and_then(|s| serde_json::from_str::<serde_json::Value>(&s).ok());

    PartitionRecord {
        id: row.try_get("id").unwrap(),
        host_id: row.try_get("host_id").unwrap(),
        name: row.try_get("name").unwrap(),
        info,
        created_at: row.try_get("created_at").unwrap(),
        updated_at: row.try_get("updated_at").unwrap(),
    }
}

fn row_to_job(row: sqlx::sqlite::SqliteRow) -> JobRecord {
    JobRecord {
        id: row.try_get("id").unwrap(),
        slurm_id: row.try_get("slurm_id").unwrap(),
        host_id: row.try_get("hostid").unwrap(),
        created_at: row.try_get("created_at").unwrap(),
        finished_at: row.try_get("completed_at").unwrap(),
        is_completed: row.try_get("is_completed").unwrap(),
        terminal_state: row.try_get("terminal_state").unwrap(),
        local_path: row.try_get("local_path").unwrap(),
        remote_path: row.try_get("remote_path").unwrap(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;
    use std::net::IpAddr;

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
            accounting_available: true,
            default_base_path: Some("/home/jeff/runs".to_string()),
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

    // Edge cases start here.

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
        let h = make_host("v6node", "alice", Address::Ip(ip));
        db.insert_host(&h).await.unwrap();

        // Lookup by hostid
        let got = db.get_by_hostid("v6node").await.unwrap().unwrap();
        match got.address {
            Address::Ip(parsed) => assert_eq!(parsed, ip),
            _ => panic!("expected IP address"),
        }

        // Lookup by (username, ip)
        let got2 = db
            .get_by_user_and_address("alice", &Address::Ip(ip))
            .await
            .unwrap();
        assert!(got2.is_some());
    }

    #[tokio::test]
    async fn insert_and_retrieve_partitions_for_hostid() {
        let hostid = "gpurig";
        let db = HostStore::open_memory().await.unwrap();
        let ip: IpAddr = "2001:db8::1".parse().unwrap();
        let host = NewHost {
            hostid: hostid.into(),
            username: "alice".into(),
            address: Address::Ip(ip),
            port: 22,
            slurm: SlurmVersion {
                major: 23,
                minor: 11,
                patch: 5,
            },
            distro: Distro {
                name: "ubuntu".into(),
                version: "22.04".into(),
            },
            kernel_version: "6.5.0-41-generic".into(),
            identity_path: Some("/home/alice/.ssh/id_ed25519".to_string()),
            accounting_available: true,
            default_base_path: Some("/home/alice/runs".to_string()),
        };
        db.insert_host(&host).await.unwrap();
        let mut info_map: HashMap<String, serde_json::Value> = HashMap::new();
        info_map.insert("MaxTime".into(), serde_json::json!("24:00:00"));
        info_map.insert("MaxCPUsPerNode".into(), serde_json::json!("UNLIMITED"));
        info_map.insert("QoS".into(), serde_json::json!(["normal", "high"]));
        info_map.insert("State".into(), serde_json::json!("UP"));
        info_map.insert("PriorityTier".into(), serde_json::json!(1));
        info_map.insert(
            "TRES".into(),
            serde_json::json!("cpu=64,mem=990000M,node=2,billing=64,gres/gpu=8"),
        );
        let spec = NewPartition {
            name: "gpu".into(),
            info: Some(serde_json::to_value(&info_map).unwrap()),
        };
        let _partition_id = db.upsert_partition_by_hostid(hostid, &spec).await.unwrap();
        let part = db
            .get_partition_by_hostid_and_name(hostid, "gpu")
            .await
            .unwrap()
            .expect("partition should exist");
        let obj = part
            .info
            .expect("info should be present")
            .as_object()
            .cloned()
            .expect("info should be a JSON object");
        assert_eq!(obj.get("MaxTime"), Some(&serde_json::json!("24:00:00")));
        assert_eq!(
            obj.get("TRES"),
            Some(&serde_json::json!(
                "cpu=64,mem=990000M,node=2,billing=64,gres/gpu=8"
            ))
        );
        assert_eq!(obj.get("PriorityTier").and_then(|v| v.as_i64()), Some(1));

        assert_eq!(obj.get("State"), Some(&serde_json::json!("UP")));
        let qos = obj
            .get("QoS")
            .and_then(|v| v.as_array())
            .expect("qos array");
        assert_eq!(
            qos,
            &vec![serde_json::json!("normal"), serde_json::json!("high")]
        );
    }

    #[tokio::test]
    async fn list_jobs_for_host_filters_and_maps_fields() {
        let db = HostStore::open_memory().await.unwrap();
        let host = make_host("host-a", "alice", Address::Hostname("node-a".into()));
        db.insert_host(&host).await.unwrap();

        let host_row = db.get_by_hostid("host-a").await.unwrap().unwrap();
        let job = NewJob {
            slurm_id: Some(42),
            host_id: host_row.id,
            local_path: "/tmp/local".into(),
            remote_path: "/remote/run".into(),
        };
        db.insert_job(&job).await.unwrap();

        let jobs = db.list_jobs_for_host(host_row.id).await.unwrap();
        assert_eq!(jobs.len(), 1);
        let got = &jobs[0];
        assert_eq!(got.slurm_id, Some(42));
        assert_eq!(got.host_id, "host-a");
        assert_eq!(got.local_path, "/tmp/local");
        assert_eq!(got.remote_path, "/remote/run");
        assert!(!got.is_completed);
        assert!(got.finished_at.is_none());
        assert!(got.terminal_state.is_none());
        assert!(!got.created_at.is_empty());
    }

    #[tokio::test]
    async fn get_job_by_job_id_returns_row() {
        let db = HostStore::open_memory().await.unwrap();
        let host = make_host("host-a", "alice", Address::Hostname("node-a".into()));
        db.insert_host(&host).await.unwrap();

        let host_row = db.get_by_hostid("host-a").await.unwrap().unwrap();
        let job = NewJob {
            slurm_id: Some(42),
            host_id: host_row.id,
            local_path: "/tmp/local-a".into(),
            remote_path: "/remote/run-a".into(),
        };
        let job_id = db.insert_job(&job).await.unwrap();

        let got = db.get_job_by_job_id(job_id).await.unwrap().unwrap();
        assert_eq!(got.id, job_id);
        assert_eq!(got.slurm_id, Some(42));
        assert_eq!(got.host_id, "host-a");
    }

    #[tokio::test]
    async fn list_running_jobs_skips_completed_jobs() {
        let db = HostStore::open_memory().await.unwrap();
        let host = make_host("host-a", "alice", Address::Hostname("node-a".into()));
        db.insert_host(&host).await.unwrap();

        let host_row = db.get_by_hostid("host-a").await.unwrap().unwrap();
        let job1 = NewJob {
            slurm_id: Some(101),
            host_id: host_row.id,
            local_path: "/tmp/local1".into(),
            remote_path: "/remote/run1".into(),
        };
        let job2 = NewJob {
            slurm_id: Some(102),
            host_id: host_row.id,
            local_path: "/tmp/local2".into(),
            remote_path: "/remote/run2".into(),
        };
        let job1_id = db.insert_job(&job1).await.unwrap();
        let job2_id = db.insert_job(&job2).await.unwrap();

        db.mark_job_completed(job1_id, Some("FAILED"))
            .await
            .unwrap();

        let running = db.list_running_jobs().await.unwrap();
        assert_eq!(running.len(), 1);
        assert_eq!(running[0].id, job2_id);

        let all = db.list_all_jobs().await.unwrap();
        let completed = all.iter().find(|j| j.id == job1_id).unwrap();
        assert!(completed.is_completed);
        assert!(completed.finished_at.is_some());
        assert_eq!(completed.terminal_state.as_deref(), Some("FAILED"));
    }
}

//! Shared storage through an existing, separately installed Garage 2.x program.
//! Requests travel on stdin; captured output and diagnostics are never logged.

mod error;
mod process;
mod settings;

use std::{
    ffi::OsString,
    fmt,
    path::Path,
    sync::{Arc, atomic::AtomicBool},
    time::{Duration, Instant},
};

use homepool_core::Member;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

pub use error::Error;

/// A consumer's newly issued credentials. Debug deliberately redacts both fields.
/// Deliver them directly to a private credential store; never print them.
pub struct Credentials {
    access_key_id: String,
    secret_access_key: String,
}

impl Credentials {
    pub fn access_key_id(&self) -> &str {
        &self.access_key_id
    }

    pub fn secret_access_key(&self) -> &str {
        &self.secret_access_key
    }
}

impl fmt::Debug for Credentials {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("Credentials([redacted])")
    }
}

/// One bucket and a fresh key with read/write access only to that bucket.
#[derive(Debug)]
pub struct Consumer {
    pub bucket: String,
    pub credentials: Credentials,
}

/// Logical usage of homepool consumer buckets, not physical disk consumption.
/// Garage's distributed counters may lag recent writes and deletes.
#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
pub struct StorageUsage {
    pub buckets: u64,
    pub objects: u64,
    pub used_bytes: u64,
    pub unfinished_upload_bytes: u64,
}

/// Drives Garage's v2 `json-api` CLI over its configured RPC connection.
/// The program/configuration must be trusted. Garage is never linked or bundled.
pub struct Garage {
    executable: OsString,
    prefix: Vec<OsString>,
    operation_timeout: Duration,
    cancelled: Arc<AtomicBool>,
}

impl Garage {
    /// Use the installed `garage` executable and an existing private config file.
    pub fn new(config: &Path) -> Self {
        Self::with_command("garage", [OsString::from("-c"), config.as_os_str().into()])
    }

    /// Select a deployment wrapper (for example a container exec command).
    /// Prefix arguments must contain no credentials; use Garage's private config.
    pub fn with_command(
        executable: impl Into<OsString>,
        prefix: impl IntoIterator<Item = OsString>,
    ) -> Self {
        Self {
            executable: executable.into(),
            prefix: prefix.into_iter().collect(),
            operation_timeout: settings::OPERATION_TIMEOUT,
            cancelled: Arc::new(AtomicBool::new(false)),
        }
    }

    /// Set one end-to-end deadline per public operation, including all subprocess
    /// I/O, cluster queries and attempted cleanup. Zero/unrepresentable limits fail.
    /// Expiry never retries a mutation; inspect Garage privately before retrying.
    pub fn with_operation_timeout(mut self, timeout: Duration) -> Result<Self, Error> {
        if timeout.is_zero() || Instant::now().checked_add(timeout).is_none() {
            return Err(Error("invalid Garage operation timeout"));
        }
        self.operation_timeout = timeout;
        Ok(self)
    }

    /// Share a cancellation flag across all operations. Setting it stops the local
    /// process group and reaps its child before returning `Error::CANCELLED`.
    /// Cancellation never resets the flag or retries a mutation; inspect its
    /// unknown outcome privately before deciding whether to retry.
    pub fn with_cancellation(mut self, cancelled: Arc<AtomicBool>) -> Self {
        self.cancelled = cancelled;
        self
    }

    fn deadline(&self) -> Result<Instant, Error> {
        Instant::now()
            .checked_add(self.operation_timeout)
            .ok_or(Error("invalid Garage operation timeout"))
    }

    fn call(&self, deadline: Instant, operation: &str, request: Value) -> Result<Value, Error> {
        process::call(
            &self.executable,
            &self.prefix,
            operation,
            &request,
            deadline,
            &self.cancelled,
        )
    }

    /// Stage the member's declared storage bytes as this node's layout capacity.
    /// A zero portion is rejected, rather than silently removing a storage node.
    /// Staging does not activate the layout. Review it before `apply_layout`.
    /// The full ID must resolve in cluster status; unknown nodes never stage roles.
    /// Serialize layout administration: Garage's draft is shared by all operators.
    pub fn stage_portion(&self, member: &Member, node_id: &str, zone: &str) -> Result<(), Error> {
        let deadline = self.deadline()?;
        if member.capabilities.storage_bytes == 0
            || node_id.len() != 64
            || !node_id.bytes().all(|b| b.is_ascii_hexdigit())
            || zone.trim().is_empty()
        {
            return Err(Error(
                "Garage portion requires storage, a full node ID and a zone",
            ));
        }
        let status = self.call(deadline, "GetClusterStatus", Value::Null)?;
        let nodes = status["nodes"]
            .as_array()
            .ok_or(Error("invalid Garage cluster status"))?;
        if !nodes.iter().any(|node| {
            node["id"]
                .as_str()
                .is_some_and(|id| id.eq_ignore_ascii_case(node_id))
        }) {
            return Err(Error("Garage storage node is not known to the cluster"));
        }
        self.call(
            deadline,
            "UpdateClusterLayout",
            json!({"roles": [{
                "id": node_id.to_ascii_lowercase(),
                "zone": zone,
                "capacity": member.capabilities.storage_bytes,
                "tags": [format!("homepool-member-{}", member.id.0)]
            }]}),
        )?;
        Ok(())
    }

    /// Apply the entire reviewed draft, using the current layout version plus one.
    /// Capacity is a placement weight, not a disk quota or a backup guarantee.
    pub fn apply_layout(&self, next_version: u64) -> Result<(), Error> {
        self.call(
            self.deadline()?,
            "ApplyClusterLayout",
            json!({"version": next_version}),
        )?;
        Ok(())
    }

    /// Create exactly one new bucket/key for an opaque pool-local consumer ID.
    /// An existing bucket causes failure before a key is created: this does not
    /// rotate keys or grant rights to an existing identity. No bucket owner or
    /// create-bucket permission is granted. Persist credentials privately on success.
    /// On a later failure, known newly created resources are cleaned up best-effort;
    /// inspect Garage after any failure before retrying (RPC is not transactional).
    pub fn create_consumer(&self, consumer_id: u64) -> Result<Consumer, Error> {
        let deadline = self.deadline()?;
        let bucket = format!("homepool-consumer-{consumer_id}");
        let created = self.call(deadline, "CreateBucket", json!({"globalAlias": bucket}))?;
        let bucket_id = text(&created, "id")?;
        let key = match self.call(
            deadline,
            "CreateKey",
            json!({"name": bucket, "deny": {"createBucket": true}}),
        ) {
            Ok(key) => key,
            Err(error) => {
                let _ = self.call(deadline, "DeleteBucket", json!({"id": bucket_id}));
                process::check_operation(deadline, &self.cancelled)?;
                return Err(error);
            }
        };
        let provision = || {
            let access_key_id = text(&key, "accessKeyId")?;
            let secret_access_key = text(&key, "secretAccessKey")?;
            self.call(
                deadline,
                "AllowBucketKey",
                json!({"bucketId": bucket_id, "accessKeyId": access_key_id,
                    "permissions": {"read": true, "write": true, "owner": false}}),
            )?;
            Ok(Consumer {
                bucket,
                credentials: Credentials {
                    access_key_id,
                    secret_access_key,
                },
            })
        };
        match provision() {
            Ok(consumer) => Ok(consumer),
            Err(error) => {
                if let Ok(id) = text(&key, "accessKeyId") {
                    let _ = self.call(deadline, "DeleteKey", json!({"id": id}));
                }
                let _ = self.call(deadline, "DeleteBucket", json!({"id": bucket_id}));
                process::check_operation(deadline, &self.cancelled)?;
                Err(error)
            }
        }
    }

    /// Sum each homepool consumer bucket once. Other buckets are excluded.
    /// All probes must succeed; incomplete results never masquerade as zero usage.
    pub fn storage_usage(&self) -> Result<StorageUsage, Error> {
        let deadline = self.deadline()?;
        let listed = self.call(deadline, "ListBuckets", Value::Null)?;
        let buckets = listed
            .as_array()
            .ok_or(Error("invalid Garage bucket list"))?;
        let mut usage = StorageUsage::default();
        for bucket in buckets {
            let aliases = bucket["globalAliases"]
                .as_array()
                .ok_or(Error("invalid Garage bucket aliases"))?;
            if !aliases
                .iter()
                .filter_map(Value::as_str)
                .any(is_consumer_bucket)
            {
                continue;
            }
            let info = self.call(
                deadline,
                "GetBucketInfo",
                json!({"id": text(bucket, "id")?}),
            )?;
            usage.buckets = add(usage.buckets, 1)?;
            usage.objects = add(usage.objects, number(&info, "objects")?)?;
            usage.used_bytes = add(usage.used_bytes, number(&info, "bytes")?)?;
            usage.unfinished_upload_bytes = add(
                usage.unfinished_upload_bytes,
                number(&info, "unfinishedMultipartUploadBytes")?,
            )?;
        }
        process::check_operation(deadline, &self.cancelled)?;
        Ok(usage)
    }
}

fn is_consumer_bucket(alias: &str) -> bool {
    alias
        .strip_prefix("homepool-consumer-")
        .is_some_and(|id| id.parse::<u64>().is_ok_and(|value| value.to_string() == id))
}

fn text(value: &Value, field: &str) -> Result<String, Error> {
    value[field]
        .as_str()
        .filter(|s| !s.is_empty())
        .map(str::to_owned)
        .ok_or(Error("invalid Garage response"))
}

fn number(value: &Value, field: &str) -> Result<u64, Error> {
    value[field]
        .as_u64()
        .ok_or(Error("invalid Garage usage counter"))
}

fn add(left: u64, right: u64) -> Result<u64, Error> {
    left.checked_add(right)
        .ok_or(Error("Garage usage counter overflow"))
}

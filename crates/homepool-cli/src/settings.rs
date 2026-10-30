//! Private member schema and configuration boundary. Core receives only validated types.

use std::{
    ffi::OsString,
    fs,
    path::{Path, PathBuf},
};

use homepool_core::{Capabilities, ComputeBudget, Member, MemberId, MemberState, Pool, Tier};
use toml::{
    Spanned,
    de::{DeTable, DeValue},
};

/// Version of the member file and status JSON contracts.
pub const SCHEMA_VERSION: u64 = 1;
/// Used only when neither an explicit path nor HOMEPOOL_CONFIG is supplied.
const DEFAULT_CONFIG_PATH: &str = "homepool/members.toml";

/// Resolve one file, without falling back after an invalid or unreadable selection.
/// Inputs are captured by the caller so tests never mutate process environment.
pub fn resolve_config(
    explicit: Option<PathBuf>,
    configured: Option<OsString>,
    xdg: Option<OsString>,
) -> Result<PathBuf, String> {
    if let Some(path) = explicit {
        return nonempty(path, "--config");
    }
    if let Some(path) = configured {
        return nonempty(PathBuf::from(path), "HOMEPOOL_CONFIG");
    }
    let root = xdg
        .map(PathBuf::from)
        .ok_or("XDG_CONFIG_HOME is unset; supply --config FILE or HOMEPOOL_CONFIG")?;
    if !root.is_absolute() {
        return Err("XDG_CONFIG_HOME must be an absolute, nonempty directory path; supply --config FILE or HOMEPOOL_CONFIG".into());
    }
    Ok(root.join(DEFAULT_CONFIG_PATH))
}

fn nonempty(path: PathBuf, field: &str) -> Result<PathBuf, String> {
    if path.as_os_str().is_empty() {
        return Err(format!("{field}: file path must not be empty"));
    }
    Ok(path)
}

pub fn load(path: &Path) -> Result<Pool, String> {
    let source =
        fs::read_to_string(path).map_err(|e| format!("{}:1: <file>: {e}", path.display()))?;
    parse(path, &source)
}

/// Parse with spans retained through validation, then discard all TOML values.
pub fn parse(path: &Path, source: &str) -> Result<Pool, String> {
    let document = Document { path, source };
    let root = DeTable::parse(source).map_err(|e| {
        document.error(
            e.span().map_or(0, |span| span.start),
            "<document>",
            e.message(),
        )
    })?;
    let table = root.get_ref();
    document.fields(table, "", &["version", "members"])?;
    let version = document.required(table, root.span().start, "", "version")?;
    if document.unsigned(version, "version", u64::MAX)? != SCHEMA_VERSION {
        return Err(document.error(
            version.span().start,
            "version",
            "unsupported schema version; expected 1",
        ));
    }
    let members = document.required(table, root.span().start, "", "members")?;
    let entries = members.get_ref().as_array().ok_or_else(|| {
        document.error(
            members.span().start,
            "members",
            "expected an array of member tables",
        )
    })?;
    let mut pool = Pool::default();
    for (index, value) in entries.iter().enumerate() {
        let field = format!("members[{index}]");
        let member = document.member(value, &field)?;
        pool.add_member(member).map_err(|_| {
            let table = value.get_ref().as_table().expect("validated member table");
            document.error(
                table.get("id").expect("validated id").span().start,
                &format!("{field}.id"),
                "duplicate member id",
            )
        })?;
    }
    Ok(pool)
}

struct Document<'a> {
    path: &'a Path,
    source: &'a str,
}

impl Document<'_> {
    fn error(&self, offset: usize, field: &str, message: &str) -> String {
        let line = self.source.as_bytes()[..offset.min(self.source.len())]
            .iter()
            .filter(|&&byte| byte == b'\n')
            .count()
            + 1;
        format!("{}:{line}: {field}: {message}", self.path.display())
    }

    fn fields(&self, table: &DeTable<'_>, prefix: &str, allowed: &[&str]) -> Result<(), String> {
        for key in table.keys() {
            if !allowed.contains(&key.get_ref().as_ref()) {
                return Err(self.error(
                    key.span().start,
                    &field_name(prefix, key.get_ref()),
                    "unknown field",
                ));
            }
        }
        Ok(())
    }

    fn required<'a, 'i>(
        &self,
        table: &'a DeTable<'i>,
        start: usize,
        prefix: &str,
        key: &str,
    ) -> Result<&'a Spanned<DeValue<'i>>, String> {
        table
            .get(key)
            .ok_or_else(|| self.error(start, &field_name(prefix, key), "missing required field"))
    }

    fn table<'a, 'i>(
        &self,
        value: &'a Spanned<DeValue<'i>>,
        field: &str,
    ) -> Result<&'a DeTable<'i>, String> {
        value
            .get_ref()
            .as_table()
            .ok_or_else(|| self.error(value.span().start, field, "expected a table"))
    }

    fn unsigned(&self, value: &Spanned<DeValue<'_>>, field: &str, max: u64) -> Result<u64, String> {
        value
            .get_ref()
            .as_integer()
            .and_then(|v| u64::from_str_radix(v.as_str(), v.radix()).ok())
            .filter(|&v| v <= max)
            .ok_or_else(|| {
                self.error(
                    value.span().start,
                    field,
                    &format!("expected an integer from 0 to {max}"),
                )
            })
    }

    fn member(&self, value: &Spanned<DeValue<'_>>, field: &str) -> Result<Member, String> {
        let table = self.table(value, field)?;
        self.fields(table, field, &["id", "tier", "state", "capabilities"])?;
        let get = |key| self.required(table, value.span().start, field, key);
        let id = MemberId(self.unsigned(get("id")?, &field_name(field, "id"), u64::MAX)?);
        let tier_value = get("tier")?;
        let tier = match tier_value.get_ref().as_str() {
            Some("core") => Tier::Core,
            Some("burst") => Tier::Burst,
            _ => {
                return Err(self.error(
                    tier_value.span().start,
                    &field_name(field, "tier"),
                    "expected core or burst",
                ));
            }
        };
        let state_value = get("state")?;
        let state = match state_value.get_ref().as_str() {
            Some("active") => MemberState::Active,
            Some("draining") => MemberState::Draining,
            Some("unreachable") => MemberState::Unreachable,
            _ => {
                return Err(self.error(
                    state_value.span().start,
                    &field_name(field, "state"),
                    "expected active, draining or unreachable",
                ));
            }
        };
        let capabilities =
            self.capabilities(get("capabilities")?, &field_name(field, "capabilities"))?;
        Ok(Member {
            id,
            tier,
            state,
            capabilities,
        })
    }

    fn capabilities(
        &self,
        value: &Spanned<DeValue<'_>>,
        field: &str,
    ) -> Result<Capabilities, String> {
        let table = self.table(value, field)?;
        self.fields(table, field, &["compute", "storage_bytes"])?;
        let storage_bytes = table
            .get("storage_bytes")
            .map(|v| self.unsigned(v, &field_name(field, "storage_bytes"), u64::MAX))
            .transpose()?
            .unwrap_or(0);
        let compute = table
            .get("compute")
            .map(|v| self.compute(v, &field_name(field, "compute")))
            .transpose()?;
        Ok(Capabilities {
            compute,
            storage_bytes,
        })
    }

    fn compute(&self, value: &Spanned<DeValue<'_>>, field: &str) -> Result<ComputeBudget, String> {
        let table = self.table(value, field)?;
        self.fields(
            table,
            field,
            &["cpu_millicores", "memory_bytes", "sandbox_slots"],
        )?;
        let get = |key, max| {
            let v = self.required(table, value.span().start, field, key)?;
            self.unsigned(v, &field_name(field, key), max)
        };
        Ok(ComputeBudget {
            cpu_millicores: get("cpu_millicores", u64::MAX)?,
            memory_bytes: get("memory_bytes", u64::MAX)?,
            sandbox_slots: get("sandbox_slots", u32::MAX.into())? as u32,
        })
    }
}

fn field_name(prefix: &str, key: &str) -> String {
    if prefix.is_empty() {
        key.into()
    } else {
        format!("{prefix}.{key}")
    }
}

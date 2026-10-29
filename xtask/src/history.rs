use std::{
    fs,
    io::Write,
    path::{Path, PathBuf},
    process::{Command, Stdio},
    time::{SystemTime, UNIX_EPOCH},
};

pub struct Snapshot(PathBuf);

impl Snapshot {
    pub fn root(&self) -> &Path {
        &self.0
    }

    pub fn create(root: &Path, revision: &str) -> Result<Self, String> {
        let unique = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_err(|e| e.to_string())?
            .as_nanos();
        let path = std::env::temp_dir().join(format!(
            "homepool-structure-{}-{unique}",
            std::process::id()
        ));
        fs::create_dir(&path).map_err(|e| format!("create structure scratch directory: {e}"))?;
        let snapshot = Self(path);
        let archive = git(root, &["archive", "--format=tar", revision])?;
        let mut child = Command::new("tar")
            .args(["-xf", "-", "-C"])
            .arg(snapshot.root())
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .map_err(|e| format!("extract base snapshot: {e}"))?;
        let write = child
            .stdin
            .take()
            .ok_or("tar stdin unavailable")?
            .write_all(&archive);
        let status = child
            .wait()
            .map_err(|e| format!("extract base snapshot: {e}"))?;
        write.map_err(|e| format!("write base snapshot: {e}"))?;
        if !status.success() {
            return Err("extract base snapshot failed".into());
        }
        Ok(snapshot)
    }
}

impl Drop for Snapshot {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

pub fn git(root: &Path, args: &[&str]) -> Result<Vec<u8>, String> {
    let output = Command::new("git")
        .args(args)
        .current_dir(root)
        .output()
        .map_err(|e| format!("git: {e}"))?;
    if !output.status.success() {
        return Err(format!(
            "git {} failed; fetch main and full history, or pass --base REF",
            args[0]
        ));
    }
    Ok(output.stdout)
}

pub fn base(root: &Path, requested: Option<&str>) -> Result<String, String> {
    let output = if let Some(revision) = requested {
        git(
            root,
            &["rev-parse", "--verify", &format!("{revision}^{{commit}}")],
        )?
    } else {
        // On main, check the last change; on a branch, check the complete PR.
        let merge_base = git(root, &["merge-base", "HEAD", "origin/main"])?;
        let head = git(root, &["rev-parse", "HEAD"])?;
        if merge_base == head
            && Command::new("git")
                .args(["cat-file", "-e", "HEAD:Cargo.toml"])
                .current_dir(root)
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .status()
                .is_ok_and(|s| s.success())
        {
            git(root, &["rev-parse", "HEAD^"])?
        } else {
            merge_base
        }
    };
    String::from_utf8(output)
        .map(|s| s.trim().to_owned())
        .map_err(|e| e.to_string())
}

pub fn issue_prefix(root: &Path) -> Result<String, String> {
    let remote = String::from_utf8(git(root, &["remote", "get-url", "origin"])?)
        .map_err(|e| e.to_string())?;
    let remote = remote.trim().trim_end_matches(".git");
    let https = format!("{}://github.com/", "https");
    let ssh = format!("git{}github.com:", '@');
    let repository = remote
        .strip_prefix(&https)
        .or_else(|| remote.strip_prefix(&ssh))
        .ok_or("origin must identify this repository on GitHub")?;
    if repository.split('/').count() != 2
        || repository.split('/').any(|part| {
            part.is_empty()
                || !part
                    .chars()
                    .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'))
        })
    {
        return Err("invalid repository origin".into());
    }
    Ok(format!("{https}{repository}/issues/"))
}

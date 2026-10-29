use std::{
    collections::BTreeSet,
    fs,
    path::{Component, Path, PathBuf},
    process::Command,
    time::{SystemTime, UNIX_EPOCH},
};

#[derive(Debug, PartialEq, Eq)]
pub enum Base {
    Initial,
    Commit(String),
}

pub struct Snapshot {
    root: PathBuf,
    pub files: Vec<PathBuf>,
}

impl Snapshot {
    pub fn root(&self) -> &Path {
        &self.root
    }

    pub fn empty() -> Result<Self, String> {
        let unique = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_err(|e| e.to_string())?
            .as_nanos();
        let root = std::env::temp_dir().join(format!(
            "homepool-structure-{}-{unique}",
            std::process::id()
        ));
        fs::create_dir(&root).map_err(|e| format!("create structure scratch directory: {e}"))?;
        Ok(Self {
            root,
            files: Vec::new(),
        })
    }

    pub fn create(root: &Path, revision: &str) -> Result<Self, String> {
        let mut snapshot = Self::empty()?;
        // Read Git objects, not an archive or checkout: export and checkout filters
        // must not alter the historical evidence used by the ratchet.
        let tree = git(root, &["ls-tree", "--full-tree", "-r", "-z", revision])?;
        for record in tree.split(|b| *b == 0).filter(|r| !r.is_empty()) {
            let record = std::str::from_utf8(record).map_err(|_| "Git paths must be UTF-8")?;
            let (object, name) = record.split_once('\t').ok_or("invalid Git tree entry")?;
            let fields: Vec<_> = object.split_whitespace().collect();
            if fields.len() != 3
                || fields[0] != "100644" && fields[0] != "100755"
                || fields[1] != "blob"
            {
                return Err(format!("unsupported Git tree entry: {name}"));
            }
            let path = PathBuf::from(name);
            if !path
                .components()
                .all(|part| matches!(part, Component::Normal(_)))
            {
                return Err("invalid path in Git tree".into());
            }
            let destination = snapshot.root.join(&path);
            fs::create_dir_all(destination.parent().ok_or("missing parent directory")?)
                .map_err(|e| e.to_string())?;
            fs::write(destination, git(root, &["cat-file", "blob", fields[2]])?)
                .map_err(|e| e.to_string())?;
            snapshot.files.push(path);
        }
        Ok(snapshot)
    }
}

impl Drop for Snapshot {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.root);
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

pub fn working_files(root: &Path) -> Result<Vec<PathBuf>, String> {
    let files = git(
        root,
        &[
            "ls-files",
            "--cached",
            "--others",
            "--exclude-standard",
            "-z",
        ],
    )?;
    files
        .split(|b| *b == 0)
        .filter(|r| !r.is_empty())
        .map(|r| {
            std::str::from_utf8(r)
                .map(PathBuf::from)
                .map_err(|_| "Git paths must be UTF-8".to_owned())
        })
        .collect::<Result<BTreeSet<_>, _>>()
        .map(|paths| paths.into_iter().collect())
}

pub fn base(root: &Path, requested: Option<&str>) -> Result<Base, String> {
    let revision = |bytes: Vec<u8>| {
        String::from_utf8(bytes)
            .map(|s| s.trim().to_owned())
            .map_err(|e| e.to_string())
    };
    if let Some(requested) = requested {
        if matches!(requested.len(), 40 | 64) && requested.bytes().all(|b| b == b'0') {
            return Ok(Base::Initial);
        }
        return revision(git(
            root,
            &["rev-parse", "--verify", &format!("{requested}^{{commit}}")],
        )?)
        .map(Base::Commit);
    }
    let head = revision(git(root, &["rev-parse", "HEAD"])?)?;
    // Read the actual parent header: graph traversal can make a shallow boundary
    // look like a root commit and must never silently grant an initial state.
    let commit =
        String::from_utf8(git(root, &["cat-file", "commit", &head])?).map_err(|e| e.to_string())?;
    let parent = commit
        .lines()
        .take_while(|line| !line.is_empty())
        .find_map(|line| line.strip_prefix("parent "));
    let dirty = !git(root, &["status", "--porcelain", "--untracked-files=normal"])?.is_empty();
    // A true root commit has no previous tree, including before the first push
    // creates a main tracking ref. Local edits still compare against that commit.
    let Some(parent) = parent else {
        return Ok(if dirty {
            Base::Commit(head)
        } else {
            Base::Initial
        });
    };
    let merge_base = revision(git(root, &["merge-base", "HEAD", "origin/main"])?)?;
    if merge_base != head {
        return Ok(Base::Commit(merge_base));
    }
    // Working-tree and index edits must be compared with the committed main state.
    Ok(Base::Commit(if dirty { head } else { parent.into() }))
}

pub fn repository_url(root: &Path) -> Result<String, String> {
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
    Ok(format!("{https}{repository}"))
}

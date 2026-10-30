use homepool_core::MemberId;

use crate::{
    error::Error,
    model::SandboxSpec,
    settings::{
        CPU_LIMIT_QUANTUM_MILLICORES, ENV_STDIN_PATH, MANAGED_LABEL, MEMBER_LABEL,
        MILLICORES_PER_CPU, SANDBOX_LABEL,
    },
};

pub(crate) fn create_args(member: MemberId, spec: &SandboxSpec) -> Result<Vec<String>, Error> {
    if spec.sandbox_id.is_empty()
        || spec.sandbox_id.len() > 128
        || !spec
            .sandbox_id
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"-_.".contains(&b))
    {
        return Err(Error::InvalidInput("sandbox identity"));
    }
    if spec.image.is_empty() || spec.image.starts_with('-') || spec.image.contains('\0') {
        return Err(Error::InvalidInput("image"));
    }
    if spec.command.iter().any(|arg| arg.contains('\0')) {
        return Err(Error::InvalidInput("command"));
    }
    if !spec
        .cpu_millicores
        .is_multiple_of(CPU_LIMIT_QUANTUM_MILLICORES)
    {
        return Err(Error::CpuLimitNotAligned {
            cpu_millicores: spec.cpu_millicores,
        });
    }
    // Docker represents NanoCPUs and memory with signed 64-bit integers.
    if spec.cpu_millicores == 0
        || spec.cpu_millicores > i64::MAX as u64 / 1_000_000
        || spec.memory_bytes == 0
        || spec.memory_bytes > i64::MAX as u64
    {
        return Err(Error::InvalidInput("resource limits"));
    }
    let cpus = format!(
        "{}.{:03}",
        spec.cpu_millicores / MILLICORES_PER_CPU,
        spec.cpu_millicores % MILLICORES_PER_CPU
    );
    let mut args = vec![
        "container".into(),
        "create".into(),
        "--name".into(),
        container_name(member, &spec.sandbox_id),
        "--label".into(),
        format!("{MANAGED_LABEL}=true"),
        "--label".into(),
        format!("{MEMBER_LABEL}={}", member.0),
        "--label".into(),
        format!("{SANDBOX_LABEL}={}", spec.sandbox_id),
        "--cpus".into(),
        cpus,
        "--memory".into(),
        spec.memory_bytes.to_string(),
        "--env-file".into(),
        ENV_STDIN_PATH.into(),
    ];
    if spec
        .profile
        .memory_swap_bytes
        .is_some_and(|swap| swap < spec.memory_bytes)
    {
        return Err(Error::InvalidInput(
            "profile memory swap below requested memory",
        ));
    }
    spec.profile
        .append_args(&container_name(member, &spec.sandbox_id), &mut args)?;
    for mount in spec.mounts.iter().chain(&spec.profile.binds) {
        for path in [&mount.source, &mount.target] {
            // Reject CSV separators and quotes, rather than changing their meaning
            // in Docker's --mount parser. Paths refer to the engine's machine.
            if !crate::profile::valid_mount_path(path) {
                return Err(Error::InvalidInput("bind mount"));
            }
        }
        let read_only = if mount.read_only { ",readonly" } else { "" };
        args.extend([
            "--mount".into(),
            format!(
                "type=bind,src={},dst={}{read_only}",
                mount.source, mount.target
            ),
        ]);
    }
    args.extend(["--".into(), spec.image.clone()]);
    args.extend(spec.command.iter().cloned());
    Ok(args)
}

// Encode the validated identity without hashing, truncation or case folding.
pub(crate) fn container_name(member: MemberId, sandbox_id: &str) -> String {
    format!("homepool-{}-{sandbox_id}", member.0)
}

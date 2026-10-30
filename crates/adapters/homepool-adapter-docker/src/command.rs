use std::{
    io::Write,
    process::{Command, Stdio},
};

use crate::{endpoint::Endpoint, error::Error, settings::DOCKER_PROGRAM};

pub(crate) fn command(endpoint: &Endpoint, args: &[String]) -> Command {
    let mut command = Command::new(DOCKER_PROGRAM);
    command.arg("--host").arg(endpoint.as_str()).args(args);
    // Explicit endpoints must not inherit context or TLS endpoint overrides.
    for key in [
        "DOCKER_HOST",
        "DOCKER_CONTEXT",
        "DOCKER_TLS",
        "DOCKER_TLS_VERIFY",
        "DOCKER_CERT_PATH",
    ] {
        command.env_remove(key);
    }
    command
}

pub(crate) fn run(
    endpoint: &Endpoint,
    operation: &'static str,
    args: &[String],
    input: &[u8],
) -> Result<Vec<u8>, Error> {
    let mut child = command(endpoint, args)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        // Docker diagnostics can echo environment input. Never retain them.
        .stderr(Stdio::null())
        .spawn()
        .map_err(|_| Error::Io(operation))?;
    let mut stdin = child.stdin.take().expect("piped stdin");
    std::thread::scope(|scope| {
        // Drain output while writing, including for inputs larger than a pipe.
        let writer = scope.spawn(move || stdin.write_all(input));
        let output = child.wait_with_output().map_err(|_| Error::Io(operation));
        let written = writer.join().map_err(|_| Error::Io(operation))?;
        let output = output?;
        if !output.status.success() {
            return Err(Error::Command {
                operation,
                code: output.status.code(),
            });
        }
        written.map_err(|_| Error::Io(operation))?;
        Ok(output.stdout)
    })
}

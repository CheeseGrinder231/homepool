use std::{
    ffi::OsString,
    io::Write,
    process::{Command, Stdio},
};

use serde_json::Value;

use super::error::Error;

pub(super) fn call(
    executable: &OsString,
    prefix: &[OsString],
    operation: &str,
    request: &Value,
) -> Result<Value, Error> {
    let mut child = Command::new(executable)
        .args(prefix)
        .args(["json-api", operation, "-"])
        .env("RUST_LOG", "off")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .map_err(|_| Error("could not start Garage; check installation and private config"))?;
    let input =
        serde_json::to_vec(request).map_err(|_| Error("could not encode Garage request"))?;
    let sent = child.stdin.take().expect("piped stdin").write_all(&input);
    if sent.is_err() {
        let _ = child.kill();
        let _ = child.wait();
        return Err(Error("could not send Garage request"));
    }
    let output = child
        .wait_with_output()
        .map_err(|_| Error("could not wait for Garage"))?;
    if !output.status.success() {
        return Err(Error(
            "Garage operation failed; inspect the installation privately",
        ));
    }
    serde_json::from_slice(&output.stdout).map_err(|_| Error("invalid Garage JSON response"))
}

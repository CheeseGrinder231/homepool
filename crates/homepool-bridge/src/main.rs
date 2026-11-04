mod limits;
mod operations;
mod peer;
mod reconnect;
mod runtime;
mod services;
mod settings;
mod settlement;
mod state;
mod view;
fn execute() -> state::Result<()> {
    let a = std::env::args().skip(1).collect::<Vec<_>>();
    if a.len() == 2 && a[0] == "init" {
        return state::init(std::path::Path::new(&a[1]));
    }
    if a.len() < 2 {
        return Err("usage: homepool-bridge init STATE | run CONFIG | invite CONFIG | devices CONFIG | revoke CONFIG ID".into());
    }
    let c = state::Config::load(std::path::Path::new(&a[1]))?;
    match a[0].as_str() {
        "run" if a.len() == 2 => runtime::run(&c),
        "invite" | "devices" if a.len() == 2 => runtime::command(&c.state_dir, &a[0], None),
        "revoke" if a.len() == 3 => runtime::command(&c.state_dir, "revoke", Some(&a[2])),
        _ => Err("arguments".into()),
    }
}
fn main() {
    if let Err(error) = execute() {
        eprintln!("bridge command failed: {error}");
        std::process::exit(1);
    }
}
#[cfg(test)]
mod service_tests;
#[cfg(test)]
mod tests;

use std::path::Path;

fn main() {
    if let Err(error) = run() {
        eprintln!("{error}");
        std::process::exit(1);
    }
}

fn run() -> Result<(), String> {
    let mut args = std::env::args().skip(1);
    if args.next().as_deref() != Some("structure") {
        return Err("usage: cargo xtask structure [--base REF]".into());
    }
    let base = match args.next().as_deref() {
        None => None,
        Some("--base") => Some(args.next().ok_or("--base requires a revision")?),
        _ => return Err("usage: cargo xtask structure [--base REF]".into()),
    };
    if args.next().is_some() {
        return Err("unexpected argument".into());
    }
    xtask::check(
        Path::new(env!("CARGO_MANIFEST_DIR")).parent().unwrap(),
        base.as_deref(),
    )
}

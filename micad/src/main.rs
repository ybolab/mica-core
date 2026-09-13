//! One binary for the management plane's two daemons: invoked as `micad` it is
//! the settings and reconciliation daemon, invoked as `apid` (a link to this
//! file) the HTTPS API daemon. The name is the only selector; any other name
//! refuses.

#![forbid(unsafe_code)]

fn main() -> anyhow::Result<()> {
    let name = std::env::args_os().next();
    match name
        .as_deref()
        .and_then(|name| std::path::Path::new(name).file_name())
        .and_then(|name| name.to_str())
    {
        Some("micad") => micad::main(),
        Some("apid") => apid::main(),
        _ => anyhow::bail!("this binary must be invoked as micad or apid"),
    }
}

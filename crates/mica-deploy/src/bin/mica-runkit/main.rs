//! One static lifecycle executable for the signed kernel image, reached
//! through links: `init` is the authenticated initramfs PID 1 and `shutdown`
//! (or `mica-shutdown`) the memory-only retained exit-ramdisk PID 1. The link
//! name is the only selector; any other name refuses.
#![forbid(unsafe_code)]

mod init;
mod shutdown;

use std::path::Path;

fn main() {
    let name = std::env::args_os().next();
    match name
        .as_deref()
        .and_then(|name| Path::new(name).file_name())
        .and_then(|name| name.to_str())
    {
        Some("init") => init::main(),
        Some("shutdown" | "mica-shutdown") => shutdown::main(),
        _ => {
            eprintln!("mica-runkit must be invoked as init or shutdown");
            std::process::exit(2);
        }
    }
}

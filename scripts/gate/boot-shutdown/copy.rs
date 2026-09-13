//! Fixture-only CLI: exercise the library copier against an assembled payload.
#![forbid(unsafe_code)]
use std::{env, fs, path::Path};
fn main() -> Result<(), Box<dyn std::error::Error>> {
    let args: Vec<_> = env::args().skip(1).collect();
    assert_eq!(args.len(), 3);
    let source = Path::new(&args[0]);
    let manifest = fs::read_to_string(&args[2])?;
    let capacity = mica_deploy::boot::exitrd_tmpfs_bytes(source, &manifest)?;
    mica_deploy::boot::copy_exitrd(source, Path::new(&args[1]), &manifest)?;
    println!("EXITRD_COPY_PASS capacityBytes={capacity}");
    Ok(())
}

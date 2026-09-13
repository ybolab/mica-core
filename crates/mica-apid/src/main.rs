//! apid on its own, for this crate's tests and local runs. Images ship apid as
//! the `apid` link to the `micad` binary.

#![forbid(unsafe_code)]

fn main() -> anyhow::Result<()> {
    mica_apid::main()
}

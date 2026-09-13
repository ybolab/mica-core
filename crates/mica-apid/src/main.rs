//! `mica-apid`, the HTTPS API daemon: its own executable and its own package,
//! so an apid upgrade never repacks micad.

#![forbid(unsafe_code)]

fn main() -> anyhow::Result<()> {
    mica_apid::main()
}

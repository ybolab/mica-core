//! `micad`, the settings and reconciliation daemon. The API daemon is its own
//! executable, `mica-apid`, so an apid upgrade never repacks micad.

#![forbid(unsafe_code)]

fn main() -> anyhow::Result<()> {
    mica_core::main()
}

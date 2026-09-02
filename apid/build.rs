use std::env;
use std::error::Error;
use std::ffi::OsStr;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};

fn main() -> Result<(), Box<dyn Error>> {
    let manifest_dir = PathBuf::from(
        env::var_os("CARGO_MANIFEST_DIR")
            .ok_or_else(|| io::Error::other("CARGO_MANIFEST_DIR is not set"))?,
    );
    let dist = manifest_dir.join("ui/dist");
    println!("cargo:rerun-if-changed={}", dist.display());

    let mut assets = Vec::new();
    collect_assets(&dist, &dist, &mut assets)?;
    assets.sort();
    if !assets.iter().any(|path| path == "index.html") {
        return Err(io::Error::other("ui/dist must contain index.html").into());
    }
    if assets.windows(2).any(|pair| pair[0] == pair[1]) {
        return Err(io::Error::other("ui/dist contains duplicate logical asset paths").into());
    }

    let mut generated = String::from("static BUILTIN_ASSETS: &[EmbeddedAsset] = &[\n");
    for logical in &assets {
        let source_suffix = format!("/ui/dist/{logical}");
        generated.push_str(&format!(
            "    EmbeddedAsset {{ path: {logical:?}, bytes: include_bytes!(concat!(env!(\"CARGO_MANIFEST_DIR\"), {source_suffix:?})) }},\n"
        ));
        println!("cargo:rerun-if-changed={}", dist.join(logical).display());
    }
    generated.push_str("];\n");

    let out_dir = PathBuf::from(
        env::var_os("OUT_DIR").ok_or_else(|| io::Error::other("OUT_DIR is not set"))?,
    );
    fs::write(out_dir.join("builtin_assets.rs"), generated)?;
    Ok(())
}

fn collect_assets(root: &Path, directory: &Path, assets: &mut Vec<String>) -> io::Result<()> {
    let mut entries = fs::read_dir(directory)?.collect::<Result<Vec<_>, _>>()?;
    entries.sort_by_key(fs::DirEntry::file_name);

    for entry in entries {
        let path = entry.path();
        let metadata = fs::symlink_metadata(&path)?;
        let file_type = metadata.file_type();
        if file_type.is_symlink() {
            return Err(invalid_asset(&path, "symlinks are not allowed"));
        }
        if file_type.is_dir() {
            collect_assets(root, &path, assets)?;
            continue;
        }
        if !file_type.is_file() {
            return Err(invalid_asset(&path, "only regular files are allowed"));
        }

        let relative = path
            .strip_prefix(root)
            .map_err(|_| invalid_asset(&path, "asset escaped ui/dist"))?;
        let logical = logical_name(relative)
            .ok_or_else(|| invalid_asset(&path, "asset name is not a safe UTF-8 URL path"))?;
        assets.push(logical);
    }
    Ok(())
}

fn logical_name(relative: &Path) -> Option<String> {
    let segments = relative
        .iter()
        .map(OsStr::to_str)
        .collect::<Option<Vec<_>>>()?;
    if segments.is_empty()
        || segments.iter().any(|segment| {
            segment.is_empty()
                || matches!(*segment, "." | "..")
                || segment.contains(['%', '\\', '/'])
                || segment.bytes().any(|byte| byte.is_ascii_control())
        })
    {
        return None;
    }
    Some(segments.join("/"))
}

fn invalid_asset(path: &Path, reason: &str) -> io::Error {
    io::Error::other(format!(
        "invalid built-in UI asset {}: {reason}",
        path.display()
    ))
}

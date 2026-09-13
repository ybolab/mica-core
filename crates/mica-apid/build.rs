use std::env;
use std::error::Error;
use std::ffi::OsStr;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};

const UI_DIST_ENV: &str = "MICA_APID_UI_DIST_DIR";

fn main() -> Result<(), Box<dyn Error>> {
    println!("cargo:rerun-if-env-changed={UI_DIST_ENV}");
    let dist = PathBuf::from(env::var_os(UI_DIST_ENV).ok_or_else(|| {
        io::Error::other(format!(
            "{UI_DIST_ENV} is not set; build the UI with crates/mica-apid/ui/build.sh before compiling apid"
        ))
    })?);
    if !dist.is_absolute() {
        return Err(io::Error::other(format!("{UI_DIST_ENV} must be an absolute path")).into());
    }
    let dist_metadata = fs::symlink_metadata(&dist).map_err(|error| {
        io::Error::other(format!(
            "cannot inspect generated built-in UI directory {}: {error}",
            dist.display()
        ))
    })?;
    if dist_metadata.file_type().is_symlink() || !dist_metadata.is_dir() {
        return Err(io::Error::other(format!(
            "generated built-in UI path {} must be a real directory",
            dist.display()
        ))
        .into());
    }
    println!("cargo:rerun-if-changed={}", dist.display());

    let mut assets = Vec::new();
    collect_assets(&dist, &dist, &mut assets)?;
    assets.sort();
    if !assets.iter().any(|path| path == "index.html") {
        return Err(io::Error::other("generated built-in UI must contain index.html").into());
    }
    if assets.windows(2).any(|pair| pair[0] == pair[1]) {
        return Err(io::Error::other(
            "generated built-in UI contains duplicate logical asset paths",
        )
        .into());
    }

    let out_dir = PathBuf::from(
        env::var_os("OUT_DIR").ok_or_else(|| io::Error::other("OUT_DIR is not set"))?,
    );
    let staged = out_dir.join("builtin-ui");
    if staged.try_exists()? {
        fs::remove_dir_all(&staged)?;
    }

    let mut generated = String::from("static BUILTIN_ASSETS: &[EmbeddedAsset] = &[\n");
    for logical in &assets {
        let source = dist.join(logical);
        let destination = staged.join(logical);
        let parent = destination
            .parent()
            .ok_or_else(|| invalid_asset(&destination, "asset has no output parent"))?;
        fs::create_dir_all(parent)?;
        fs::write(&destination, fs::read(&source)?)?;

        let source_suffix = format!("/builtin-ui/{logical}");
        generated.push_str(&format!(
            "    EmbeddedAsset {{ path: {logical:?}, bytes: include_bytes!(concat!(env!(\"OUT_DIR\"), {source_suffix:?})) }},\n"
        ));
        println!("cargo:rerun-if-changed={}", source.display());
    }
    generated.push_str("];\n");

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
            .map_err(|_| invalid_asset(&path, "asset escaped the generated UI root"))?;
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

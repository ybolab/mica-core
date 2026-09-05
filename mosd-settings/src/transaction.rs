//! Undo journal for the single settings writer across DATA and STATE.

use std::fs::{self, File};
use std::io;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::documents::CONFIG_DOCUMENTS;
use crate::store::write_atomically;

const STATE_DOCUMENT: &str = "settings.toml";

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Undo {
    document: String,
    previous: Option<String>,
}

fn target(state: &Path, config: &Path, document: &str) -> PathBuf {
    if document == STATE_DOCUMENT {
        state.to_path_buf()
    } else {
        config.join(document)
    }
}

fn journal(state: &Path) -> PathBuf {
    state.with_extension("transaction.json")
}

fn read_optional(path: &Path) -> io::Result<Option<String>> {
    match fs::read_to_string(path) {
        Ok(text) => Ok(Some(text)),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error),
    }
}

fn remove_synced(path: &Path) -> io::Result<()> {
    let parent = path
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or(Path::new("."));
    match fs::remove_file(path) {
        Ok(()) => File::open(parent)?.sync_all(),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error),
    }
}

/// Restore an interrupted save before any reader or writer uses its documents.
pub(crate) fn recover(state: &Path, config: &Path) -> io::Result<()> {
    let Some(text) = read_optional(&journal(state))? else {
        return Ok(());
    };
    let undo: Vec<Undo> = serde_json::from_str(&text).map_err(|_| {
        io::Error::new(
            io::ErrorKind::InvalidData,
            "invalid settings transaction journal",
        )
    })?;
    // The journal carries credentials, so parser errors never include its bytes.
    // Only the store's fixed document names may direct recovery writes.
    if undo.len() > CONFIG_DOCUMENTS.len() + 1
        || undo.iter().enumerate().any(|(index, entry)| {
            (entry.document != STATE_DOCUMENT
                && !CONFIG_DOCUMENTS.contains(&entry.document.as_str()))
                || undo[..index]
                    .iter()
                    .any(|old| old.document == entry.document)
        })
    {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "invalid settings transaction documents",
        ));
    }
    for entry in undo.iter().rev() {
        let path = target(state, config, &entry.document);
        match &entry.previous {
            Some(previous) if read_optional(&path)?.as_ref() != Some(previous) => {
                write_atomically(&path, previous)?
            }
            Some(_) => {}
            None => remove_synced(&path)?,
        }
    }
    // Until this removal is durable, repeating the rollback is harmless.
    remove_synced(&journal(state))
}

/// Commit changed documents, rolling back any failure before journal removal.
pub(crate) fn save(state: &Path, config: &Path, documents: &[(String, String)]) -> io::Result<()> {
    save_with(state, config, documents, write_atomically)
}

fn save_with(
    state: &Path,
    config: &Path,
    documents: &[(String, String)],
    write: impl Fn(&Path, &str) -> io::Result<()>,
) -> io::Result<()> {
    recover(state, config)?;
    let parent = state
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or(Path::new("."));
    fs::create_dir_all(parent)?;
    let mut changed = Vec::new();
    let mut undo = Vec::new();
    for (document, text) in documents {
        let path = target(state, config, document);
        let previous = read_optional(&path)?;
        if previous.as_ref() != Some(text) {
            changed.push((path, text));
            undo.push(Undo {
                document: document.clone(),
                previous,
            });
        }
    }
    if changed.is_empty() {
        return Ok(());
    }
    let record = serde_json::to_string(&undo).map_err(io::Error::other)?;
    let marker = journal(state);
    write_atomically(&marker, &record)?;
    let result = (|| {
        for (path, text) in changed {
            write(&path, text)?;
        }
        remove_synced(&marker)
    })();
    if let Err(error) = result {
        // A failed directory fsync may follow a successful unlink. Restore the
        // journal before rolling back so another interruption remains recoverable.
        if !marker.exists() {
            write_atomically(&marker, &record)?;
        }
        recover(state, config).map_err(|rollback| {
            io::Error::other(format!(
                "settings commit failed ({error}); rollback is pending ({rollback})"
            ))
        })?;
        return Err(error);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{Settings, Store};
    use std::os::unix::fs::PermissionsExt;

    #[test]
    fn every_write_failure_restores_the_previous_files_even_after_a_rename() {
        for failure in 0..3 {
            for after_rename in [false, true] {
                let dir = tempfile::tempdir().unwrap();
                let state = dir.path().join(STATE_DOCUMENT);
                let documents: Vec<_> = ["system.json", "time.json", STATE_DOCUMENT]
                    .into_iter()
                    .map(|name| (name.to_string(), "new".to_string()))
                    .collect();
                fs::write(&state, "old state").unwrap();
                fs::write(dir.path().join("system.json"), "old config").unwrap();
                let count = std::cell::Cell::new(0);
                let error = save_with(&state, dir.path(), &documents, |path, text| {
                    let index = count.get();
                    count.set(index + 1);
                    if index != failure || after_rename {
                        write_atomically(path, text)?;
                    }
                    if index == failure {
                        Err(io::Error::other("injected I/O failure"))
                    } else {
                        Ok(())
                    }
                })
                .unwrap_err();
                assert!(error.to_string().contains("injected I/O failure"));
                assert_eq!(fs::read_to_string(&state).unwrap(), "old state");
                assert_eq!(
                    fs::read_to_string(dir.path().join("system.json")).unwrap(),
                    "old config"
                );
                assert!(!dir.path().join("time.json").exists());
                assert!(!journal(&state).exists());
            }
        }
    }

    #[test]
    fn an_interrupted_save_is_rolled_back_before_loading() {
        let dir = tempfile::tempdir().unwrap();
        let config = dir.path().join("config");
        fs::create_dir(&config).unwrap();
        let state = dir.path().join(STATE_DOCUMENT);
        let store = Store::new(&state, &config);
        let previous = Settings::default();
        store.save(&previous).unwrap();
        let original = fs::read_to_string(config.join("system.json")).unwrap();
        let undo = vec![
            Undo {
                document: "system.json".into(),
                previous: Some(original.clone()),
            },
            Undo {
                document: "time.json".into(),
                previous: None,
            },
        ];
        write_atomically(&journal(&state), &serde_json::to_string(&undo).unwrap()).unwrap();
        assert_eq!(
            fs::metadata(journal(&state)).unwrap().permissions().mode() & 0o777,
            0o600
        );
        fs::write(config.join("system.json"), "interrupted replacement").unwrap();
        fs::write(config.join("time.json"), "new file before interruption").unwrap();
        assert_eq!(store.load().unwrap(), previous);
        assert_eq!(
            fs::read_to_string(config.join("system.json")).unwrap(),
            original
        );
        assert!(!config.join("time.json").exists());
        assert!(!journal(&state).exists());
        assert_eq!(store.load().unwrap(), previous);
    }

    #[test]
    fn blocked_recovery_keeps_the_journal_and_refuses_a_new_save() {
        let dir = tempfile::tempdir().unwrap();
        let config = dir.path().join("config");
        fs::create_dir(&config).unwrap();
        let state = dir.path().join(STATE_DOCUMENT);
        let undo = vec![Undo {
            document: "system.json".into(),
            previous: Some("old".into()),
        }];
        write_atomically(&journal(&state), &serde_json::to_string(&undo).unwrap()).unwrap();
        fs::create_dir(config.join("system.json")).unwrap();
        assert!(save(&state, &config, &[("system.json".into(), "new".into())]).is_err());
        assert!(journal(&state).exists());
        fs::remove_dir(config.join("system.json")).unwrap();
        recover(&state, &config).unwrap();
        assert_eq!(
            fs::read_to_string(config.join("system.json")).unwrap(),
            "old"
        );
    }

    #[test]
    fn invalid_journals_cannot_redirect_writes_or_disclose_their_contents() {
        let dir = tempfile::tempdir().unwrap();
        let state = dir.path().join(STATE_DOCUMENT);
        for record in [
            "secret-invalid-json".to_string(),
            r#"[{"document":"../outside","previous":"secret"}]"#.to_string(),
        ] {
            write_atomically(&journal(&state), &record).unwrap();
            let error = recover(&state, dir.path()).unwrap_err();
            assert!(!error.to_string().contains("secret"));
            assert!(journal(&state).exists());
        }
    }
}

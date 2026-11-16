//! A separate owner-only ledger: no changes to vocabulary or settings.
//! An advisory file lock serializes the observer and CLI. Atomic rename
//! prevents readers seeing a partial write. Only short correction pairs persist.
use std::fs::{self, OpenOptions};
use std::io::{Read, Write};
use std::path::Path;

use super::Correction;
use color_eyre::{Result, eyre::ensure};

#[derive(clap::Subcommand, Debug)]
pub(crate) enum Command {
    /// Print locally learned contextual corrections as JSON.
    List,
    /// Forget a correction by its id (also suppresses relearning this pair).
    Forget { id: u64 },
}

#[derive(Default, serde::Deserialize, serde::Serialize)]
struct Ledger {
    #[serde(default)]
    next_id: u64,
    #[serde(default)]
    entries: Vec<Correction>,
    #[serde(default)]
    ignored: Vec<(String, String, String)>,
}

fn read(path: &Path) -> Result<Ledger> {
    match std::fs::File::open(path) {
        Ok(file) => {
            let mut bytes = Vec::new();
            file.take(1024 * 1024 + 1).read_to_end(&mut bytes)?;
            ensure!(
                bytes.len() <= 1024 * 1024,
                "correction ledger exceeds 1 MiB"
            );
            Ok(serde_json::from_slice(&bytes)?)
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Ledger::default()),
        Err(e) => Err(e.into()),
    }
}

fn edit<T>(root: &Path, change: impl FnOnce(&mut Ledger) -> Result<T>) -> Result<T> {
    fs::create_dir_all(root)?;
    let mut options = OpenOptions::new();
    options.create(true).read(true).write(true).truncate(false);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let lock = options.open(root.join("corrections.lock"))?;
    fs2::FileExt::lock_exclusive(&lock)?;
    let path = root.join("corrections.json");
    let mut ledger = read(&path)?;
    let result = change(&mut ledger)?;
    let temporary = root.join("corrections.json.tmp");
    let mut file_options = OpenOptions::new();
    file_options.create(true).write(true).truncate(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        file_options.mode(0o600);
    }
    let mut file = file_options.open(&temporary)?;
    file.write_all(&serde_json::to_vec_pretty(&ledger)?)?;
    file.sync_all()?;
    fs::rename(temporary, path)?;
    Ok(result)
}

pub(super) fn list() -> Result<Vec<Correction>> {
    Ok(read(&crate::app_paths::support_dir()?.join("corrections.json"))?.entries)
}

#[cfg(target_os = "linux")]
pub(super) fn record(application: &str, heard: String, intended: String) -> Result<Option<u64>> {
    edit(&crate::app_paths::support_dir()?, |ledger| {
        add(ledger, application, heard, intended)
    })
}

#[cfg(any(target_os = "linux", test))]
fn add(
    ledger: &mut Ledger,
    application: &str,
    heard: String,
    intended: String,
) -> Result<Option<u64>> {
    if ledger
        .ignored
        .contains(&(application.to_owned(), heard.clone(), intended.clone()))
    {
        return Ok(None);
    }
    if let Some(existing) = ledger
        .entries
        .iter_mut()
        .find(|c| c.application == application && c.heard == heard && c.intended == intended)
    {
        existing.observations = existing.observations.saturating_add(1);
        return Ok(None);
    }
    ledger.next_id = ledger.next_id.saturating_add(1);
    let id = ledger.next_id;
    ledger.entries.push(Correction {
        id,
        application: application.to_owned(),
        heard,
        intended,
        observations: 1,
    });
    if ledger.entries.len() > 500 {
        ledger.entries.remove(0);
    }
    Ok(Some(id))
}

fn forget_in(ledger: &mut Ledger, id: u64) -> Result<()> {
    let position = ledger.entries.iter().position(|c| c.id == id);
    ensure!(position.is_some(), "no correction with id {id}");
    let entry = ledger.entries.remove(position.unwrap());
    ledger
        .ignored
        .push((entry.application, entry.heard, entry.intended));
    if ledger.ignored.len() > 500 {
        ledger.ignored.remove(0);
    }
    Ok(())
}

pub(crate) fn forget(id: u64) -> Result<()> {
    edit(&crate::app_paths::support_dir()?, |ledger| {
        forget_in(ledger, id)
    })
}

pub(crate) fn run(command: Command) -> u8 {
    let result = match command {
        Command::List => list().and_then(|entries| {
            println!("{}", serde_json::to_string_pretty(&entries)?);
            Ok(())
        }),
        Command::Forget { id } => forget(id),
    };
    match result {
        Ok(()) => 0,
        Err(error) => {
            eprintln!("{error:#}");
            1
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn concurrent_writers_merge_and_corrupt_ledgers_are_preserved() {
        let root = std::env::temp_dir().join(format!(
            "hex-corrections-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::thread::scope(|scope| {
            for _ in 0..4 {
                let root = &root;
                scope.spawn(move || {
                    for _ in 0..10 {
                        edit(root, |ledger| {
                            add(ledger, "Editor", "cloud".into(), "Claude".into())
                        })
                        .unwrap();
                    }
                });
            }
        });
        let path = root.join("corrections.json");
        let ledger = read(&path).unwrap();
        assert_eq!(ledger.entries.len(), 1);
        assert_eq!(ledger.entries[0].observations, 40);
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(
                fs::metadata(&path).unwrap().permissions().mode() & 0o777,
                0o600
            );
        }
        fs::write(&path, b"invalid").unwrap();
        assert!(
            edit(&root, |ledger| add(
                ledger,
                "Editor",
                "Virtel".into(),
                "Vercel".into()
            ))
            .is_err()
        );
        assert_eq!(fs::read(&path).unwrap(), b"invalid");
        fs::remove_dir_all(root).unwrap();
    }
    #[test]
    fn forgetting_suppresses_the_pair_without_affecting_others() {
        let mut ledger = Ledger::default();
        let id = add(&mut ledger, "Editor", "cloud".into(), "Claude".into())
            .unwrap()
            .unwrap();
        assert!(
            add(&mut ledger, "Editor", "cloud".into(), "Claude".into())
                .unwrap()
                .is_none()
        );
        assert_eq!(ledger.entries[0].observations, 2);
        forget_in(&mut ledger, id).unwrap();
        assert!(
            add(&mut ledger, "Editor", "cloud".into(), "Claude".into())
                .unwrap()
                .is_none()
        );
        assert!(
            add(&mut ledger, "Other", "cloud".into(), "Claude".into())
                .unwrap()
                .is_some()
        );
    }
}

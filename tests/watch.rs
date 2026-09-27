//! Recarga en caliente contra el sistema de archivos real. Cada test usa su propio directorio
//! bajo `CARGO_TARGET_TMPDIR`, así que corren en paralelo sin pisarse.
#![cfg(feature = "watch")]

use std::{error::Error, fs, io, path::Path, path::PathBuf, sync::Arc, sync::mpsc, time::Duration};

use breaker_panel::{Diff, Flags, LoadError, Watcher};

// Los helpers propagan con `?`: `allow-unwrap-in-tests` solo cubre las funciones `#[test]`.
type TestResult = Result<(), Box<dyn Error>>;

const ON: &str = "[flags]\n\"payments\" = { enabled = true }\n";
const OFF: &str = "[flags]\n\"payments\" = { enabled = false, reason = \"mantenimiento\" }\n";

fn flags_file(test: &str) -> io::Result<PathBuf> {
    let dir = PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join(test);
    let _ = fs::remove_dir_all(&dir);
    fs::create_dir_all(&dir)?;
    let path = dir.join("flags.toml");
    fs::write(&path, ON)?;
    Ok(path)
}

fn diffs(flags: &Flags) -> mpsc::Receiver<Diff> {
    let (tx, rx) = mpsc::channel();
    flags.on_change(move |diff| {
        let _ = tx.send(diff.clone());
    });
    rx
}

/// Como guardan los editores: escribir un temporal y renombrarlo encima.
fn atomic_save_reloads(path: &Path, (flags, _watcher): (Arc<Flags>, Watcher)) -> TestResult {
    let rx = diffs(&flags);
    let tmp = path.with_extension("tmp");
    fs::write(&tmp, OFF)?;
    fs::rename(&tmp, path)?;

    // Plazo generoso para el evento, no una espera: el test sigue en cuanto llega.
    let diff = rx.recv_timeout(Duration::from_secs(10))?;
    assert_eq!(diff.changed, ["payments"]);
    assert!(flags.require("payments").is_err());
    Ok(())
}

#[test]
fn guardado_atomico_dispara_el_reload() -> TestResult {
    let path = flags_file("guardado_atomico")?;
    atomic_save_reloads(&path, Flags::watch_file(&path)?)
}

#[test]
fn guardado_atomico_dispara_el_reload_con_polling() -> TestResult {
    let path = flags_file("guardado_atomico_polling")?;
    let interval = Duration::from_millis(50);
    atomic_save_reloads(&path, Flags::poll_file(&path, interval)?)
}

#[test]
fn reload_invalido_conserva_el_snapshot_y_no_avisa() -> TestResult {
    let path = flags_file("reload_invalido")?;
    let (flags, watcher) = Flags::<()>::watch_file(&path)?;
    let rx = diffs(&flags);

    fs::write(&path, "[flags]\n\"payments\" = { enabled = false }\n")?;
    let r = watcher.reload();

    assert!(matches!(r, Err(LoadError::MissingReason { .. })), "{r:?}");
    assert_eq!(flags.require("payments"), Ok(()));
    assert_eq!(flags.snapshot().revision(), 0);
    assert!(rx.try_recv().is_err());
    Ok(())
}

#[test]
fn arrancar_sin_archivo_falla() -> TestResult {
    let path = flags_file("sin_archivo")?.with_file_name("no_existe.toml");
    let r = Flags::<()>::watch_file(&path);
    assert!(matches!(r, Err(LoadError::Io { .. })), "{r:?}");
    Ok(())
}

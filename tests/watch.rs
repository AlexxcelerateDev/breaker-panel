//! Recarga en caliente contra el sistema de archivos real. Cada test usa su propio directorio
//! bajo `CARGO_TARGET_TMPDIR`, así que corren en paralelo sin pisarse.
#![cfg(feature = "watch")]

use std::{
    error::Error,
    fs::{self, File, FileTimes},
    io::{self, Write},
    path::{Path, PathBuf},
    sync::{Arc, mpsc},
    thread,
    time::{Duration, Instant},
};

use breaker_panel::{Diff, Flags, LoadError, Watcher};

// Los helpers propagan con `?`: `allow-unwrap-in-tests` solo cubre las funciones `#[test]`.
type TestResult = Result<(), Box<dyn Error>>;

const ON: &str = "[flags]\n\"payments\" = { enabled = true }\n";
const OFF: &str = "[flags]\n\"payments\" = { enabled = false, reason = \"mantenimiento\" }\n";
const SIN_REASON: &str = "[flags]\n\"payments\" = { enabled = false }\n";

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

fn rejects(watcher: &Watcher) -> mpsc::Receiver<String> {
    let (tx, rx) = mpsc::channel();
    watcher.on_reject(move |e| {
        let _ = tx.send(format!("{e:#}"));
    });
    rx
}

/// Como guardan los editores: escribir un temporal y renombrarlo encima.
///
/// Con el mtime del archivo anterior, que es el peor caso para `poll_file`: `notify` compara el
/// mtime con resolución de segundos, así que es lo que ve ante un cambio en el mismo segundo, y
/// solo mirando el contenido se detecta. Sin fijarlo, el test pasaría por suerte cuando el
/// guardado cae en el segundo siguiente. A los tests con eventos les da igual.
fn atomic_save(path: &Path, text: &str) -> io::Result<()> {
    let mtime = fs::metadata(path)?.modified()?;
    let tmp = path.with_extension("tmp");
    // `File::set_times` y no `fs::set_times`, que es de 1.99: los tests también compilan en la
    // MSRV (1.98).
    let mut file = File::create(&tmp)?;
    file.write_all(text.as_bytes())?;
    file.set_times(FileTimes::new().set_modified(mtime))?;
    drop(file);
    fs::rename(&tmp, path)
}

/// Plazo generoso para el evento, no una espera: el test sigue en cuanto llega.
fn next<T>(rx: &mpsc::Receiver<T>) -> Result<T, mpsc::RecvTimeoutError> {
    rx.recv_timeout(Duration::from_secs(10))
}

fn atomic_save_reloads(path: &Path, (flags, _watcher): (Arc<Flags>, Watcher)) -> TestResult {
    let rx = diffs(&flags);
    atomic_save(path, OFF)?;

    assert_eq!(next(&rx)?.changed, ["payments"]);
    assert!(flags.require("payments").is_err());
    assert_eq!(flags.snapshot().toml(), OFF);
    Ok(())
}

#[test]
fn watcher_y_flags_caben_en_el_estado_de_axum() {
    // `State` exige `Send + Sync`: un endpoint admin con `reload()` necesita el `Watcher` ahí.
    fn send_sync<T: Send + Sync>() {}
    send_sync::<Watcher>();
    send_sync::<Arc<Flags>>();
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
fn un_callback_que_entra_en_panico_no_para_la_recarga() -> TestResult {
    let path = flags_file("callback_en_panico")?;
    let (flags, _watcher) = Flags::<()>::watch_file(&path)?;
    flags.on_change(|_| panic!("callback roto"));
    let rx = diffs(&flags);

    // El callback de después se entera igual...
    atomic_save(&path, OFF)?;
    assert_eq!(next(&rx)?.changed, ["payments"]);
    // ...y el hilo del watcher sigue vivo para la recarga siguiente.
    atomic_save(&path, ON)?;
    assert_eq!(next(&rx)?.changed, ["payments"]);
    Ok(())
}

#[test]
fn soltar_el_watcher_deja_de_vigilar() -> TestResult {
    let path = flags_file("soltar_watcher")?;
    let (flags, watcher) = Flags::<()>::watch_file(&path)?;

    drop(watcher);

    // El hilo del debouncer es lo único que guarda otra referencia a los flags: cuando la
    // suelta, ha parado y nadie puede reemplazarlos. Plazo para que pare, no una espera fija.
    let plazo = Instant::now() + Duration::from_secs(10);
    while Arc::strong_count(&flags) > 1 {
        assert!(
            Instant::now() < plazo,
            "el watcher sigue vivo tras soltarlo"
        );
        thread::yield_now();
    }
    Ok(())
}

#[test]
fn una_relectura_con_el_mismo_contenido_no_aplica_nada() -> TestResult {
    let path = flags_file("relectura_sin_cambios")?;
    // Al arrancar se relee una vez tras empezar a vigilar; es el mismo camino que siguen los
    // eventos de otros archivos del directorio. Con el mismo contenido, no hay revisión nueva.
    let (flags, _watcher) = Flags::<()>::watch_file(&path)?;
    assert_eq!(flags.snapshot().revision(), 0);
    Ok(())
}

#[test]
fn borrar_el_archivo_se_rechaza_y_recrearlo_lo_recupera() -> TestResult {
    let path = flags_file("borrar_y_recrear")?;
    let (flags, watcher) = Flags::<()>::watch_file(&path)?;
    let (rejected, rx) = (rejects(&watcher), diffs(&flags));

    fs::remove_file(&path)?;
    let error = next(&rejected)?;
    assert!(error.starts_with("no se pudo leer"), "{error}");
    assert_eq!(flags.require("payments"), Ok(()));
    // Mientras siga sin existir, ni otra recarga (aquí forzada) ni cualquier otro evento del
    // directorio vuelven a avisar: el mismo rechazo se avisa una vez.
    assert!(watcher.reload().is_err());
    assert!(rejected.try_recv().is_err());

    fs::write(&path, OFF)?;
    assert_eq!(next(&rx)?.changed, ["payments"]);
    Ok(())
}

#[test]
fn un_on_reject_que_recarga_no_entra_en_bucle() -> TestResult {
    let path = flags_file("on_reject_recarga")?;
    let (_flags, watcher) = Flags::<()>::watch_file(&path)?;
    let watcher = Arc::new(watcher);
    let (tx, rx) = mpsc::channel();
    let w = Arc::downgrade(&watcher);
    // "Reintentar al rechazar": con el archivo aún roto, vuelve a fallar.
    watcher.on_reject(move |_| {
        let _ = tx.send(());
        if let Some(w) = w.upgrade() {
            let _ = w.reload();
        }
    });

    fs::write(&path, SIN_REASON)?;
    let r = watcher.reload();

    // Sin deduplicar los avisos, la recursión desbordaría la pila antes de llegar aquí.
    assert!(r.is_err());
    next(&rx)?;
    assert!(rx.try_recv().is_err());
    Ok(())
}

#[test]
fn un_on_change_que_recarga_no_deja_muerto_el_watcher() -> TestResult {
    let path = flags_file("on_change_recarga")?;
    let (flags, watcher) = Flags::<()>::watch_file(&path)?;
    let watcher = Arc::new(watcher);
    let w = Arc::downgrade(&watcher);
    // Se esperaría a sí mismo: tiene que fallar alto, no bloquear el hilo del watcher.
    flags.on_change(move |_| {
        if let Some(w) = w.upgrade() {
            let _ = w.reload();
        }
    });
    let rx = diffs(&flags);

    atomic_save(&path, OFF)?;
    assert_eq!(next(&rx)?.changed, ["payments"]);
    atomic_save(&path, ON)?;
    assert_eq!(next(&rx)?.changed, ["payments"]);
    Ok(())
}

#[test]
fn reload_invalido_conserva_el_snapshot_y_no_avisa() -> TestResult {
    let path = flags_file("reload_invalido")?;
    let (flags, watcher) = Flags::<()>::watch_file(&path)?;
    let (rejected, rx) = (rejects(&watcher), diffs(&flags));

    fs::write(&path, SIN_REASON)?;
    let r = watcher.reload();

    assert!(matches!(r, Err(LoadError::MissingReason { .. })), "{r:?}");
    // El rechazo es observable sin depender del log de la librería.
    assert!(next(&rejected)?.contains("sin `reason`"));
    assert_eq!(flags.require("payments"), Ok(()));
    assert_eq!(flags.snapshot().revision(), 0);
    assert!(rx.try_recv().is_err());
    // Lo que vería un health check: el disco dice una cosa y lo aplicado, otra.
    assert_ne!(fs::read_to_string(&path)?, flags.snapshot().toml());
    assert_eq!(flags.snapshot().toml(), ON);
    Ok(())
}

#[test]
fn arrancar_sin_archivo_falla() -> TestResult {
    let path = flags_file("sin_archivo")?.with_file_name("no_existe.toml");
    let r = Flags::<()>::watch_file(&path);
    assert!(matches!(r, Err(LoadError::Io { .. })), "{r:?}");
    Ok(())
}

use std::{
    error::Error,
    fmt, fs, io,
    path::{Path, PathBuf},
    sync::{Arc, Mutex, PoisonError},
    time::Duration,
};

use notify_debouncer_full::{
    DebounceEventResult, Debouncer, RecommendedCache, new_debouncer_opt,
    notify::{self, PollWatcher, RecommendedWatcher, RecursiveMode},
};
use serde::de::DeserializeOwned;

use crate::{Flags, LoadError, Snapshot};

/// Un guardado de editor (temp + rename) llega como varios eventos: se recarga una vez, cuando
/// dejan de llegar durante este tiempo.
const DEBOUNCE: Duration = Duration::from_millis(200);

/// `force = false` es un evento del watcher: no aplica nada si el archivo no cambió.
type Reload = Arc<dyn Fn(bool) -> Result<(), LoadError> + Send + Sync>;

/// Vigila un archivo de flags. Soltarlo deja de vigilar; los [`Flags`] siguen con el último
/// snapshot aplicado.
pub struct Watcher {
    reload: Reload,
    _debouncer: Box<dyn Send>,
}

impl Watcher {
    /// Relee el archivo y lo aplica aunque no haya cambiado: para un SIGHUP o un endpoint admin.
    ///
    /// # Errors
    ///
    /// [`LoadError::Io`] si no se puede leer, y los de [`Snapshot::from_toml_str`]. En los dos
    /// casos sigue vigente el snapshot anterior y no se dispara `on_change`.
    ///
    /// # Examples
    ///
    /// ```no_run
    /// use breaker_panel::Flags;
    ///
    /// let (flags, watcher) = Flags::<()>::watch_file("flags.toml")?;
    /// // Al recibir SIGHUP:
    /// watcher.reload()?;
    /// # Ok::<(), breaker_panel::LoadError>(())
    /// ```
    pub fn reload(&self) -> Result<(), LoadError> {
        (self.reload)(true)
    }
}

impl fmt::Debug for Watcher {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Watcher").finish_non_exhaustive()
    }
}

impl<M: DeserializeOwned + Default + Send + Sync + 'static> Flags<M> {
    /// Carga los flags de un archivo y lo recarga en caliente cuando cambia.
    ///
    /// Vigila el directorio del archivo, no el archivo: así ve los guardados atómicos de los
    /// editores (temp + rename) y el cambio de symlink de un `ConfigMap` de Kubernetes. Una
    /// recarga que falla se registra con `tracing` y deja vigente el snapshot anterior. El
    /// watcher corre en su propio hilo y no necesita runtime async.
    ///
    /// # Errors
    ///
    /// Al arrancar no hay valores por defecto: [`LoadError::Io`] si el archivo no se puede leer,
    /// los de [`Snapshot::from_toml_str`] si no es válido, y [`LoadError::Watch`] si el sistema
    /// no deja vigilar el directorio.
    ///
    /// # Examples
    ///
    /// ```no_run
    /// use breaker_panel::Flags;
    ///
    /// let (flags, _watcher) = Flags::<()>::watch_file("/etc/app/flags.toml")?;
    /// flags.require("payments.ops.charge").ok();
    /// # Ok::<(), breaker_panel::LoadError>(())
    /// ```
    pub fn watch_file(path: impl AsRef<Path>) -> Result<(Arc<Self>, Watcher), LoadError> {
        start::<M, RecommendedWatcher>(path.as_ref(), notify::Config::default())
    }

    /// Como [`watch_file`](Self::watch_file), pero mirando el archivo cada `interval` en vez de
    /// esperar eventos del sistema: para los bind mounts de Docker en Mac y Windows, que no los
    /// propagan.
    ///
    /// En cada vuelta lee y hashea los archivos del directorio: el mtime que compara `notify`
    /// tiene resolución de segundos, y sin mirar el contenido se perdería un cambio que caiga en
    /// el mismo segundo que el anterior.
    ///
    /// # Errors
    ///
    /// Los de [`watch_file`](Self::watch_file).
    ///
    /// # Examples
    ///
    /// ```no_run
    /// use std::time::Duration;
    /// use breaker_panel::Flags;
    ///
    /// let (flags, _watcher) = Flags::<()>::poll_file("flags.toml", Duration::from_secs(2))?;
    /// # Ok::<(), breaker_panel::LoadError>(())
    /// ```
    pub fn poll_file(
        path: impl AsRef<Path>,
        interval: Duration,
    ) -> Result<(Arc<Self>, Watcher), LoadError> {
        let config = notify::Config::default()
            .with_poll_interval(interval)
            .with_compare_contents(true);
        start::<M, PollWatcher>(path.as_ref(), config)
    }
}

fn start<M, W>(path: &Path, config: notify::Config) -> Result<(Arc<Flags<M>>, Watcher), LoadError>
where
    M: DeserializeOwned + Default + Send + Sync + 'static,
    W: notify::Watcher + Send + 'static,
{
    let text = read(path)?;
    let flags = Arc::new(Flags::new(Snapshot::from_toml_str(&text)?));
    let reload = reloader(Arc::clone(&flags), path.to_owned(), text);
    let debouncer = debounce::<W>(path, config, Arc::clone(&reload))?;
    let watcher = Watcher {
        reload,
        _debouncer: Box::new(debouncer),
    };
    Ok((flags, watcher))
}

fn reloader<M>(flags: Arc<Flags<M>>, path: PathBuf, text: String) -> Reload
where
    M: DeserializeOwned + Default + Send + Sync + 'static,
{
    let seen = Mutex::new(text);
    Arc::new(move |force| {
        let result = apply(&flags, &path, &seen, force);
        if let Err(e) = &result {
            // Como `dyn Error`, el subscriber registra también la cadena de `source()`.
            let error: &(dyn Error + 'static) = e;
            tracing::warn!(path = %path.display(), error, "recarga de flags rechazada");
        }
        result
    })
}

/// El directorio padre avisa también de cambios en otros archivos, y el symlink de Kubernetes
/// cambia sin que el archivo aparezca en el evento: por eso se compara el contenido en vez de
/// filtrar por path. Un contenido inválido ya visto tampoco se reintenta (ni se vuelve a
/// registrar) hasta que cambie.
fn apply<M>(
    flags: &Flags<M>,
    path: &Path,
    seen: &Mutex<String>,
    force: bool,
) -> Result<(), LoadError>
where
    M: DeserializeOwned + Default,
{
    let text = read(path)?;
    let mut seen = seen.lock().unwrap_or_else(PoisonError::into_inner);
    if !force && *seen == text {
        return Ok(());
    }
    *seen = text;
    flags.replace(Snapshot::from_toml_str(&seen)?);
    Ok(())
}

fn debounce<W>(
    path: &Path,
    config: notify::Config,
    reload: Reload,
) -> Result<Debouncer<W, RecommendedCache>, LoadError>
where
    W: notify::Watcher,
{
    let dir = parent(path);
    let watch_error = |e| LoadError::Watch {
        path: dir.to_owned(),
        source: io::Error::other(e),
    };
    let on_events = move |result: DebounceEventResult| on_events(result, &reload);
    let mut debouncer =
        new_debouncer_opt::<_, W, _>(DEBOUNCE, None, on_events, RecommendedCache::new(), config)
            .map_err(watch_error)?;
    debouncer
        .watch(dir, RecursiveMode::NonRecursive)
        .map_err(watch_error)?;
    Ok(debouncer)
}

fn on_events(result: DebounceEventResult, reload: &Reload) {
    match result {
        // Los accesos se ignoran: en Linux leer el archivo ya genera uno, y recargar por ellos
        // sería un bucle.
        Ok(events) if events.iter().any(|e| !e.kind.is_access()) => {
            // El error ya lo registró el propio `reload`.
            let _ = reload(false);
        }
        Ok(_) => {}
        Err(errors) => tracing::warn!(?errors, "error vigilando el archivo de flags"),
    }
}

fn parent(path: &Path) -> &Path {
    match path.parent() {
        Some(dir) if !dir.as_os_str().is_empty() => dir,
        _ => Path::new("."),
    }
}

fn read(path: &Path) -> Result<String, LoadError> {
    fs::read_to_string(path).map_err(|source| LoadError::Io {
        path: path.to_owned(),
        source,
    })
}

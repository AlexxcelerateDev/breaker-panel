use std::{
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

use crate::{
    Flags, LoadError, Snapshot,
    flags::{Listeners, call_all, subscribe},
};

/// Un guardado de editor (temp + rename) llega como varios eventos: se recarga una vez, cuando
/// dejan de llegar durante este tiempo.
const DEBOUNCE: Duration = Duration::from_millis(200);

/// `force = false` es un evento del watcher: no aplica nada si el archivo no cambió.
type Reload = Arc<dyn Fn(bool) -> Result<(), LoadError> + Send + Sync>;

/// Vigila un archivo de flags. Soltarlo deja de vigilar; los [`Flags`] siguen con el último
/// snapshot aplicado.
///
/// Ojo con `let (flags, _) = Flags::watch_file(..)`: el patrón `_` lo suelta en el acto y el
/// archivo deja de vigilarse sin que nada avise. Va en una variable con nombre (`_watcher`
/// también vale) o en el estado de la app, junto a los flags.
pub struct Watcher {
    reload: Reload,
    rejects: Arc<Listeners<LoadError>>,
    // Solo se mantiene vivo, nunca se usa: el `Mutex` es para que `Watcher` sea `Sync` (el
    // estado de axum lo exige) sin depender de que el debouncer de cada plataforma lo sea.
    _debouncer: Mutex<Box<dyn Send>>,
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

    /// Registra un callback que recibe el error de cada recarga rechazada, la del watcher y la
    /// de [`reload`](Self::reload): un archivo roto, borrado o sin una key registrada.
    ///
    /// Un rechazo deja vigente el snapshot anterior, así que **sin esto solo se ve en el log de
    /// la librería** (target `breaker_panel::watch`), que un filtro por crate descarta: el
    /// archivo dice una cosa y el servicio hace otra, sin que nadie lo sepa. Aquí se puede
    /// registrar con el target propio, contar una métrica o marcar un health check.
    ///
    /// Corre en el hilo que intentó la recarga; como los de `on_change`, un pánico se registra y
    /// no afecta a los demás.
    ///
    /// # Examples
    ///
    /// ```no_run
    /// use breaker_panel::Flags;
    ///
    /// let (flags, watcher) = Flags::<()>::watch_file("flags.toml")?;
    /// // `{:#}` incluye la causa: la línea y la columna si el TOML no parsea.
    /// watcher.on_reject(|e| eprintln!("flags.toml rechazado, sigue el anterior: {e:#}"));
    /// # Ok::<(), breaker_panel::LoadError>(())
    /// ```
    pub fn on_reject(&self, f: impl Fn(&LoadError) + Send + Sync + 'static) {
        subscribe(&self.rejects, f);
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
    /// editores (temp + rename) y el cambio de symlink de un `ConfigMap` de Kubernetes. Por eso
    /// conviene que el archivo esté solo en su directorio: cualquier cambio a su lado lo relee.
    /// En Docker, monta el directorio: con un bind mount de un solo archivo, un guardado
    /// atómico en el host deja al contenedor con el inodo viejo para siempre.
    ///
    /// Una recarga que falla deja vigente el snapshot anterior, se registra con `tracing` y
    /// llega a [`Watcher::on_reject`]. El watcher corre en su propio hilo y no necesita runtime
    /// async.
    ///
    /// La recarga dura lo que viva el [`Watcher`] devuelto: no lo sueltes con `_`.
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
    let rejects = Arc::new(Listeners::default());
    let reload = reloader(
        Arc::clone(&flags),
        path.to_owned(),
        text,
        Arc::clone(&rejects),
    );
    let debouncer = debounce::<W>(path, config, Arc::clone(&reload))?;
    // Un cambio entre la lectura y el `watch` no genera evento: se relee una vez. Si el
    // contenido es el mismo, no hace nada; si el nuevo es inválido, queda registrado.
    let _ = reload(false);
    let _debouncer = Mutex::new(Box::new(debouncer) as Box<dyn Send>);
    Ok((
        flags,
        Watcher {
            reload,
            rejects,
            _debouncer,
        },
    ))
}

fn reloader<M>(
    flags: Arc<Flags<M>>,
    path: PathBuf,
    text: String,
    rejects: Arc<Listeners<LoadError>>,
) -> Reload
where
    M: DeserializeOwned + Default + Send + Sync + 'static,
{
    let seen = Mutex::new(text);
    Arc::new(move |force| {
        let result = apply(&flags, &path, &seen, force);
        if let Err(e) = &result {
            // `{:#}` y no el error como campo: el formateador JSON de `tracing-subscriber` no
            // pinta `source()`, y se perderían la línea y la columna.
            let error = format!("{e:#}");
            tracing::warn!(path = %path.display(), error, "recarga de flags rechazada");
            call_all(&rejects, e, "on_reject");
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
    // Tomado hasta después de `replace`, a propósito: soltarlo antes dejaría que dos recargas
    // (la del watcher y un `reload` manual) se aplicaran al revés, y el snapshot vigente sería
    // el viejo con `seen` diciendo que ya se aplicó el nuevo.
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

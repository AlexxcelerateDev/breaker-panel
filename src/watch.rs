use std::{
    fmt, fs, io,
    path::{self, Path, PathBuf},
    sync::{Arc, Mutex},
    time::Duration,
};

use notify_debouncer_full::{
    DebounceEventResult, Debouncer, RecommendedCache, new_debouncer_opt,
    notify::{self, PollWatcher, RecommendedWatcher, RecursiveMode},
};
use serde::de::DeserializeOwned;

use crate::{
    Flags, LoadError, Snapshot,
    flags::{Listeners, call_all, lock, subscribe},
};

/// Un guardado de editor (temp + rename) llega como varios eventos: se recarga una vez, cuando
/// dejan de llegar durante este tiempo.
const DEBOUNCE: Duration = Duration::from_millis(200);

/// El intervalo mínimo de [`Flags::poll_file`]: en cada vuelta lee y hashea todo el directorio.
const MIN_POLL: Duration = Duration::from_millis(100);

/// [`Source::reload`] sin el tipo de `M`, para que `Watcher` no sea genérico.
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
    /// casos sigue vigente el snapshot anterior y no se dispara `on_change`. Un rechazo ya
    /// avisado (el mismo contenido inválido, el archivo que sigue sin poder leerse) se devuelve
    /// aquí, pero no vuelve a llegar a [`on_reject`](Self::on_reject).
    ///
    /// # Panics
    ///
    /// Si se llama desde un callback de `on_change` de estos flags: se esperaría a sí mismo.
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
    /// Cada rechazo llega una vez: el mismo contenido inválido, o el archivo que sigue sin poder
    /// leerse, no vuelve a avisar hasta que cambie. Por eso un callback que llame a `reload` no
    /// entra en bucle.
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
    /// El directorio tiene que ser siempre el mismo. Si un despliegue lo borra y lo crea de
    /// nuevo (`rm -rf` y copiar, un `rsync --delete` del padre), el sistema sigue vigilando el
    /// que ya no existe: llega un rechazo al borrarlo y después nada más. Para ese despliegue
    /// está [`poll_file`](Self::poll_file), que vuelve a encontrarlo.
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
    /// no deja vigilar el directorio. Un cambio entre la primera lectura y el `watch` no
    /// generaría evento, así que se relee una vez al empezar a vigilar: si para entonces el
    /// archivo ya no es válido o no está, también falla. Con una escritura en el sitio (no
    /// atómica) a la vez que el arranque, cualquiera de las dos lecturas puede pillarlo a medias.
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
    /// el mismo segundo que el anterior. Por eso un `interval` de menos de 100 ms se sube a
    /// 100 ms: con cero, sondear ocupaba un núcleo entero.
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
            .with_poll_interval(interval.max(MIN_POLL))
            .with_compare_contents(true);
        start::<M, PollWatcher>(path.as_ref(), config)
    }
}

fn start<M, W>(path: &Path, config: notify::Config) -> Result<(Arc<Flags<M>>, Watcher), LoadError>
where
    M: DeserializeOwned + Default + Send + Sync + 'static,
    W: notify::Watcher + Send + 'static,
{
    // `notify` vigila el directorio resuelto al arrancar, pero una ruta relativa se volvería a
    // resolver en cada recarga contra el `cwd` de ese momento, y leería otro archivo.
    let path = &path::absolute(path).map_err(|source| LoadError::Io {
        path: path.to_owned(),
        source,
    })?;
    let text = read(path)?;
    let flags = Arc::new(Flags::new(Snapshot::from_toml_str(&text)?));
    let source = Arc::new(Source::new(Arc::clone(&flags), path, text));
    let debouncer = debounce::<W>(path, config, source.reloader())?;
    // Un cambio entre la lectura y el `watch` no genera evento: se relee una vez. Si ya no es
    // válido, el arranque falla, como si la primera lectura lo hubiera encontrado así (§7): aún
    // no hay `on_reject` al que avisar, y aceptarlo dejaría la réplica atrasada sin decirlo.
    source.apply(false).map_err(|rejection| rejection.error)?;
    Ok((flags, Watcher::new(&source, debouncer)))
}

impl Watcher {
    fn new<M>(source: &Arc<Source<M>>, debouncer: impl Send + 'static) -> Self
    where
        M: DeserializeOwned + Default + Send + Sync + 'static,
    {
        Self {
            reload: source.reloader(),
            rejects: Arc::clone(&source.rejects),
            _debouncer: Mutex::new(Box::new(debouncer)),
        }
    }
}

/// Lo que necesita una recarga: a quién aplicarla, de dónde leer y a quién avisar si falla.
struct Source<M> {
    flags: Arc<Flags<M>>,
    path: PathBuf,
    seen: Mutex<Seen>,
    rejects: Arc<Listeners<LoadError>>,
}

/// Lo último que se leyó del archivo, se aplicara o no.
#[derive(PartialEq)]
enum Seen {
    Text(String),
    Unreadable(io::ErrorKind),
}

impl Seen {
    fn of(read: &io::Result<String>) -> Self {
        match read {
            Ok(text) => Self::Text(text.clone()),
            Err(e) => Self::Unreadable(e.kind()),
        }
    }
}

/// Un rechazo, y si es nuevo. Uno repetido (el mismo contenido inválido, el archivo que sigue
/// sin poder leerse) no se registra ni se avisa otra vez: si no, cualquier evento del directorio
/// lo repetiría, y un `on_reject` que llamase a `reload` entraría en recursión sin fin.
struct Rejection {
    error: LoadError,
    new: bool,
}

impl<M: DeserializeOwned + Default> Source<M> {
    fn new(flags: Arc<Flags<M>>, path: &Path, text: String) -> Self {
        Self {
            flags,
            path: path.to_owned(),
            seen: Mutex::new(Seen::Text(text)),
            rejects: Arc::default(),
        }
    }

    /// La recarga que guardan el debouncer y el [`Watcher`].
    fn reloader(self: &Arc<Self>) -> Reload
    where
        M: Send + Sync + 'static,
    {
        let source = Arc::clone(self);
        Arc::new(move |force| source.reload(force))
    }

    /// `force = false` es un evento del watcher: no aplica nada si el archivo no cambió.
    fn reload(&self, force: bool) -> Result<(), LoadError> {
        self.flags.forbid_reentry();
        self.apply(force).map_err(|Rejection { error, new }| {
            if new {
                // `{:#}` y no el error como campo: el formateador JSON de `tracing-subscriber`
                // no pinta `source()`, y se perderían la línea y la columna.
                let message = format!("{error:#}");
                let path = self.path.display();
                tracing::warn!(%path, error = message, "recarga de flags rechazada");
                call_all(&self.rejects, &error, "on_reject");
            }
            error
        })
    }

    /// El directorio padre avisa también de cambios en otros archivos, y el symlink de
    /// Kubernetes cambia sin que el archivo aparezca en el evento: por eso se compara el
    /// contenido en vez de filtrar por path.
    fn apply(&self, force: bool) -> Result<(), Rejection> {
        // Leer y aplicar con `seen` tomado, `replace` incluido, a propósito: si no, dos recargas
        // (la del watcher y un `reload` manual) podrían aplicarse al revés, y el snapshot vigente
        // sería el viejo con `seen` diciendo que ya se aplicó el nuevo.
        let mut seen = lock(&self.seen);
        let read = fs::read_to_string(&self.path);
        let now = Seen::of(&read);
        let new = *seen != now;
        if !force && !new {
            return Ok(());
        }
        *seen = now;
        let reject = |error| Rejection { error, new };
        let path = self.path.clone();
        let text = read.map_err(|source| reject(LoadError::Io { path, source }))?;
        self.flags
            .replace(Snapshot::from_toml_str(&text).map_err(reject)?);
        Ok(())
    }
}

fn debounce<W>(
    path: &Path,
    config: notify::Config,
    reload: Reload,
) -> Result<Debouncer<W, RecommendedCache>, LoadError>
where
    W: notify::Watcher,
{
    // La ruta ya es absoluta (`start`): solo la raíz no tiene padre, y no se puede leer.
    let dir = path.parent().unwrap_or(path);
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

fn read(path: &Path) -> Result<String, LoadError> {
    fs::read_to_string(path).map_err(|source| LoadError::Io {
        path: path.to_owned(),
        source,
    })
}

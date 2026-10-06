use std::{
    fmt, fs, io,
    path::{self, Path, PathBuf},
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
    },
    time::Duration,
};

use notify_debouncer_full::{
    DebounceEventHandler, DebounceEventResult, DebouncedEvent, Debouncer, RecommendedCache,
    file_id::{self, FileId},
    new_debouncer_opt,
    notify::{self, PollWatcher, RecommendedWatcher, RecursiveMode, WatcherKind},
};
use serde::de::DeserializeOwned;

use crate::{
    Flags, LoadError, Snapshot,
    flags::{Listeners, call_all, lock, subscribe},
};

/// An editor save (temp + rename) arrives as several events: it reloads once, when they stop
/// arriving for this long.
const DEBOUNCE: Duration = Duration::from_millis(200);

/// The minimum interval of [`Flags::poll_file`]: each round reads and hashes the whole directory.
const MIN_POLL: Duration = Duration::from_millis(100);

/// [`Source::reload`] without the `M` type, so that `Watcher` is not generic.
type Reload = Arc<dyn Fn(bool) -> Result<(), LoadError> + Send + Sync>;

/// Watches a flags file. Dropping it stops watching; the [`Flags`] keep the last applied
/// snapshot.
///
/// Beware of `let (flags, _) = Flags::watch_file(..)`: the `_` pattern drops it on the spot and
/// the file stops being watched with nothing warning you. Bind it to a named variable
/// (`_watcher` works too) or keep it in the app state, next to the flags.
pub struct Watcher {
    reload: Reload,
    rejects: Arc<Listeners<LoadError>>,
    // Only kept alive, never used: the `Mutex` makes `Watcher` `Sync` (axum's state requires it)
    // without depending on each platform's debouncer being so.
    _debouncer: Mutex<Box<dyn Send>>,
}

impl Watcher {
    /// Rereads the file and applies it even if it has not changed: for a SIGHUP or an admin
    /// endpoint. Each call is a new revision and an `on_change` with empty lists: an endpoint
    /// that exposes it should not be callable in a loop.
    ///
    /// # Errors
    ///
    /// [`LoadError::Io`] if it cannot be read, and those of [`Snapshot::from_toml_str`]. In both
    /// cases the previous snapshot stays in effect and `on_change` is not triggered. A rejection
    /// already reported (the same invalid content, the file that still cannot be read) is
    /// returned here, but does not reach [`on_reject`](Self::on_reject) again.
    ///
    /// # Panics
    ///
    /// If called from an `on_change` callback of these flags: it would wait for itself.
    ///
    /// # Examples
    ///
    /// ```no_run
    /// use breaker_panel::Flags;
    ///
    /// let (flags, watcher) = Flags::<()>::watch_file("flags.toml")?;
    /// // On SIGHUP:
    /// watcher.reload()?;
    /// # Ok::<(), breaker_panel::LoadError>(())
    /// ```
    pub fn reload(&self) -> Result<(), LoadError> {
        (self.reload)(true)
    }

    /// Registers a callback that receives the error of each rejected reload, both the watcher's
    /// and [`reload`](Self::reload)'s: a broken or deleted file, or one missing a registered key.
    ///
    /// Each rejection arrives once: the same invalid content, or the file that still cannot be
    /// read, is not reported again until it changes. That is why a callback that calls `reload`
    /// does not loop.
    ///
    /// With [`Flags::watch_file`] a [`LoadError::Watch`] also arrives, once, if the path stops
    /// leading to the watched directory: it is deleted or recreated (on Linux and Windows: on
    /// macOS reloading continues), or a symlink above it is repointed. That one is not a
    /// rejection: what was applied may already be the new file, and what is lost is reloading
    /// the next ones. That is why the example does not say "keeping the previous one".
    ///
    /// A rejection keeps the previous snapshot in effect, so **without this it only shows up in
    /// the library's log** (target `breaker_panel::watch`), which a per-crate filter drops: the
    /// file says one thing and the service does another, without anyone knowing. Here you can log
    /// it with your own target, count a metric or flag a health check.
    ///
    /// It runs on the thread that attempted the reload; as with `on_change`, a panic is logged
    /// and does not affect the others.
    ///
    /// # Examples
    ///
    /// ```no_run
    /// use breaker_panel::Flags;
    ///
    /// let (flags, watcher) = Flags::<()>::watch_file("flags.toml")?;
    /// // `{:#}` includes the cause: the line and column if the TOML does not parse.
    /// watcher.on_reject(|e| eprintln!("flags.toml: {e:#}"));
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
    /// Loads the flags from a file and hot-reloads it when it changes.
    ///
    /// It watches the file's directory, not the file: that way it sees editors' atomic saves
    /// (temp + rename) and the symlink swap of a Kubernetes `ConfigMap`. That is why the file is
    /// best kept alone in its directory: any change next to it rereads it. In Docker, mount the
    /// directory: with a single-file bind mount no host event arrives on Docker Desktop for Mac,
    /// and on a Linux host an atomic save leaves the container with the old inode forever.
    ///
    /// On Linux and Windows the directory has to stay the same. If a deployment deletes it and
    /// creates it again (`rm -rf` and copy, an `rsync --delete` of the parent), the system keeps
    /// watching the one that no longer exists and stops seeing changes. With a deployment's
    /// immediate recreation there is not even a rejection: the new file is applied and what is
    /// lost is the next edit. That is why, as soon as it happens, a [`LoadError::Watch`] reaches
    /// [`Watcher::on_reject`]. **Except on Windows if it is renamed** (`mv conf conf.old` and
    /// another in its place): the system follows the rename silently, and no notice arrives. For
    /// those deployments there is [`poll_file`](Self::poll_file), which finds it again. On macOS
    /// it does not happen: FSEvents watches the path, finds the new directory and reloading
    /// continues.
    ///
    /// On all of them, the directory is the one the path resolves to at startup: if a symlink
    /// above it is repointed (`current -> releases/v2`), reloading stays on the old one. The
    /// [`LoadError::Watch`] arrives with that directory's next event, when something in it is
    /// touched or it is deleted; on macOS, deleting the whole release with the config in a
    /// subdirectory generates none. There too, `poll_file`.
    ///
    /// A failed reload keeps the previous snapshot in effect, is logged with `tracing` and
    /// reaches [`Watcher::on_reject`]. The watcher runs on its own thread and needs no async
    /// runtime.
    ///
    /// Reloading lasts as long as the returned [`Watcher`] lives: do not drop it with `_`.
    ///
    /// # Errors
    ///
    /// At startup there are no defaults: [`LoadError::Io`] if the file cannot be read, those of
    /// [`Snapshot::from_toml_str`] if it is not valid, and [`LoadError::Watch`] if the system
    /// does not allow watching the directory. A change between the first read and the `watch`
    /// would generate no event, so it rereads once when it starts watching: if by then the file
    /// is no longer valid or is gone, it also fails. With an in-place (non-atomic) write at the
    /// same time as startup, either of the two reads can catch it half-written.
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

    /// Like [`watch_file`](Self::watch_file), but checking the file every `interval` instead of
    /// waiting for system events: for Docker Desktop for Windows bind mounts, which do not
    /// propagate them (on Mac they do, if the directory is mounted).
    ///
    /// Each round reads and hashes the directory's files: the mtime `notify` compares has
    /// one-second resolution, and without looking at the content a change landing in the same
    /// second as the previous one would be missed. That is why an `interval` under 100 ms is
    /// raised to 100 ms: with zero, polling took up a whole core.
    ///
    /// # Errors
    ///
    /// Those of [`watch_file`](Self::watch_file).
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
        if interval < MIN_POLL {
            // Make it visible: whoever asked for 1 ms believes it polls every millisecond.
            tracing::warn!(?interval, min = ?MIN_POLL, "poll interval raised to the minimum");
        }
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
    // `notify` watches the directory resolved at startup, but a relative path would be resolved
    // again on each reload against the `cwd` of that moment, and would read another file.
    let path = &path::absolute(path).map_err(|source| LoadError::Io {
        path: path.to_owned(),
        source,
    })?;
    let text = read(path)?;
    let flags = Arc::new(Flags::new(Snapshot::from_toml_str(&text)?));
    let anchor = Anchor::of::<W>(dir(path));
    let source = Arc::new(Source::new(Arc::clone(&flags), path, text, anchor));
    let debouncer = debounce::<W>(path, config, source.event_handler())?;
    // A change between the read and the `watch` generates no event: reread once. If it is no
    // longer valid, startup fails, as if the first read had found it that way: there is no
    // `on_reject` to notify yet, and accepting it would leave the replica stale without saying so.
    source.apply(false).map_err(|rejection| rejection.error)?;
    Ok((flags, Watcher::new(&source, debouncer)))
}

/// What the backend actually watches, to know whether the path still leads to it (see
/// [`Source::check_dir`]). It is decided by the backend and not by `target_os`: `notify`'s
/// `macos_kqueue` feature, which any crate in the graph can turn on, swaps FSEvents for kqueue.
enum Anchor {
    /// inotify, kqueue and Windows follow the startup directory even if it is deleted and
    /// created again, and then stop seeing changes.
    Identity(FileId),
    /// FSEvents follows the path resolved at startup: it finds a directory recreated at it, and
    /// comparing by identifier would give a false alarm; but it does not follow a symlink
    /// repointed above it.
    Resolved(PathBuf),
}

impl Anchor {
    /// `None` with polling, which walks the path on every round and has nothing to lose.
    fn of<W: notify::Watcher>(dir: &Path) -> Option<Self> {
        match W::kind() {
            WatcherKind::PollWatcher => None,
            WatcherKind::Fsevent => fs::canonicalize(dir).ok().map(Self::Resolved),
            _ => file_id::get_file_id(dir).ok().map(Self::Identity),
        }
    }

    /// Whether `dir` still leads to what the backend watches.
    fn holds(&self, dir: &Path, events: &[DebouncedEvent]) -> bool {
        match self {
            // Both signals are needed: Linux reuses the deleted directory's inode for the new one
            // (the identifier does not change), but sends the removal of the directory itself;
            // Windows does not send it, but the identifier does change.
            Self::Identity(was) => {
                let removed = events
                    .iter()
                    .any(|e| e.kind.is_remove() && e.paths.iter().any(|p| p == dir));
                !removed && file_id::get_file_id(dir).is_ok_and(|now| now == *was)
            }
            // No directory, no notice: if it is recreated at the same path, FSEvents finds it,
            // and meanwhile the read fails and arrives as a rejection.
            Self::Resolved(was) => fs::canonicalize(dir).map_or(true, |now| now == *was),
        }
    }
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

/// What a reload needs: whom to apply it to, where to read from and whom to notify if it fails.
struct Source<M> {
    flags: Arc<Flags<M>>,
    path: PathBuf,
    seen: Mutex<Seen>,
    rejects: Arc<Listeners<LoadError>>,
    /// What the backend watches; `None` with polling.
    anchor: Option<Anchor>,
    /// Whether it was already reported that it stopped being the same: reported once.
    dir_lost: AtomicBool,
}

/// What is sent to `on_reject` when the watched directory stops being the startup one.
const DIR_LOST: &str = "the path no longer leads to the watched directory (it was deleted, \
                        recreated, or a symlink above it was repointed): event-based reloading \
                        no longer sees changes; use poll_file or restart";

/// The last thing read from the file, whether it was applied or not.
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

/// A rejection, and whether it is new. A repeated one (the same invalid content, the file that
/// still cannot be read) is not logged or reported again: otherwise any directory event would
/// repeat it, and an `on_reject` that called `reload` would recurse forever.
struct Rejection {
    error: LoadError,
    new: bool,
}

impl<M: DeserializeOwned + Default> Source<M> {
    fn new(flags: Arc<Flags<M>>, path: &Path, text: String, anchor: Option<Anchor>) -> Self {
        Self {
            flags,
            path: path.to_owned(),
            seen: Mutex::new(Seen::Text(text)),
            rejects: Arc::default(),
            anchor,
            dir_lost: AtomicBool::new(false),
        }
    }

    /// What the debouncer does with each batch of events.
    fn event_handler(self: &Arc<Self>) -> impl FnMut(DebounceEventResult) + Send + 'static
    where
        M: Send + Sync + 'static,
    {
        let source = Arc::clone(self);
        move |result| source.on_events(result)
    }

    fn on_events(&self, result: DebounceEventResult) {
        match result {
            Ok(events) => {
                // Accesses are ignored: on Linux reading the file already generates one, and
                // reloading on them would be a loop. `reload` itself already logs the error.
                if events.iter().any(|e| !e.kind.is_access()) {
                    let _ = self.reload(false);
                }
                self.check_dir(&events);
            }
            Err(errors) => {
                tracing::warn!(?errors, "error watching the flags file");
                self.check_dir(&[]);
            }
        }
    }

    /// If the path stops leading to what the backend watches (see [`Anchor`]), event-based
    /// reloading is dead, and silently. With a deployment's immediate recreation not even a
    /// rejection is left, because the last reload the old directory triggers already reads the
    /// new file. That is why it is checked after every batch of events, and reported once.
    fn check_dir(&self, events: &[DebouncedEvent]) {
        let Some(anchor) = &self.anchor else { return };
        let dir = dir(&self.path);
        if !anchor.holds(dir, events) && !self.dir_lost.swap(true, Ordering::Relaxed) {
            self.report_lost(dir);
        }
    }

    fn report_lost(&self, dir: &Path) {
        let error = LoadError::Watch {
            path: dir.to_owned(),
            source: io::Error::other(DIR_LOST),
        };
        tracing::warn!(error = format!("{error:#}"), "hot reloading has stopped");
        call_all(&self.rejects, &error, "on_reject");
    }

    /// The [`Watcher`]'s forced reload.
    fn reloader(self: &Arc<Self>) -> Reload
    where
        M: Send + Sync + 'static,
    {
        let source = Arc::clone(self);
        Arc::new(move |force| source.reload(force))
    }

    /// `force = false` is a watcher event: it applies nothing if the file did not change.
    fn reload(&self, force: bool) -> Result<(), LoadError> {
        self.flags.forbid_reentry();
        self.apply(force).map_err(|Rejection { error, new }| {
            if new {
                // `{:#}` and not the error as a field: `tracing-subscriber`'s JSON formatter does
                // not print `source()`, and the line and column would be lost.
                let message = format!("{error:#}");
                let path = self.path.display();
                tracing::warn!(%path, error = message, "flags reload rejected");
                call_all(&self.rejects, &error, "on_reject");
            }
            error
        })
    }

    /// The parent directory also reports changes to other files, and the Kubernetes symlink
    /// changes without the file appearing in the event: that is why the content is compared
    /// instead of filtering by path.
    fn apply(&self, force: bool) -> Result<(), Rejection> {
        // Read and apply with `seen` held, `replace` included, on purpose: otherwise two reloads
        // (the watcher's and a manual `reload`) could be applied in reverse order, and the
        // current snapshot would be the old one with `seen` saying the new one was applied.
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
    on_events: impl DebounceEventHandler,
) -> Result<Debouncer<W, RecommendedCache>, LoadError>
where
    W: notify::Watcher,
{
    let dir = dir(path);
    let watch_error = |e| LoadError::Watch {
        path: dir.to_owned(),
        source: io::Error::other(e),
    };
    let mut debouncer =
        new_debouncer_opt::<_, W, _>(DEBOUNCE, None, on_events, RecommendedCache::new(), config)
            .map_err(watch_error)?;
    debouncer
        .watch(dir, RecursiveMode::NonRecursive)
        .map_err(watch_error)?;
    Ok(debouncer)
}

fn read(path: &Path) -> Result<String, LoadError> {
    fs::read_to_string(path).map_err(|source| LoadError::Io {
        path: path.to_owned(),
        source,
    })
}

/// The watched directory. The path is already absolute (`start`): only the root has no parent,
/// and it cannot be read.
fn dir(path: &Path) -> &Path {
    path.parent().unwrap_or(path)
}

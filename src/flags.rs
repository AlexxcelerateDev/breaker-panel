use std::{
    fmt,
    panic::{self, AssertUnwindSafe},
    sync::{Arc, Mutex, MutexGuard, PoisonError},
    thread::{self, ThreadId},
};

use arc_swap::ArcSwap;
use serde::de::DeserializeOwned;

use crate::{FlagError, LoadError, Resolved, Snapshot};

/// Callbacks registered for one kind of notice: the `Diff`s of `on_change`, the `LoadError`s of
/// `Watcher::on_reject`.
pub(crate) type Listeners<T> = Mutex<Vec<Arc<dyn Fn(&T) + Send + Sync>>>;

/// The live flags: the current snapshot, replaceable on the fly, and who listens for changes.
///
/// A query that lets the operation through takes no locks and allocates nothing: one atomic load
/// of the snapshot and one lookup. The only exception is a thread's first query, where `arc-swap`
/// may allocate its per-thread state once (and reuse it after the thread exits). A denied query
/// does allocate, to build the [`FlagError`]. Each instance is independent, so each test creates
/// its own.
pub struct Flags<M = ()> {
    current: ArcSwap<Snapshot<M>>,
    listeners: Listeners<Diff>,
    // Serializes each `replace` end to end, notices included: two at once would compute the diff
    // against the same revision, and callbacks would see revisions out of order.
    writer: Mutex<()>,
    // The thread notifying the `on_change` callbacks, to detect one of them re-entering.
    notifying: Mutex<Option<ThreadId>>,
}

/// What changed between two revisions. Each [`Flags::on_change`] callback receives it.
///
/// The lists are in alphabetical order.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct Diff {
    /// New keys.
    pub added: Vec<String>,
    /// Keys that are gone.
    pub removed: Vec<String>,
    /// Keys still present whose effective state changed (with the cascade applied).
    pub changed: Vec<String>,
    /// Keys still disabled whose cause changed: a different `reason` or a different
    /// `disabled_by`. It is what the end user sees.
    ///
    /// `meta` is not part of the diff: comparing it would require `M: PartialEq`.
    pub reason_changed: Vec<String>,
    /// The replaced revision.
    pub previous_revision: u64,
    /// The new revision.
    pub revision: u64,
}

impl<M: DeserializeOwned + Default> Flags<M> {
    /// Loads the flags from the text of a file, without touching the disk.
    ///
    /// # Errors
    ///
    /// Those of [`Snapshot::from_toml_str`].
    ///
    /// # Examples
    ///
    /// ```
    /// use breaker_panel::Flags;
    ///
    /// let flags: Flags = Flags::from_toml_str("[flags]\n\"payments\" = { enabled = true }")?;
    /// assert_eq!(flags.require("payments"), Ok(()));
    /// # Ok::<(), breaker_panel::LoadError>(())
    /// ```
    pub fn from_toml_str(s: &str) -> Result<Self, LoadError> {
        Snapshot::from_toml_str(s).map(Self::new)
    }
}

impl<M> Flags<M> {
    pub(crate) fn new(snapshot: Snapshot<M>) -> Self {
        Self {
            current: ArcSwap::from_pointee(snapshot),
            listeners: Mutex::default(),
            writer: Mutex::default(),
            notifying: Mutex::default(),
        }
    }

    /// [`Snapshot::require`] on the current snapshot.
    ///
    /// # Errors
    ///
    /// [`FlagError::Unknown`] if the key is not declared, and [`FlagError::Disabled`] if it or an
    /// ancestor is disabled.
    ///
    /// # Examples
    ///
    /// ```
    /// use breaker_panel::{Flags, segment};
    ///
    /// let flags: Flags = Flags::from_toml_str(r#"
    ///     [flags]
    ///     "payments.methods.paypal" = { enabled = true }
    ///     "payments.ops.charge"     = { enabled = false, reason = "charges paused" }
    /// "#)?;
    /// let m = segment("paypal")?;
    /// assert!(flags.require(format!("payments.methods.{m}")).is_ok());
    /// assert!(flags.require("payments.ops.charge").is_err());
    /// # Ok::<(), Box<dyn std::error::Error>>(())
    /// ```
    // The name people bring from Unleash or OpenFeature: with the alias, rustdoc finds it and
    // rustc suggests `require` (since 1.99, ahead of similar names).
    #[doc(alias = "is_enabled")]
    pub fn require(&self, key: impl AsRef<str>) -> Result<(), FlagError> {
        self.current.load().require(key)
    }

    /// The current snapshot, to make several queries that are consistent with each other.
    ///
    /// # Examples
    ///
    /// ```
    /// use breaker_panel::Flags;
    ///
    /// let flags: Flags = Flags::from_toml_str(r#"
    ///     [flags]
    ///     "payments.methods.paypal" = { enabled = true }
    ///     "payments.methods.stripe" = { enabled = false, reason = "down" }
    /// "#)?;
    /// let snap = flags.snapshot();
    /// let enabled = snap.children("payments.methods").filter(|(_, r)| r.enabled).count();
    /// assert_eq!(enabled, 1);
    /// # Ok::<(), breaker_panel::LoadError>(())
    /// ```
    pub fn snapshot(&self) -> Arc<Snapshot<M>> {
        self.current.load_full()
    }

    /// Replaces the current snapshot, assigns it the next revision and notifies the
    /// [`on_change`](Self::on_change) callbacks.
    ///
    /// Useful for reloading from another source that serves the same format (a TOML stored in a
    /// database, or served by an endpoint): `Snapshot` is only built from TOML.
    ///
    /// It cannot fail: every `Snapshot` comes already validated by [`Snapshot::from_toml_str`].
    /// It returns once all callbacks have finished.
    ///
    /// # Panics
    ///
    /// If called from an `on_change` callback of these same flags: it would wait for the notice
    /// in progress, which is its own, and block forever.
    ///
    /// # Examples
    ///
    /// ```
    /// use breaker_panel::{Flags, Snapshot};
    ///
    /// let flags: Flags = Flags::from_toml_str("[flags]\n\"payments\" = { enabled = true }")?;
    /// flags.replace(Snapshot::from_toml_str(
    ///     "[flags]\n\"payments\" = { enabled = false, reason = \"maintenance\" }",
    /// )?);
    /// assert!(flags.require("payments").is_err());
    /// assert_eq!(flags.snapshot().revision(), 1);
    /// # Ok::<(), breaker_panel::LoadError>(())
    /// ```
    pub fn replace(&self, mut next: Snapshot<M>) {
        self.forbid_reentry();
        let _writer = lock(&self.writer);
        let prev = self.current.load_full();
        next.revision = prev.revision + 1;
        let diff = Diff::new(&prev, &next);
        self.current.store(Arc::new(next));
        tracing::info!(revision = diff.revision, "flags reloaded");
        // Declared after `_writer`, so it is dropped first: the mark goes before the lock.
        let _notifying = Notifying::start(&self.notifying);
        call_all(&self.listeners, &diff, "on_change");
    }

    /// A `replace` or a `Watcher::reload` from an `on_change` callback of these flags would wait
    /// for itself: blocked forever, and on the watcher thread, with nothing saying so. Better a
    /// panic with the reason, which `call_all` catches and logs.
    pub(crate) fn forbid_reentry(&self) {
        let notifying = *lock(&self.notifying);
        assert_ne!(
            notifying,
            Some(thread::current().id()),
            "replace or Watcher::reload called from an on_change callback of the same flags",
        );
    }

    /// Registers a callback that receives the [`Diff`] of each applied replacement. One that
    /// fails does not trigger it; one with no visible changes (a comment, a forced `reload`)
    /// does, with empty lists.
    ///
    /// Notices arrive one after another and in revision order, on the thread that made the
    /// replacement (the watcher's, when reloading the file): the callback has to return quickly.
    /// From it you can query and register another callback, but **not** call
    /// [`replace`](Self::replace) or `Watcher::reload` on these same flags: they would wait for
    /// themselves, so they panic. That is only detected on the same thread: if the callback
    /// hands it to another thread and waits for it, both block forever. A callback that panics
    /// is logged with `tracing`, and the rest are still called.
    ///
    /// # Examples
    ///
    /// ```
    /// use std::sync::mpsc;
    /// use breaker_panel::{Flags, Snapshot};
    ///
    /// let flags: Flags = Flags::from_toml_str("[flags]\n\"payments\" = { enabled = true }")?;
    /// let (tx, rx) = mpsc::channel();
    /// flags.on_change(move |diff| {
    ///     let _ = tx.send(diff.changed.clone());
    /// });
    ///
    /// flags.replace(Snapshot::from_toml_str(
    ///     "[flags]\n\"payments\" = { enabled = false, reason = \"maintenance\" }",
    /// )?);
    /// assert_eq!(rx.try_recv(), Ok(vec!["payments".to_owned()]));
    /// # Ok::<(), breaker_panel::LoadError>(())
    /// ```
    pub fn on_change(&self, f: impl Fn(&Diff) + Send + Sync + 'static) {
        subscribe(&self.listeners, f);
    }
}

impl<M: fmt::Debug> fmt::Debug for Flags<M> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Flags")
            .field("current", &self.current.load())
            .finish_non_exhaustive()
    }
}

impl Diff {
    fn new<M>(prev: &Snapshot<M>, next: &Snapshot<M>) -> Self {
        let same_state = |p: &Resolved<M>, r: &Resolved<M>| p.enabled == r.enabled;
        let same_cause = |p: &Resolved<M>, r: &Resolved<M>| {
            (&p.disabled_by, &p.reason) == (&r.disabled_by, &r.reason)
        };
        Self {
            added: only_in(next, prev),
            removed: only_in(prev, next),
            changed: in_both_where(prev, next, |p, r| !same_state(p, r)),
            reason_changed: in_both_where(prev, next, |p, r| same_state(p, r) && !same_cause(p, r)),
            previous_revision: prev.revision,
            revision: next.revision,
        }
    }
}

/// Marks the current thread as the one notifying `on_change`, until dropped.
///
/// A guard and not a pair of assignments: `call_all` catches the callbacks' panics, but not one
/// from the `tracing` subscriber that logs them, which is the app's code. If that panic left the
/// mark set, every later `replace` from this thread would panic in `forbid_reentry`, blaming a
/// callback that does not exist.
struct Notifying<'a>(&'a Mutex<Option<ThreadId>>);

impl<'a> Notifying<'a> {
    fn start(mark: &'a Mutex<Option<ThreadId>>) -> Self {
        *lock(mark) = Some(thread::current().id());
        Self(mark)
    }
}

impl Drop for Notifying<'_> {
    fn drop(&mut self) {
        *lock(self.0) = None;
    }
}

/// Locks one of the crate's mutexes ignoring poisoning, on purpose.
///
/// A std `Mutex` is poisoned if a thread panics while holding it, and from then on `lock`
/// returns `Err` to warn that the data may be half-written. Here it cannot be: each critical
/// section is an assignment or a `push`, or the mutex holds `()`. An `unwrap` would be worse
/// than useless: after a single panic, every later `replace` and reload would fail too, and hot
/// reloading would die for good.
///
/// Once `std::sync::nonpoison::Mutex` (feature `nonpoison_mutex`) is stabilized, the fields
/// switch to that type and this helper goes away.
pub(crate) fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

pub(crate) fn subscribe<T>(listeners: &Listeners<T>, f: impl Fn(&T) + Send + Sync + 'static) {
    lock(listeners).push(Arc::new(f));
}

/// Calls the callbacks outside the list's lock, so that one can register another.
///
/// A panic in one callback must not leave the following ones without their notice nor, when
/// reloading from the watcher, kill its thread: hot reloading would stop with nothing saying so.
pub(crate) fn call_all<T>(listeners: &Listeners<T>, arg: &T, hook: &str) {
    let listeners = lock(listeners).clone();
    for listener in listeners {
        let called = panic::catch_unwind(AssertUnwindSafe(|| listener(arg)));
        if called.is_err() {
            tracing::error!(hook, "a callback panicked");
        }
    }
}

fn in_both_where<M>(
    prev: &Snapshot<M>,
    next: &Snapshot<M>,
    differs: impl Fn(&Resolved<M>, &Resolved<M>) -> bool,
) -> Vec<String> {
    let keys = next
        .flags
        .iter()
        .filter(|(key, r)| prev.flags.get(*key).is_some_and(|p| differs(p, r)));
    keys.map(|(key, _)| key.clone()).collect()
}

fn only_in<M>(a: &Snapshot<M>, b: &Snapshot<M>) -> Vec<String> {
    let keys = a.flags.keys().filter(|key| !b.flags.contains_key(*key));
    keys.cloned().collect()
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::*;

    fn snap(toml: &str) -> Snapshot {
        Snapshot::from_toml_str(toml).unwrap()
    }

    #[test]
    fn replace_avisa_solo_con_las_keys_que_cambiaron() {
        let flags = Flags::new(snap(
            r#"[flags]
            "a"   = { enabled = true }
            "a.b" = { enabled = true }
            "c"   = { enabled = true }
            "d"   = { enabled = true }"#,
        ));
        let (tx, rx) = std::sync::mpsc::channel();
        flags.on_change(move |diff| tx.send(diff.clone()).unwrap());

        flags.replace(snap(
            r#"[flags]
            "a"   = { enabled = false, reason = "por a" }
            "a.b" = { enabled = true }
            "c"   = { enabled = true, reason = "un reason no cambia el estado" }
            "e"   = { enabled = true }"#,
        ));

        let esperado = Diff {
            added: vec!["e".into()],
            removed: vec!["d".into()],
            // `a.b` no cambió en el archivo, pero su estado efectivo sí: la cascada cuenta.
            changed: vec!["a".into(), "a.b".into()],
            reason_changed: vec![],
            previous_revision: 0,
            revision: 1,
        };
        assert_eq!(rx.try_recv(), Ok(esperado));
    }

    #[test]
    fn un_cambio_solo_de_motivo_sale_en_reason_changed() {
        let flags = Flags::new(snap(
            r#"[flags]
            "a"   = { enabled = false, reason = "migración" }
            "a.b" = { enabled = true }
            "c"   = { enabled = false, reason = "igual" }"#,
        ));
        let (tx, rx) = std::sync::mpsc::channel();
        flags.on_change(move |diff| tx.send(diff.clone()).unwrap());

        flags.replace(snap(
            r#"[flags]
            "a"   = { enabled = false, reason = "migración, vuelve a las 18:00" }
            "a.b" = { enabled = true }
            "c"   = { enabled = false, reason = "igual" }"#,
        ));

        let diff = rx.try_recv().unwrap();
        assert!(diff.changed.is_empty(), "{diff:?}");
        // `a.b` hereda el motivo de `a`: también cambió lo que ve su usuario.
        assert_eq!(diff.reason_changed, ["a", "a.b"]);
    }

    #[test]
    fn con_replace_concurrentes_los_avisos_llegan_en_orden_de_revision() {
        let flags = Arc::new(Flags::new(snap("[flags]")));
        let (tx, rx) = std::sync::mpsc::channel();
        flags.on_change(move |diff| tx.send(diff.revision).unwrap());

        let hilos: Vec<_> = (0..8)
            .map(|_| {
                let flags = Arc::clone(&flags);
                std::thread::spawn(move || (0..500).for_each(|_| flags.replace(snap("[flags]"))))
            })
            .collect();
        hilos.into_iter().for_each(|h| h.join().unwrap());

        // Un listener que guarda "el último estado" (un espejo, un audit log) no puede quedarse
        // con uno viejo porque dos avisos se crucen.
        let revisiones: Vec<u64> = rx.try_iter().collect();
        assert_eq!(revisiones, (1..=4000).collect::<Vec<_>>());
    }

    #[test]
    fn replace_desde_un_callback_falla_alto_en_vez_de_bloquearse() {
        let flags = Arc::new(Flags::new(snap("[flags]")));
        let dentro = Arc::clone(&flags);
        let (tx, rx) = std::sync::mpsc::channel();
        flags.on_change(move |_| {
            let r = panic::catch_unwind(AssertUnwindSafe(|| dentro.replace(snap("[flags]"))));
            tx.send(r.is_err()).unwrap();
        });

        // En otro hilo y con plazo, generoso y no una espera: si el `replace` de dentro se
        // esperase a sí mismo, el test fallaría en vez de colgar la suite (y el CI, horas).
        let fuera = Arc::clone(&flags);
        let hilo = std::thread::spawn(move || fuera.replace(snap("[flags]")));

        // El `replace` de dentro entró en pánico en vez de esperarse a sí mismo.
        assert_eq!(rx.recv_timeout(Duration::from_secs(10)), Ok(true));
        hilo.join().unwrap();
        assert_eq!(flags.snapshot().revision(), 1);
    }

    #[test]
    fn el_debug_de_flags_muestra_el_snapshot_vigente() {
        let flags = Flags::new(snap("[flags]\n\"a\" = { enabled = true }"));
        flags.replace(snap("[flags]\n\"b\" = { enabled = true }"));

        let debug = format!("{flags:?}");

        assert!(debug.starts_with("Flags {"), "{debug}");
        assert!(debug.contains("revision: 1"), "{debug}");
        assert!(debug.contains("\"b\""), "{debug}");
    }

    /// Entra en pánico con cada evento `error`, como el de `call_all` cuando un callback entra en
    /// pánico: es código del consumidor, y su pánico sí sale de `replace`.
    struct SubscriberRoto;

    impl tracing::Subscriber for SubscriberRoto {
        fn enabled(&self, _: &tracing::Metadata<'_>) -> bool {
            true
        }
        fn new_span(&self, _: &tracing::span::Attributes<'_>) -> tracing::span::Id {
            tracing::span::Id::from_u64(1)
        }
        fn record(&self, _: &tracing::span::Id, _: &tracing::span::Record<'_>) {}
        fn record_follows_from(&self, _: &tracing::span::Id, _: &tracing::span::Id) {}
        fn event(&self, event: &tracing::Event<'_>) {
            assert_ne!(
                *event.metadata().level(),
                tracing::Level::ERROR,
                "subscriber roto"
            );
        }
        fn enter(&self, _: &tracing::span::Id) {}
        fn exit(&self, _: &tracing::span::Id) {}
    }

    #[test]
    fn un_aviso_que_sale_en_panico_no_bloquea_el_siguiente_replace() {
        let flags = Flags::new(snap("[flags]"));
        flags.on_change(|_| panic!("callback roto"));
        let escapa = tracing::subscriber::with_default(SubscriberRoto, || {
            panic::catch_unwind(AssertUnwindSafe(|| flags.replace(snap("[flags]"))))
        });
        assert!(
            escapa.is_err(),
            "el pánico del subscriber tenía que salir de replace"
        );

        // Si el hilo se quedase marcado como el que avisa, `forbid_reentry` saltaría aquí.
        let r = panic::catch_unwind(AssertUnwindSafe(|| flags.replace(snap("[flags]"))));
        assert!(r.is_ok(), "replace tras un aviso que salió en pánico");
        assert_eq!(flags.snapshot().revision(), 2);
    }

    #[test]
    fn un_snapshot_tomado_no_cambia_al_reemplazar() {
        let flags = Flags::new(snap("[flags]\n\"a\" = { enabled = true }"));
        let antes = flags.snapshot();

        flags.replace(snap("[flags]\n\"a\" = { enabled = false, reason = \"x\" }"));

        assert_eq!(antes.require("a"), Ok(()));
        assert!(flags.require("a").is_err());
        assert_eq!((antes.revision(), flags.snapshot().revision()), (0, 1));
    }

    #[test]
    fn un_callback_puede_consultar_y_registrar_otro_sin_bloquearse() {
        let flags = Arc::new(Flags::new(snap("[flags]\n\"a\" = { enabled = true }")));
        let (tx, rx) = std::sync::mpsc::channel();
        let dentro = Arc::clone(&flags);
        flags.on_change(move |_| {
            tx.send(dentro.require("a").is_err()).unwrap();
            dentro.on_change(|_| {});
        });

        flags.replace(snap("[flags]\n\"a\" = { enabled = false, reason = \"x\" }"));

        // El callback ya ve el snapshot nuevo.
        assert_eq!(rx.try_recv(), Ok(true));
    }
}

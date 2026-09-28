use std::{
    fmt,
    panic::{self, AssertUnwindSafe},
    sync::{Arc, Mutex, PoisonError},
    thread::{self, ThreadId},
};

use arc_swap::ArcSwap;
use serde::de::DeserializeOwned;

use crate::{FlagError, LoadError, Resolved, Snapshot};

/// Callbacks registrados para un tipo de aviso: los `Diff` de `on_change`, los `LoadError` de
/// `Watcher::on_reject`.
pub(crate) type Listeners<T> = Mutex<Vec<Arc<dyn Fn(&T) + Send + Sync>>>;

/// Los flags vivos: el snapshot vigente, reemplazable en caliente, y quién escucha los cambios.
///
/// Una consulta que deja pasar no toma locks ni reserva memoria: un load atómico del snapshot y
/// un lookup. Una denegada sí reserva, para construir el [`FlagError`]. Cada instancia es
/// independiente, así que cada test crea la suya.
pub struct Flags<M = ()> {
    current: ArcSwap<Snapshot<M>>,
    listeners: Listeners<Diff>,
    // Serializa cada `replace` de punta a punta, avisos incluidos: dos a la vez calcularían el
    // diff contra la misma revisión, y los callbacks verían las revisiones desordenadas.
    writer: Mutex<()>,
    // El hilo que está avisando a los `on_change`, para detectar que uno de ellos reentra.
    notifying: Mutex<Option<ThreadId>>,
}

/// Lo que cambió entre dos revisiones. Lo recibe cada callback de [`Flags::on_change`].
///
/// Las listas van en orden alfabético.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct Diff {
    /// Keys nuevas.
    pub added: Vec<String>,
    /// Keys que ya no están.
    pub removed: Vec<String>,
    /// Keys que siguen y cambiaron de estado efectivo (con la cascada aplicada).
    pub changed: Vec<String>,
    /// Keys que siguen apagadas pero cambiaron de motivo: otro `reason` u otro `disabled_by`.
    /// Es lo que ve el usuario final.
    ///
    /// `meta` no entra en el diff: compararlo exigiría `M: PartialEq`.
    pub reason_changed: Vec<String>,
    /// La revisión reemplazada.
    pub previous_revision: u64,
    /// La revisión nueva.
    pub revision: u64,
}

impl<M: DeserializeOwned + Default> Flags<M> {
    /// Carga los flags desde el texto de un archivo, sin tocar disco.
    ///
    /// # Errors
    ///
    /// Los de [`Snapshot::from_toml_str`].
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

    /// [`Snapshot::require`] sobre el snapshot vigente.
    ///
    /// # Errors
    ///
    /// [`FlagError::Unknown`] si la key no está declarada, y [`FlagError::Disabled`] si ella o
    /// un ancestro está apagado.
    ///
    /// # Examples
    ///
    /// ```
    /// use breaker_panel::{Flags, segment};
    ///
    /// let flags: Flags = Flags::from_toml_str(r#"
    ///     [flags]
    ///     "payments.methods.paypal" = { enabled = true }
    ///     "payments.ops.charge"     = { enabled = false, reason = "cobros pausados" }
    /// "#)?;
    /// let m = segment("paypal")?;
    /// assert!(flags.require(format!("payments.methods.{m}")).is_ok());
    /// assert!(flags.require("payments.ops.charge").is_err());
    /// # Ok::<(), Box<dyn std::error::Error>>(())
    /// ```
    pub fn require(&self, key: impl AsRef<str>) -> Result<(), FlagError> {
        self.current.load().require(key)
    }

    /// El snapshot vigente, para hacer varias consultas consistentes entre sí.
    ///
    /// # Examples
    ///
    /// ```
    /// use breaker_panel::Flags;
    ///
    /// let flags: Flags = Flags::from_toml_str(r#"
    ///     [flags]
    ///     "payments.methods.paypal" = { enabled = true }
    ///     "payments.methods.stripe" = { enabled = false, reason = "caído" }
    /// "#)?;
    /// let snap = flags.snapshot();
    /// let activos = snap.children("payments.methods").filter(|(_, r)| r.enabled).count();
    /// assert_eq!(activos, 1);
    /// # Ok::<(), breaker_panel::LoadError>(())
    /// ```
    pub fn snapshot(&self) -> Arc<Snapshot<M>> {
        self.current.load_full()
    }

    /// Sustituye el snapshot vigente, le asigna la revisión siguiente y avisa a los callbacks de
    /// [`on_change`](Self::on_change).
    ///
    /// Sirve para recargar desde otra fuente que entregue el mismo formato (un TOML guardado en
    /// una base, o servido por un endpoint): `Snapshot` solo se construye desde TOML.
    ///
    /// No falla: todo `Snapshot` ya viene validado de [`Snapshot::from_toml_str`]. Vuelve cuando
    /// todos los callbacks han terminado.
    ///
    /// # Panics
    ///
    /// Si se llama desde un callback de `on_change` de estos mismos flags: esperaría a que
    /// terminase el aviso en curso, que es el suyo, y se bloquearía para siempre.
    ///
    /// # Examples
    ///
    /// ```
    /// use breaker_panel::{Flags, Snapshot};
    ///
    /// let flags: Flags = Flags::from_toml_str("[flags]\n\"payments\" = { enabled = true }")?;
    /// flags.replace(Snapshot::from_toml_str(
    ///     "[flags]\n\"payments\" = { enabled = false, reason = \"mantenimiento\" }",
    /// )?);
    /// assert!(flags.require("payments").is_err());
    /// assert_eq!(flags.snapshot().revision(), 1);
    /// # Ok::<(), breaker_panel::LoadError>(())
    /// ```
    pub fn replace(&self, mut next: Snapshot<M>) {
        self.forbid_reentry();
        let _writer = self.writer.lock().unwrap_or_else(PoisonError::into_inner);
        let prev = self.current.load_full();
        next.revision = prev.revision + 1;
        let diff = Diff::new(&prev, &next);
        self.current.store(Arc::new(next));
        tracing::info!(revision = diff.revision, "flags recargados");
        self.set_notifying(Some(thread::current().id()));
        call_all(&self.listeners, &diff, "on_change");
        self.set_notifying(None);
    }

    /// Un `replace` o un `Watcher::reload` desde un callback de `on_change` de estos flags se
    /// esperaría a sí mismo: bloqueado para siempre, y en el hilo del watcher, sin que nada lo
    /// diga. Mejor un pánico con el motivo, que `call_all` captura y registra.
    pub(crate) fn forbid_reentry(&self) {
        let notifying = *self
            .notifying
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        assert_ne!(
            notifying,
            Some(thread::current().id()),
            "replace o Watcher::reload desde un callback de on_change de los mismos flags",
        );
    }

    fn set_notifying(&self, thread: Option<ThreadId>) {
        *self
            .notifying
            .lock()
            .unwrap_or_else(PoisonError::into_inner) = thread;
    }

    /// Registra un callback que recibe el [`Diff`] de cada reemplazo aplicado. Uno que falla
    /// no lo dispara; uno sin cambios visibles (un comentario, un `reload` forzado) sí, con las
    /// listas vacías.
    ///
    /// Los avisos llegan uno detrás de otro y en orden de revisión, en el hilo que hizo el
    /// reemplazo (el del watcher, al recargar el archivo): el callback tiene que volver rápido.
    /// Desde él se puede consultar y registrar otro callback, pero **no** llamar a
    /// [`replace`](Self::replace) ni a `Watcher::reload` sobre estos mismos flags: se esperarían
    /// a sí mismos, así que entran en pánico. Un callback que entra en pánico se registra con
    /// `tracing`, y el resto se llaman igual.
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
    ///     "[flags]\n\"payments\" = { enabled = false, reason = \"mantenimiento\" }",
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

pub(crate) fn subscribe<T>(listeners: &Listeners<T>, f: impl Fn(&T) + Send + Sync + 'static) {
    let mut listeners = listeners.lock().unwrap_or_else(PoisonError::into_inner);
    listeners.push(Arc::new(f));
}

/// Llama a los callbacks fuera del lock de la lista, para que uno pueda registrar otro.
///
/// Un pánico en un callback no puede dejar sin aviso a los siguientes ni, al recargar desde el
/// watcher, matar su hilo: la recarga en caliente se pararía sin que nada lo dijera.
pub(crate) fn call_all<T>(listeners: &Listeners<T>, arg: &T, hook: &str) {
    let listeners = listeners
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .clone();
    for listener in listeners {
        let called = panic::catch_unwind(AssertUnwindSafe(|| listener(arg)));
        if called.is_err() {
            tracing::error!(hook, "un callback entró en pánico");
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

        flags.replace(snap("[flags]"));

        // El `replace` de dentro entró en pánico en vez de esperarse a sí mismo.
        assert_eq!(rx.try_recv(), Ok(true));
        assert_eq!(flags.snapshot().revision(), 1);
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

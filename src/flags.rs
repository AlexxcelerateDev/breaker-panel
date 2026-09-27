use std::{
    fmt,
    sync::{Arc, Mutex, PoisonError},
};

use arc_swap::ArcSwap;
use serde::de::DeserializeOwned;

use crate::{FlagError, LoadError, Snapshot};

type Listener = dyn Fn(&Diff) + Send + Sync;

/// Los flags vivos: el snapshot vigente, reemplazable en caliente, y quién escucha los cambios.
///
/// Las consultas no toman locks ni reservan memoria: un load atómico del snapshot y un lookup.
/// Cada instancia es independiente —no hay estado global—, así que cada test crea la suya.
pub struct Flags<M = ()> {
    current: ArcSwap<Snapshot<M>>,
    // También serializa los `replace`: dos a la vez calcularían el diff contra la misma revisión.
    listeners: Mutex<Vec<Arc<Listener>>>,
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
    /// [`on_change`](Self::on_change). Para fuentes propias (una DB, un endpoint).
    ///
    /// No falla: todo `Snapshot` ya viene validado de [`Snapshot::from_toml_str`].
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
        let (diff, listeners) = {
            let listeners = self
                .listeners
                .lock()
                .unwrap_or_else(PoisonError::into_inner);
            let prev = self.current.load_full();
            next.revision = prev.revision + 1;
            let diff = Diff::new(&prev, &next);
            self.current.store(Arc::new(next));
            (diff, listeners.clone())
        };
        tracing::info!(revision = diff.revision, "flags recargados");
        // Fuera del lock: un callback que registre otro o llame a `replace` no se bloquea.
        for listener in listeners {
            listener(&diff);
        }
    }

    /// Registra un callback que recibe el [`Diff`] de cada reemplazo aplicado. Uno que falla
    /// no lo dispara.
    ///
    /// Corre en el hilo que hizo el reemplazo (el del watcher, al recargar el archivo): tiene
    /// que volver rápido.
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
        let mut listeners = self
            .listeners
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        listeners.push(Arc::new(f));
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
        let changed = next
            .flags
            .iter()
            .filter(|(key, r)| prev.flags.get(*key).is_some_and(|p| p.enabled != r.enabled));
        Self {
            added: only_in(next, prev),
            removed: only_in(prev, next),
            changed: changed.map(|(key, _)| key.clone()).collect(),
            previous_revision: prev.revision,
            revision: next.revision,
        }
    }
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
            previous_revision: 0,
            revision: 1,
        };
        assert_eq!(rx.try_recv(), Ok(esperado));
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

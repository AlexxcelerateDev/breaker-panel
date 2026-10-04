use std::collections::BTreeMap;

use serde::{Deserialize, de::DeserializeOwned};

use crate::{FlagError, LoadError, TomlError, key::is_key};

/// El estado de una key con la cascada ya aplicada.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct Resolved<M = ()> {
    /// Efectivo: `false` si la key o cualquier ancestro declarado está apagado.
    pub enabled: bool,
    /// El ancestro apagado más cercano a la raíz, o la propia key. `None` si está encendida.
    pub disabled_by: Option<String>,
    /// El `reason` de `disabled_by`. `None` si está encendida.
    pub reason: Option<String>,
    /// Lo que el archivo pone en `meta`; la librería no lo interpreta.
    pub meta: M,
}

/// Los flags de una carga: inmutables, validados y con la cascada resuelta.
///
/// Varias consultas sobre el mismo `Snapshot` son consistentes entre sí aunque el archivo se
/// recargue en medio, también a través de un `.await`.
#[derive(Debug)]
pub struct Snapshot<M = ()> {
    // `BTreeMap` y no `HashMap`: `children` sale en el mismo orden en cada recarga y en cada
    // réplica. El lookup sigue siendo uno, sin recorrer el árbol.
    pub(crate) flags: BTreeMap<String, Resolved<M>>,
    pub(crate) revision: u64,
    toml: String,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields, bound = "M: DeserializeOwned + Default")]
struct File<M> {
    flags: BTreeMap<String, Entry<M>>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Entry<M> {
    enabled: bool,
    reason: Option<String>,
    #[serde(default)]
    meta: M,
}

impl<M: DeserializeOwned + Default> Snapshot<M> {
    /// Carga un snapshot desde el texto de un archivo de flags, con revisión 0.
    ///
    /// Con `M = ()` una entrada no puede traer `meta`; para leerlo, un `M` propio que implemente
    /// `Deserialize` y `Default` (el que se usa si una entrada no lo trae).
    ///
    /// # Errors
    ///
    /// Rechaza el archivo entero ante el primer problema: [`LoadError::Toml`] si no parsea, trae
    /// un campo desconocido o un `meta` que no encaja en `M`; [`LoadError::InvalidKey`],
    /// [`LoadError::MissingReason`], y con la feature `registry`, [`LoadError::MissingKey`].
    ///
    /// # Examples
    ///
    /// ```
    /// use breaker_panel::Snapshot;
    ///
    /// let snap: Snapshot = Snapshot::from_toml_str(r#"
    ///     [flags]
    ///     "payments" = { enabled = false, reason = "mantenimiento" }
    /// "#)?;
    /// assert!(snap.require("payments").is_err());
    ///
    /// let sin_reason = "[flags]\n\"payments\" = { enabled = false }";
    /// assert!(Snapshot::<()>::from_toml_str(sin_reason).is_err());
    /// # Ok::<(), breaker_panel::LoadError>(())
    /// ```
    pub fn from_toml_str(s: &str) -> Result<Self, LoadError> {
        let file: File<M> = toml::from_str(s).map_err(|e| LoadError::Toml(TomlError(e)))?;
        validate(&file.flags)?;
        let flags = resolve(file.flags);
        #[cfg(feature = "registry")]
        crate::registry::check(&flags)?;
        let toml = s.to_owned();
        Ok(Self {
            flags,
            revision: 0,
            toml,
        })
    }
}

impl<M> Snapshot<M> {
    /// Deja pasar si la key está encendida, con la cascada aplicada.
    ///
    /// # Errors
    ///
    /// [`FlagError::Unknown`] si la key no está declarada, y [`FlagError::Disabled`] si ella o
    /// un ancestro está apagado.
    ///
    /// # Examples
    ///
    /// ```
    /// use breaker_panel::{FlagError, Snapshot};
    ///
    /// let snap: Snapshot = Snapshot::from_toml_str(r#"
    ///     [flags]
    ///     "payments"                = { enabled = false, reason = "mantenimiento" }
    ///     "payments.methods.paypal" = { enabled = true }
    /// "#)?;
    /// let Err(FlagError::Disabled { disabled_by, reason, .. }) = snap.require("payments.methods.paypal")
    /// else { panic!("el padre está apagado") };
    /// assert_eq!((disabled_by.as_str(), reason.as_str()), ("payments", "mantenimiento"));
    /// # Ok::<(), breaker_panel::LoadError>(())
    /// ```
    // El nombre que trae quien viene de Unleash u OpenFeature: con el alias, rustdoc lo
    // encuentra y rustc sugiere `require` (desde 1.99, por delante de nombres parecidos).
    #[doc(alias = "is_enabled")]
    pub fn require(&self, key: impl AsRef<str>) -> Result<(), FlagError> {
        let key = key.as_ref();
        let resolved = self.get(key)?;
        let Some(by) = &resolved.disabled_by else {
            return Ok(());
        };
        tracing::debug!(key, disabled_by = %by, "require denegado");
        Err(FlagError::Disabled {
            key: key.to_owned(),
            disabled_by: by.clone(),
            reason: resolved.reason.clone().unwrap_or_default(),
        })
    }

    /// El estado resuelto de una key, esté encendida o no.
    ///
    /// # Errors
    ///
    /// [`FlagError::Unknown`] si la key no está declarada.
    ///
    /// # Examples
    ///
    /// ```
    /// use breaker_panel::Snapshot;
    ///
    /// let snap: Snapshot = Snapshot::from_toml_str("[flags]\n\"payments\" = { enabled = true }")?;
    /// assert!(snap.get("payments")?.enabled);
    /// assert!(snap.get("refunds").is_err());
    /// # Ok::<(), Box<dyn std::error::Error>>(())
    /// ```
    pub fn get(&self, key: impl AsRef<str>) -> Result<&Resolved<M>, FlagError> {
        let key = key.as_ref();
        self.flags.get(key).ok_or_else(|| FlagError::Unknown {
            key: key.to_owned(),
        })
    }

    /// Los hijos directos declarados de `prefix`, en orden alfabético. `prefix` no hace falta
    /// que esté declarado.
    ///
    /// # Examples
    ///
    /// ```
    /// use breaker_panel::Snapshot;
    ///
    /// let snap: Snapshot = Snapshot::from_toml_str(r#"
    ///     [flags]
    ///     "payments.methods.paypal"        = { enabled = true }
    ///     "payments.methods.paypal.refund" = { enabled = true }
    ///     "payments.methods.stripe"        = { enabled = false, reason = "caído" }
    /// "#)?;
    /// let methods: Vec<_> = snap.children("payments.methods").map(|(k, r)| (k, r.enabled)).collect();
    /// assert_eq!(methods, [("payments.methods.paypal", true), ("payments.methods.stripe", false)]);
    /// # Ok::<(), breaker_panel::LoadError>(())
    /// ```
    pub fn children(&self, prefix: &str) -> impl Iterator<Item = (&str, &Resolved<M>)> {
        self.flags.iter().filter_map(move |(key, resolved)| {
            let rest = key.strip_prefix(prefix)?.strip_prefix('.')?;
            (!rest.contains('.')).then_some((key.as_str(), resolved))
        })
    }

    /// La revisión: 0 al cargar, y [`Flags`](crate::Flags) la incrementa en cada reemplazo.
    ///
    /// # Examples
    ///
    /// ```
    /// use breaker_panel::Flags;
    ///
    /// let flags: Flags = Flags::from_toml_str("[flags]")?;
    /// assert_eq!(flags.snapshot().revision(), 0);
    /// # Ok::<(), breaker_panel::LoadError>(())
    /// ```
    pub fn revision(&self) -> u64 {
        self.revision
    }

    /// El TOML del que se cargó, tal cual.
    ///
    /// Es lo que permite comprobar que una réplica aplicó lo último, cosa que `revision()` no
    /// dice (es un contador por proceso):
    ///
    /// - **Réplica atrasada**: el archivo en disco distinto de `toml()` es una recarga que no se
    ///   aplicó. Puede ser un rechazo, o un evento que nunca llegó (un bind mount de Docker
    ///   Desktop o un directorio recreado con [`Flags::watch_file`](crate::Flags::watch_file)),
    ///   y eso último no lo ve ningún callback. Tolera la diferencia unos cientos de
    ///   milisegundos: es lo que tarda en recargar. No cubre un bind mount de un solo archivo: el
    ///   contenedor sigue leyendo el inodo viejo, que coincide con `toml()` aunque el host ya
    ///   tenga otro.
    /// - **Réplicas que coinciden**: un hash de `toml()` en el health check, comparado con el
    ///   del archivo desplegado, calculado fuera del contenedor; esto detecta también el caso
    ///   anterior. El algoritmo es de la app: con SHA-256, el mismo valor que
    ///   `sha256sum flags.toml`.
    ///
    /// # Examples
    ///
    /// ```
    /// use breaker_panel::Snapshot;
    ///
    /// let archivo = "[flags]\n\"payments\" = { enabled = true }\n";
    /// let snap: Snapshot = Snapshot::from_toml_str(archivo)?;
    /// // En un `/health`: `std::fs::read_to_string(path)? != snap.toml()` es una réplica atrasada.
    /// assert_eq!(snap.toml(), archivo);
    /// # Ok::<(), breaker_panel::LoadError>(())
    /// ```
    pub fn toml(&self) -> &str {
        &self.toml
    }
}

fn validate<M>(entries: &BTreeMap<String, Entry<M>>) -> Result<(), LoadError> {
    for (key, entry) in entries {
        if !is_key(key) {
            return Err(LoadError::InvalidKey { key: key.clone() });
        }
        let sin_reason = entry.reason.as_deref().is_none_or(|r| r.trim().is_empty());
        if !entry.enabled && sin_reason {
            return Err(LoadError::MissingReason { key: key.clone() });
        }
    }
    Ok(())
}

/// Aplana la cascada una vez por carga: en runtime cada consulta es un lookup.
fn resolve<M>(entries: BTreeMap<String, Entry<M>>) -> BTreeMap<String, Resolved<M>> {
    // Las causas se calculan antes de consumir `entries`; en un `BTreeMap`, `keys` e
    // `into_iter` recorren el mismo orden, así que el `zip` empareja bien.
    let causes: Vec<_> = entries.keys().map(|key| cause(key, &entries)).collect();
    let pairs = entries.into_iter().zip(causes);
    pairs
        .map(|((key, entry), (disabled_by, reason))| {
            let resolved = Resolved {
                enabled: disabled_by.is_none(),
                disabled_by,
                reason,
                meta: entry.meta,
            };
            (key, resolved)
        })
        .collect()
}

/// `(disabled_by, reason)`: el primer declarado apagado de la raíz hacia la key.
fn cause<M>(key: &str, entries: &BTreeMap<String, Entry<M>>) -> (Option<String>, Option<String>) {
    let by = key
        .match_indices('.')
        .map(|(i, _)| &key[..i])
        .chain([key])
        .find(|k| entries.get(*k).is_some_and(|e| !e.enabled));
    let reason = by.and_then(|b| entries.get(b)?.reason.clone());
    (by.map(str::to_owned), reason)
}

#[cfg(test)]
mod tests {
    use std::assert_matches;

    use super::*;

    fn snap(toml: &str) -> Snapshot {
        Snapshot::from_toml_str(toml).unwrap()
    }

    fn resolved(disabled_by: Option<&str>, reason: Option<&str>) -> Resolved {
        Resolved {
            enabled: disabled_by.is_none(),
            disabled_by: disabled_by.map(str::to_owned),
            reason: reason.map(str::to_owned),
            meta: (),
        }
    }

    #[test]
    fn padre_apagado_apaga_al_hijo_con_su_reason() {
        let s = snap(
            r#"[flags]
            "a"   = { enabled = false, reason = "por a" }
            "a.b" = { enabled = true }"#,
        );
        assert_eq!(s.get("a.b"), Ok(&resolved(Some("a"), Some("por a"))));
    }

    #[test]
    fn con_dos_ancestros_apagados_manda_el_mas_alto() {
        let s = snap(
            r#"[flags]
            "a"     = { enabled = false, reason = "por a" }
            "a.b"   = { enabled = false, reason = "por b" }
            "a.b.c" = { enabled = true }"#,
        );
        assert_eq!(s.get("a.b.c"), Ok(&resolved(Some("a"), Some("por a"))));
        assert_eq!(s.get("a.b"), Ok(&resolved(Some("a"), Some("por a"))));
    }

    #[test]
    fn apagada_solo_ella_es_su_propio_disabled_by() {
        let s = snap(
            r#"[flags]
            "a"   = { enabled = true }
            "a.b" = { enabled = false, reason = "por b" }"#,
        );
        assert_eq!(s.get("a.b"), Ok(&resolved(Some("a.b"), Some("por b"))));
        assert_eq!(s.get("a"), Ok(&resolved(None, None)));
    }

    #[test]
    fn segmento_intermedio_no_declarado_es_neutro() {
        let s = snap(
            r#"[flags]
            "a"     = { enabled = false, reason = "por a" }
            "a.b.c" = { enabled = true }
            "x"     = { enabled = true }
            "x.y.z" = { enabled = true }"#,
        );
        // Ni apaga ni corta la cascada: `a.b` no existe, pero `a` llega hasta `a.b.c`.
        assert_eq!(s.get("x.y.z"), Ok(&resolved(None, None)));
        assert_eq!(s.get("a.b.c"), Ok(&resolved(Some("a"), Some("por a"))));
        assert_eq!(
            s.require("x.y"),
            Err(FlagError::Unknown { key: "x.y".into() })
        );
    }

    #[test]
    fn require_de_key_no_declarada_es_unknown() {
        let s = snap("[flags]\n\"a\" = { enabled = true }");
        assert_eq!(s.require("b"), Err(FlagError::Unknown { key: "b".into() }));
        assert_eq!(s.require("a"), Ok(()));
    }

    #[test]
    fn require_de_key_apagada_trae_el_motivo() {
        let s = snap(
            r#"[flags]
            "a"   = { enabled = false, reason = "por a" }
            "a.b" = { enabled = true }"#,
        );
        let esperado = FlagError::Disabled {
            key: "a.b".into(),
            disabled_by: "a".into(),
            reason: "por a".into(),
        };
        assert_eq!(s.require("a.b"), Err(esperado));
    }

    #[test]
    fn children_solo_devuelve_hijos_directos_declarados() {
        let s = snap(
            r#"[flags]
            "payments"                       = { enabled = true }
            "payments.methods.paypal"        = { enabled = true }
            "payments.methods.paypal.refund" = { enabled = true }
            "payments.methods.stripe"        = { enabled = true }
            "payments.methodsx"              = { enabled = true }
            "payments.ops.charge"            = { enabled = true }"#,
        );
        let keys: Vec<_> = s.children("payments.methods").map(|(k, _)| k).collect();
        assert_eq!(keys, ["payments.methods.paypal", "payments.methods.stripe"]);
        assert_eq!(s.children("payments.ops.charge").count(), 0);
    }

    #[test]
    fn la_carga_rechaza_lo_que_no_parsea_o_no_encaja() {
        let casos = [
            ("vacío, sin [flags]", ""),
            ("sección desconocida", "[flags]\n[flag]"),
            (
                "campo desconocido",
                "[flags]\n\"a\" = { enabled = true, enable = true }",
            ),
            ("sin enabled", "[flags]\n\"a\" = { reason = \"x\" }"),
            (
                "key duplicada",
                "[flags]\n\"a\" = { enabled = true }\n\"a\" = { enabled = true }",
            ),
            // Sin comillas, TOML lee `a.b` como tablas anidadas: `a` acaba con un campo `b`.
            (
                "key sin comillas",
                "[flags]\na = { enabled = true }\na.b = { enabled = true }",
            ),
            (
                "meta con M = ()",
                "[flags]\n\"a\" = { enabled = true, meta = { x = 1 } }",
            ),
        ];
        for (caso, toml) in casos {
            let r = Snapshot::<()>::from_toml_str(toml);
            assert_matches!(r, Err(LoadError::Toml(_)), "{caso}");
        }
    }

    #[test]
    fn la_carga_rechaza_keys_con_formato_invalido() {
        for key in ["A", "a..b", "a.", "pay-pal", "a b"] {
            let r =
                Snapshot::<()>::from_toml_str(&format!("[flags]\n{key:?} = {{ enabled = true }}"));
            assert_matches!(r, Err(LoadError::InvalidKey { .. }), "{key:?}");
        }
    }

    #[test]
    fn la_carga_rechaza_apagados_sin_reason() {
        for entry in [
            "enabled = false",
            "enabled = false, reason = \"\"",
            "enabled = false, reason = \" \"",
        ] {
            let r = Snapshot::<()>::from_toml_str(&format!("[flags]\n\"a\" = {{ {entry} }}"));
            assert_matches!(r, Err(LoadError::MissingReason { .. }), "{entry}");
        }
    }

    #[test]
    fn meta_se_deserializa_en_m_y_si_falta_usa_default() {
        #[derive(Debug, Default, PartialEq, Deserialize)]
        struct Meta {
            display_name: String,
        }
        let s: Snapshot<Meta> = Snapshot::from_toml_str(
            r#"[flags]
            "paypal" = { enabled = true, meta = { display_name = "PayPal" } }
            "stripe" = { enabled = true }"#,
        )
        .unwrap();
        assert_eq!(s.get("paypal").unwrap().meta.display_name, "PayPal");
        assert_eq!(s.get("stripe").unwrap().meta, Meta::default());
    }
}

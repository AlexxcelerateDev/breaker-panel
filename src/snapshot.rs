use std::collections::BTreeMap;

use serde::{Deserialize, de::DeserializeOwned};

use crate::{FlagError, LoadError, TomlError, key::is_key};

/// The state of a key with the cascade already applied.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct Resolved<M = ()> {
    /// Effective: `false` if the key or any declared ancestor is disabled.
    pub enabled: bool,
    /// The disabled ancestor closest to the root, or the key itself. `None` if enabled.
    pub disabled_by: Option<String>,
    /// The `reason` of `disabled_by`. `None` if enabled.
    pub reason: Option<String>,
    /// Whatever the file puts in `meta`; the library does not interpret it.
    pub meta: M,
}

/// The flags of one load: immutable, validated and with the cascade resolved.
///
/// Several queries on the same `Snapshot` are consistent with each other even if the file is
/// reloaded in between, also across an `.await`.
#[derive(Debug)]
pub struct Snapshot<M = ()> {
    // `BTreeMap` and not `HashMap`: `children` comes out in the same order on every reload and
    // on every replica. The lookup is still a single one, without walking the tree.
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
    /// Loads a snapshot from the text of a flags file, with revision 0.
    ///
    /// With `M = ()` an entry cannot have `meta`; to read it, use your own `M` that implements
    /// `Deserialize` and `Default` (used when an entry has none). To load any file and ignore
    /// `meta`, as a validator that does not know the app's `M` would, use
    /// `serde::de::IgnoredAny`.
    ///
    /// # Errors
    ///
    /// Rejects the whole file at the first problem: [`LoadError::Toml`] if it does not parse, has
    /// an unknown field or a `meta` that does not fit `M`; [`LoadError::InvalidKey`],
    /// [`LoadError::MissingReason`], and with the `registry` feature, [`LoadError::MissingKey`].
    ///
    /// # Examples
    ///
    /// ```
    /// use breaker_panel::Snapshot;
    ///
    /// let snap: Snapshot = Snapshot::from_toml_str(r#"
    ///     [flags]
    ///     "payments" = { enabled = false, reason = "maintenance" }
    /// "#)?;
    /// assert!(snap.require("payments").is_err());
    ///
    /// let no_reason = "[flags]\n\"payments\" = { enabled = false }";
    /// assert!(Snapshot::<()>::from_toml_str(no_reason).is_err());
    ///
    /// let with_meta = "[flags]\n\"payments\" = { enabled = true, meta = { owner = \"x\" } }";
    /// assert!(Snapshot::<()>::from_toml_str(with_meta).is_err());
    /// assert!(Snapshot::<serde::de::IgnoredAny>::from_toml_str(with_meta).is_ok());
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
    /// Lets the operation through if the key is enabled, with the cascade applied.
    ///
    /// # Errors
    ///
    /// [`FlagError::Unknown`] if the key is not declared, and [`FlagError::Disabled`] if it or an
    /// ancestor is disabled.
    ///
    /// # Examples
    ///
    /// ```
    /// use breaker_panel::{FlagError, Snapshot};
    ///
    /// let snap: Snapshot = Snapshot::from_toml_str(r#"
    ///     [flags]
    ///     "payments"                = { enabled = false, reason = "maintenance" }
    ///     "payments.methods.paypal" = { enabled = true }
    /// "#)?;
    /// let Err(FlagError::Disabled { disabled_by, reason, .. }) = snap.require("payments.methods.paypal")
    /// else { panic!("the parent is disabled") };
    /// assert_eq!((disabled_by.as_str(), reason.as_str()), ("payments", "maintenance"));
    /// # Ok::<(), breaker_panel::LoadError>(())
    /// ```
    // The name people bring from Unleash or OpenFeature: with the alias, rustdoc finds it and
    // rustc suggests `require` (since 1.99, ahead of similar names).
    #[doc(alias = "is_enabled")]
    pub fn require(&self, key: impl AsRef<str>) -> Result<(), FlagError> {
        let key = key.as_ref();
        let resolved = self.get(key)?;
        let Some(by) = &resolved.disabled_by else {
            return Ok(());
        };
        tracing::debug!(key, disabled_by = %by, "require denied");
        Err(FlagError::Disabled {
            key: key.to_owned(),
            disabled_by: by.clone(),
            reason: resolved.reason.clone().unwrap_or_default(),
        })
    }

    /// The resolved state of a key, whether it is enabled or not.
    ///
    /// # Errors
    ///
    /// [`FlagError::Unknown`] if the key is not declared.
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

    /// The declared direct children of `prefix`, in alphabetical order. `prefix` itself does not
    /// need to be declared.
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
    ///     "payments.methods.stripe"        = { enabled = false, reason = "down" }
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

    /// The revision: 0 on load, and [`Flags`](crate::Flags) increments it on each replacement.
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

    /// The TOML it was loaded from, verbatim.
    ///
    /// It is what lets you check that a replica applied the latest version, which `revision()`
    /// does not tell you (it is a per-process counter):
    ///
    /// - **Stale replica**: a file on disk that differs from `toml()` is a reload that was not
    ///   applied. It may be a rejection, or an event that never arrived (a Docker Desktop for
    ///   Windows bind mount, a directory renamed on Windows, or a symlink repointed until the old
    ///   directory changes, with `Flags::watch_file`), and that last case reaches no callback.
    ///   Tolerate the difference for a few hundred milliseconds: that is how long reloading
    ///   takes. It does not cover a single-file bind mount on a Linux host: the container keeps
    ///   reading the old inode, which matches `toml()` even though the host already has another.
    /// - **Matching replicas**: a hash of `toml()` in the health check, compared with that of the
    ///   deployed file, computed outside the container; this also detects the previous case. The
    ///   algorithm is the app's choice: with SHA-256, the same value as `sha256sum flags.toml`.
    ///
    /// # Examples
    ///
    /// ```
    /// use breaker_panel::Snapshot;
    ///
    /// let file = "[flags]\n\"payments\" = { enabled = true }\n";
    /// let snap: Snapshot = Snapshot::from_toml_str(file)?;
    /// // In a `/health`: `std::fs::read_to_string(path)? != snap.toml()` is a stale replica.
    /// assert_eq!(snap.toml(), file);
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
        let no_reason = entry.reason.as_deref().is_none_or(|r| r.trim().is_empty());
        if !entry.enabled && no_reason {
            return Err(LoadError::MissingReason { key: key.clone() });
        }
    }
    Ok(())
}

/// Flattens the cascade once per load: at runtime each query is a lookup.
fn resolve<M>(entries: BTreeMap<String, Entry<M>>) -> BTreeMap<String, Resolved<M>> {
    // The causes are computed before consuming `entries`; in a `BTreeMap`, `keys` and
    // `into_iter` walk the same order, so the `zip` pairs them correctly.
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

/// `(disabled_by, reason)`: the first declared disabled one from the root down to the key.
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
            assert!(matches!(r, Err(LoadError::Toml(_))), "{caso}: {r:?}");
        }
    }

    #[test]
    fn la_carga_rechaza_keys_con_formato_invalido() {
        for key in ["A", "a..b", "a.", "pay-pal", "a b"] {
            let r =
                Snapshot::<()>::from_toml_str(&format!("[flags]\n{key:?} = {{ enabled = true }}"));
            assert!(
                matches!(r, Err(LoadError::InvalidKey { .. })),
                "{key:?}: {r:?}"
            );
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
            assert!(
                matches!(r, Err(LoadError::MissingReason { .. })),
                "{entry}: {r:?}"
            );
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

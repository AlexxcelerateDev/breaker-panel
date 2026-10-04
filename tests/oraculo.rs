//! La cascada, `require`, `children`, `toml()` y `Diff` contra un oráculo escrito aparte, sobre
//! archivos aleatorios con semilla fija.
//!
//! El oráculo recorre de la key hacia la raíz y se queda con el último apagado que ve, que es el
//! más alto: otro recorrido que el de `snapshot.rs`, a propósito. Los segmentos se pisan como
//! prefijo de texto sin serlo de jerarquía (`a`, `ab`, `a_b`), que es donde se equivocaría una
//! cascada hecha con `starts_with`.
//!
//! 300 casos tardan ~0,12 s en debug. Con 100, un oráculo que elige el apagado más bajo en vez
//! del más alto ya da 159 discrepancias: la prueba es sensible con muy pocos casos.

use std::{
    collections::BTreeMap,
    sync::{Arc, Mutex},
};

use breaker_panel::{Diff, FlagError, Flags, Snapshot};

const CASOS: u32 = 300;
const SEGMENTOS: [&str; 6] = ["a", "b", "ab", "a_b", "0", "x1"];
const MOTIVOS: [&str; 5] = [
    "mantenimiento",
    "con \"comillas\"",
    "línea\nnueva",
    "barra \\ invertida",
    "emoji 🚀",
];

/// xorshift64*: determinista por semilla, sin dependencias.
struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 >> 12;
        self.0 ^= self.0 << 25;
        self.0 ^= self.0 >> 27;
        self.0.wrapping_mul(0x2545_F491_4F6C_DD1D)
    }

    fn below(&mut self, n: usize) -> usize {
        usize::try_from(self.next() % n as u64).unwrap_or_default()
    }

    fn chance(&mut self, por_ciento: u64) -> bool {
        self.next() % 100 < por_ciento
    }
}

#[derive(Clone)]
struct Entrada {
    enabled: bool,
    reason: Option<String>,
}

type Archivo = BTreeMap<String, Entrada>;

fn key(rng: &mut Rng) -> String {
    let segmentos: Vec<&str> = (0..=rng.below(5))
        .map(|_| SEGMENTOS[rng.below(SEGMENTOS.len())])
        .collect();
    segmentos.join(".")
}

fn motivo(rng: &mut Rng) -> String {
    format!("{} #{}", MOTIVOS[rng.below(MOTIVOS.len())], rng.below(1000))
}

fn entrada(rng: &mut Rng) -> Entrada {
    let enabled = rng.chance(70);
    let reason = (!enabled || rng.chance(20)).then(|| motivo(rng));
    Entrada { enabled, reason }
}

fn archivo(rng: &mut Rng) -> Archivo {
    (0..=rng.below(25))
        .map(|_| (key(rng), entrada(rng)))
        .collect()
}

/// Quitar, alternar, cambiar el motivo y añadir: lo que hace una edición del archivo.
fn mutar(rng: &mut Rng, a: &Archivo) -> Archivo {
    let mut b = Archivo::new();
    for (k, e) in a {
        let mut e = e.clone();
        if rng.chance(20) {
            e.enabled = !e.enabled;
        }
        if !e.enabled && (e.reason.is_none() || rng.chance(15)) {
            e.reason = Some(motivo(rng));
        }
        if !rng.chance(15) {
            b.insert(k.clone(), e);
        }
    }
    (0..rng.below(4)).for_each(|_| _ = b.insert(key(rng), entrada(rng)));
    b
}

/// `{:?}` de Rust escapa igual que un string básico de TOML para estos motivos: comillas,
/// barra, `\n` y caracteres imprimibles.
fn toml(a: &Archivo) -> String {
    let lineas = a.iter().map(|(k, e)| match &e.reason {
        Some(r) => format!("{k:?} = {{ enabled = {}, reason = {r:?} }}", e.enabled),
        None => format!("{k:?} = {{ enabled = {} }}", e.enabled),
    });
    format!("[flags]\n{}\n", lineas.collect::<Vec<_>>().join("\n"))
}

fn padre(k: &str) -> Option<&str> {
    k.rfind('.').map(|i| &k[..i])
}

/// `(enabled, disabled_by, reason)`.
fn resolver(a: &Archivo, k: &str) -> (bool, Option<String>, Option<String>) {
    let mut actual = Some(k);
    let mut mas_alto = None;
    while let Some(c) = actual {
        if a.get(c).is_some_and(|e| !e.enabled) {
            mas_alto = Some(c);
        }
        actual = padre(c);
    }
    let reason = mas_alto.and_then(|t| a.get(t)?.reason.clone());
    (mas_alto.is_none(), mas_alto.map(str::to_owned), reason)
}

fn hijos(a: &Archivo, prefijo: &str) -> Vec<String> {
    let hijos = a.keys().filter(|k| padre(k) == Some(prefijo));
    hijos.cloned().collect()
}

/// `(added, removed, changed, reason_changed)`.
fn diff(a: &Archivo, b: &Archivo) -> [Vec<String>; 4] {
    let solo_en =
        |x: &Archivo, y: &Archivo| x.keys().filter(|k| !y.contains_key(*k)).cloned().collect();
    let comunes: Vec<&String> = b.keys().filter(|k| a.contains_key(*k)).collect();
    let donde = |f: &dyn Fn(&str) -> bool| {
        comunes
            .iter()
            .filter(|k| f(k))
            .map(|k| (*k).clone())
            .collect()
    };
    let (ra, rb) = (|k: &str| resolver(a, k), |k: &str| resolver(b, k));
    [
        solo_en(b, a),
        solo_en(a, b),
        donde(&|k| ra(k).0 != rb(k).0),
        donde(&|k| ra(k).0 == rb(k).0 && (ra(k).1, ra(k).2) != (rb(k).1, rb(k).2)),
    ]
}

/// Todas las discrepancias de un snapshot con su archivo, como texto para el assert.
fn discrepancias(rng: &mut Rng, a: &Archivo, snap: &Snapshot) -> Vec<String> {
    let mut fallos = Vec::new();
    for k in a.keys() {
        let esperado = resolver(a, k);
        let lib = snap
            .get(k)
            .map(|r| (r.enabled, r.disabled_by.clone(), r.reason.clone()));
        if lib.as_ref() != Ok(&esperado) {
            fallos.push(format!("get({k:?}) = {lib:?}, oráculo {esperado:?}"));
        }
        let require = match (snap.require(k), &esperado) {
            (Ok(()), (true, ..)) => true,
            (
                Err(FlagError::Disabled {
                    disabled_by,
                    reason,
                    ..
                }),
                (false, by, re),
            ) => Some(&disabled_by) == by.as_ref() && Some(&reason) == re.as_ref(),
            _ => false,
        };
        if !require {
            fallos.push(format!("require({k:?}) = {:?}", snap.require(k)));
        }
    }
    fallos.extend(desconocidas(rng, a, snap));
    fallos.extend(children(rng, a, snap));
    fallos
}

fn desconocidas(rng: &mut Rng, a: &Archivo, snap: &Snapshot) -> Vec<String> {
    let keys = (0..5).map(|_| key(rng)).filter(|k| !a.contains_key(k));
    let mal = keys.filter(|k| !matches!(snap.require(k), Err(FlagError::Unknown { .. })));
    mal.map(|k| format!("require({k:?}) no declarada = {:?}", snap.require(&k)))
        .collect()
}

/// `children` de `""`, de cada key, de cada prefijo propio y de prefijos al azar.
fn children(rng: &mut Rng, a: &Archivo, snap: &Snapshot) -> Vec<String> {
    let mut prefijos = vec![String::new(), key(rng), key(rng)];
    for k in a.keys() {
        let mut actual = Some(k.as_str());
        while let Some(p) = actual {
            prefijos.push(p.to_owned());
            actual = padre(p);
        }
    }
    let mal = prefijos.into_iter().filter_map(|p| {
        let lib: Vec<String> = snap.children(&p).map(|(k, _)| k.to_owned()).collect();
        let esperado = hijos(a, &p);
        (lib != esperado).then(|| format!("children({p:?}) = {lib:?}, oráculo {esperado:?}"))
    });
    mal.collect()
}

#[test]
fn la_cascada_children_y_diff_cuadran_con_el_oraculo() {
    let mut rng = Rng(0x9E37_79B9_7F4A_7C15);
    let mut fallos = Vec::new();
    for caso in 0..CASOS {
        let a = archivo(&mut rng);
        let b = mutar(&mut rng, &a);
        let texto = toml(&a);
        let flags: Flags = Flags::from_toml_str(&texto).unwrap();
        assert_eq!(flags.snapshot().toml(), texto, "caso {caso}: toml()");
        let antes = discrepancias(&mut rng, &a, &flags.snapshot());
        fallos.extend(antes.into_iter().map(|f| format!("caso {caso}: {f}")));

        let visto: Arc<Mutex<Option<Diff>>> = Arc::default();
        let v = Arc::clone(&visto);
        flags.on_change(move |d| *v.lock().unwrap() = Some(d.clone()));
        flags.replace(Snapshot::from_toml_str(&toml(&b)).unwrap());
        let d = visto.lock().unwrap().take().unwrap();
        let lib = [d.added, d.removed, d.changed, d.reason_changed];
        if lib != diff(&a, &b) {
            fallos.push(format!(
                "caso {caso}: Diff {lib:?}, oráculo {:?}",
                diff(&a, &b)
            ));
        }
        let despues = discrepancias(&mut rng, &b, &flags.snapshot());
        fallos.extend(
            despues
                .into_iter()
                .map(|f| format!("caso {caso} tras replace: {f}")),
        );
    }
    assert!(
        fallos.is_empty(),
        "{} discrepancias:\n{}",
        fallos.len(),
        fallos[..fallos.len().min(10)].join("\n")
    );
}

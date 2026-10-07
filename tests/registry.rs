//! Keys registradas con `flag_key!`. Van en su propio fichero porque el registro es por binario:
//! aquí toda carga exige `payments.methods.paypal`, y en el resto de tests nadie la declara.
#![cfg(feature = "registry")]

use breaker_panel::{Flags, LoadError, flag_key};

flag_key!(PAYPAL = "payments.methods.paypal");

const CON: &str = "[flags]\n\"payments.methods.paypal\" = { enabled = true }\n";
const SIN: &str = "[flags]\n\"payments\" = { enabled = true }\n";

#[test]
fn key_registrada_ausente_falla_al_arrancar() {
    let r = Flags::<()>::from_toml_str(SIN);
    assert!(
        matches!(&r, Err(LoadError::MissingKey { key }) if key == "payments.methods.paypal"),
        "{r:?}"
    );
}

#[test]
fn key_registrada_se_consulta_por_su_static() {
    let flags: Flags = Flags::from_toml_str(CON).unwrap();
    assert_eq!(flags.require(PAYPAL), Ok(()));
}

#[cfg(feature = "watch")]
#[test]
fn reload_que_quita_una_key_registrada_se_rechaza() {
    let dir = std::path::PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join("registry_reload");
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("flags.toml");
    std::fs::write(&path, CON).unwrap();
    let (flags, watcher) = Flags::<()>::watch_file(&path).unwrap();

    std::fs::write(&path, SIN).unwrap();
    let r = watcher.reload();

    assert!(matches!(r, Err(LoadError::MissingKey { .. })), "{r:?}");
    assert_eq!(flags.require(PAYPAL), Ok(()));
    assert_eq!(flags.snapshot().revision(), 0);
}

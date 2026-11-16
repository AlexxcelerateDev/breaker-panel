//! Una consulta que deja pasar no reserva memoria: lo promete la doc de `Flags` y lo pide
//! `docs/REQUIREMENTS.md` §10. Va en su propio fichero porque el allocator global es uno por
//! binario.
//!
//! El contador es por hilo: el harness reserva en el suyo mientras corre el test, y un contador
//! global lo contaría.

use std::{
    alloc::{GlobalAlloc, Layout, System},
    cell::Cell,
};

use breaker_panel::Flags;

thread_local! {
    // `const` y sin `Drop`: acceder no reserva ni registra un destructor, que volvería a entrar
    // en el allocator.
    static RESERVAS: Cell<u64> = const { Cell::new(0) };
}

/// `System`, contando cada reserva en el hilo que la hace.
struct Contador;

// La única excepción a `unsafe_code = "deny"`: un allocator propio pasa por este trait.
#[expect(
    unsafe_code,
    reason = "GlobalAlloc es un trait unsafe; todo se delega en System"
)]
// SAFETY: cada método delega en `System` con los mismos argumentos, así que hereda su contrato.
// `realloc` y `alloc_zeroed` usan la implementación por defecto, que llama a `alloc` y la cuenta.
unsafe impl GlobalAlloc for Contador {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        // `try_with`: durante la destrucción de un hilo el `thread_local` puede no estar ya.
        let _ = RESERVAS.try_with(|n| n.set(n.get() + 1));
        // SAFETY: el llamador cumple el contrato de `GlobalAlloc::alloc`, el mismo que `System`.
        unsafe { System.alloc(layout) }
    }

    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        // SAFETY: `ptr` viene de `alloc`, es decir, de `System`, con este mismo `layout`.
        unsafe { System.dealloc(ptr, layout) }
    }
}

#[global_allocator]
static GLOBAL: Contador = Contador;

fn reservas(f: impl FnOnce()) -> u64 {
    let antes = RESERVAS.with(Cell::get);
    f();
    RESERVAS.with(Cell::get) - antes
}

const ARCHIVO: &str = r#"[flags]
"payments"                = { enabled = true }
"payments.methods.paypal" = { enabled = true }
"payments.ops.refund"     = { enabled = false, reason = "pausado" }"#;

#[test]
fn require_que_deja_pasar_no_reserva_memoria() {
    let flags: Flags = Flags::from_toml_str(ARCHIVO).unwrap();
    let snap = flags.snapshot();
    // arc-swap reserva un nodo la primera vez que cada hilo hace `load`: la promesa vale desde
    // la segunda consulta del hilo.
    assert_eq!(flags.require("payments"), Ok(()));

    let n = reservas(|| {
        for _ in 0..1000 {
            assert_eq!(flags.require("payments.methods.paypal"), Ok(()));
            assert_eq!(snap.require("payments.methods.paypal"), Ok(()));
            assert!(snap.get("payments").is_ok());
        }
    });

    assert_eq!(n, 0, "reservas en 3000 consultas que dejan pasar");
}

#[test]
fn el_contador_ve_las_reservas() {
    // Sin esto, un contador roto (otro allocator, el `thread_local` sin tocar) daría 0 y el test
    // de arriba pasaría sin probar nada. La consulta denegada construye el `FlagError`.
    let flags: Flags = Flags::from_toml_str(ARCHIVO).unwrap();
    assert_eq!(flags.require("payments"), Ok(()));

    let n = reservas(|| assert!(flags.require("payments.ops.refund").is_err()));

    assert!(n > 0, "{n} reservas al construir un FlagError");
}

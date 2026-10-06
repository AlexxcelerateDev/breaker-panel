# breaker-panel

Embedded hierarchical kill switches for Rust: a local TOML file, hot reload and cascading by key
prefix, no external service.

They are *ops toggles* in Pete Hodgson's taxonomy: turn off a failing provider or pause an
operation on the fly, without a redeploy. They are not release toggles, experiments or per-user
permissions.

```toml
[flags]
"payments"                       = { enabled = true }
"payments.methods.paypal"        = { enabled = true, meta = { display_name = "PayPal" } }
"payments.methods.paypal.refund" = { enabled = false, reason = "PayPal is not processing refunds today" }
"payments.methods.stripe"        = { enabled = true, meta = { display_name = "Stripe" } }
"payments.methods.stripe.refund" = { enabled = true }
"payments.ops.charge"            = { enabled = true }
"payments.ops.refund"            = { enabled = true }
```

```rust
use breaker_panel::{Flags, segment};
use serde::Deserialize;

#[derive(Debug, Default, Deserialize)]
struct Meta {
    display_name: String,
}

# fn main() -> Result<(), Box<dyn std::error::Error>> {
# let toml = r#"
# [flags]
# "payments"                       = { enabled = true }
# "payments.methods.paypal"        = { enabled = true, meta = { display_name = "PayPal" } }
# "payments.methods.paypal.refund" = { enabled = false, reason = "PayPal is not processing refunds today" }
# "payments.methods.stripe"        = { enabled = true, meta = { display_name = "Stripe" } }
# "payments.methods.stripe.refund" = { enabled = true }
# "payments.ops.charge"            = { enabled = true }
# "payments.ops.refund"            = { enabled = true }
# "#;
// In production: `let (flags, _watcher) = Flags::<Meta>::watch_file("flags.toml")?;`
let flags: Flags<Meta> = Flags::from_toml_str(toml)?;

// GET /payment-methods: several queries on the same snapshot are consistent.
let snap = flags.snapshot();
for (key, r) in snap.children("payments.methods") {
    println!("{key}: {} enabled={} {:?}", r.meta.display_name, r.enabled, r.reason);
}

// POST /refunds: one `require` per dimension. The method comes from the user: validate it first.
let m = segment("paypal")?;
let err = flags.require(format!("payments.methods.{m}.refund")).unwrap_err();
assert_eq!(err.to_string(), r#""payments.methods.paypal.refund" is disabled: PayPal is not processing refunds today"#);
flags.require("payments.ops.refund")?;
# Ok(())
# }
```

A complete axum server —the three endpoints, error-to-HTTP mapping and hot reloading— is in
[`examples/axum.rs`](examples/axum.rs):

```text
cargo run --example axum
curl localhost:3000/payment-methods
curl -X POST localhost:3000/refunds -H 'content-type: application/json' -d '{"method":"paypal"}'
```

## Semantics

- Each entry has `enabled`; `reason` is required when it is disabled (it is shown to the end
  user); `meta` is optional and is deserialized by your type `M` (with the default `M = ()`, a
  `meta` is an error; `serde::de::IgnoredAny` accepts and ignores it). Any other field is an
  error.
- **Cascade without override**: a key is enabled only if it and all its declared ancestors are.
  `disabled_by` is the disabled ancestor closest to the root, and the exposed `reason` is its
  own.
- An undeclared intermediate segment (`payments.methods`) is neutral: it disables nothing and
  cannot be queried.
- Querying an undeclared key gives `Unknown`, never a silent `false`.
- The whole file is validated on load and on reload. If a reload fails, the previous snapshot
  stays in effect and `on_change` is not notified. There are no defaults at startup: a missing
  or invalid file is an error.
- Keys go **in quotes**: without them, TOML reads `payments.methods` as nested tables (and the
  load fails).

## Modeling guide

Key format: `<domain>.<dimension>.<value>[.<subswitch>]`. The library enforces the format
(`^[a-z0-9_]+(\.[a-z0-9_]+)*$`) and the `reason`; the rest are design rules.

1. **One root per domain.** Everything that should die with the domain goes under it.
2. **Each concept lives in a single place.** A provider exists only under `payments.methods`.
   Two branches are two sources of truth.
3. **Siblings of the same kind.** A node's children are all methods or all operations, never
   mixed, so that `children()` makes sense.
4. **The parent test.** "If I turn off the parent, can the child stay alive?" If the answer is
   "never", it is a child. If it has two natural parents (`paypal` and `refund`), it is a
   combination: place it by rule 5 and check the other parent with a second `require`.
5. **Combinations hang from the axis that changes most.** Methods get added and removed;
   operations hardly change. A new method is a self-contained block, and retiring a provider is
   deleting its block.
6. **Subswitches only when needed, and on every sibling.** If `.refund` exists on one method,
   it must exist on all of them: if it is missing on one, the key is unknown and fails closed.
7. **Names.** Dimension in plural (`methods`, `ops`), values in singular (`paypal`, `refund`),
   `snake_case`.
8. **Everything disabled has a `reason`.**

## Errors and HTTP

The mapping is up to the app. A suggestion:

| Error | Response |
|---|---|
| `InvalidSegment` | 400 |
| `Unknown` on a key built from user input | 400 (nonexistent method) |
| `Unknown` on any other key | 500 (configuration bug) |
| `Disabled` | 503 with the `reason` |

For `Disabled`, 503 and not 409 or 422: a retrying client treats those two as permanent and
gives up, when a kill switch is temporary by definition.

## Hot reloading (`watch` feature)

`Flags::watch_file` watches the file's **directory**, so it sees editors' atomic saves (temp +
rename) and the symlink swap of a Kubernetes `ConfigMap`. `Watcher::reload` forces a reload
(SIGHUP, admin endpoint); dropping the `Watcher` stops watching. The watcher runs on its own
thread: no async runtime needed. `Watcher` is `Send + Sync`, so it fits in axum's state.

- **A rejected reload keeps the previous snapshot in effect**: the file says one thing and the
  service does another. Record it with `Watcher::on_reject`; otherwise it only reaches the
  library's log, and a per-crate filter drops it (see [Observability](#observability)).
- **Save it atomically** (temp file + rename, like editors and `ConfigMap`s do): an in-place
  write can be read half-done. While running, that is a rejection that reaches `on_reject` and
  fixes itself when the write finishes; at startup, a failing `watch_file`. On Windows, the
  rename in `File.Move` (.NET), `os.replace` (Python) and `move` (cmd) fails with access denied
  if another process has the file open at that instant, even just to read it: the reload
  itself, or `poll_file` on every round. Retry, or use a rename with POSIX semantics (Rust's
  `std::fs::rename`). `Move-Item -Force` does not fail, but it is not atomic: it deletes and
  then moves.
- **Keep the file alone in its directory**: any change next to it rereads it, and `poll_file`
  hashes everything in it on every round.
- **Docker: mount the directory, not the file.** With a single-file bind mount, on Docker
  Desktop for Mac `watch_file` sees no change from the host, not even an in-place write; on a
  Linux host, an atomic save creates a new inode and the container keeps the old one forever,
  even when polling. With the directory mounted, on Docker Desktop for Mac (VirtioFS) events do
  cross: `watch_file` reloads, and only the file's deletion is lost, which does not reach
  `on_reject`. On Docker Desktop for Windows they do not cross: there, `Flags::poll_file`.
- **Do not replace the directory** (Linux, Windows). If a deployment deletes it and creates it
  again (`rm -rf` and copy, an `rsync --delete` of the parent), `watch_file` keeps watching the
  one that no longer exists and stops seeing changes. With a deployment's immediate recreation
  there is not even a rejection: the new file is applied and what is lost is the next edit.
  That is why, as soon as it happens, a `LoadError::Watch` reaches `on_reject`, **except on
  Windows if it is renamed** (`mv conf conf.old` and another in its place): the system follows
  the rename silently, and no notice arrives. There, `poll_file`, which finds it again. A
  `ConfigMap` does not have the problem: it swaps a symlink inside a directory that stays
  alive. Neither does macOS: FSEvents watches the path, finds the new directory and reloading
  continues, with no notice because there is nothing to report.
- **A symlink repointed above the directory** (`current -> releases/v2`) leaves reloading on
  the old one, on every platform: what is watched is what the path resolved to at startup. The
  `LoadError::Watch` arrives with that directory's next event, when something in it is touched
  or it is deleted; on macOS, deleting the whole release with the config in a subdirectory
  generates none. For those deployments, `poll_file`.

Each replica reloads on its own and they may differ while the change propagates: the listing
(`GET`) is informational and the operation's `require` (`POST`) is the authority. `revision()`
is a per-process counter: it is no use for comparing replicas. For that, `Snapshot::toml()`
returns the applied TOML verbatim, and a health check can use it in two ways:

- **Stale replica**: a file on disk that differs from `toml()` is a reload that was not
  applied. It also covers an event that never arrived (Docker Desktop for Windows with
  `watch_file`, a directory renamed on Windows, a symlink repointed until the old directory
  changes), which does not reach `on_reject` because there is nothing to reject. Tolerate a
  difference of a few hundred milliseconds: that is how long reloading takes. **It does not
  cover a single-file bind mount on a Linux host**: the disk the container sees is the same old
  inode, so it matches `toml()` even though the host already has another.
- **Matching replicas**: a hash of `toml()` in the health check, compared with that of the
  deployed file, computed outside the container. This is the check that also detects the
  previous case. With SHA-256 it is the same value as `sha256sum flags.toml`; the algorithm is
  the app's choice, not this library's.

## Observability

`on_change` receives a `Diff` for each applied reload, in revision order: keys added, removed,
with a different effective state (`changed`) or with a different cause (`reason_changed`).
Changes to `meta` are not included. A reload with no visible changes (a comment) also notifies,
with empty lists.

The library emits these `tracing` events:

| Event | Target | Level |
|---|---|---|
| Reload applied, with its revision | `breaker_panel::flags` | `info` |
| Reload rejected, with the chain of causes (line and column if the TOML does not parse) | `breaker_panel::watch` | `warn` |
| Event-based watching stopped: the directory was deleted or recreated (also sent to `on_reject`) | `breaker_panel::watch` | `warn` |
| `poll_file` with an interval under 100 ms, which is raised to 100 | `breaker_panel::watch` | `warn` |
| An `on_change` or `on_reject` callback panicked | `breaker_panel::flags` | `error` |
| `require` denied: **one per call**, so under load with a switch off it is one line per request | `breaker_panel::snapshot` | `debug` |

If your filter is per crate (`EnvFilter` with `my_app=info`), add `breaker_panel=info` or use
`on_reject`. To log a full `LoadError`, use `{:#}`: its plain `Display` only gives the top
level.

## Registered keys (`registry` feature)

```rust,standalone_crate
use breaker_panel::{Flags, flag_key};

flag_key!(CHARGE = "payments.ops.charge");

# fn main() -> Result<(), Box<dyn std::error::Error>> {
let flags: Flags = Flags::from_toml_str("[flags]\n\"payments.ops.charge\" = { enabled = true }")?;
flags.require(CHARGE)?;
# Ok(())
# }
```

Every key declared with `flag_key!` has to be in the file: if it is missing, startup fails, and
a reload that removes it is rejected. A malformed key (`"payments.Methods"`) does not even
compile. Dynamic keys (`format!`) are not validated at startup: if they do not exist, they give
`Unknown` at runtime.

The linker builds the registry per binary, from the crates it links: the keys of a dependency
the code never references are not registered. It is the crate's only global state, and it is
read-only. Consequence for your tests: every TOML they load has to include the keys registered
in that binary.

## When to use this and when not to

- **flagd in file mode** (`open-feature-flagd`) evaluates locally, reloads the file and brings
  targeting with JSONLogic and percentage rollouts. If you need that, use it.
- **LaunchDarkly, Unleash**: if you need the control plane —UI, auditing, permissions,
  analytics.
- **breaker-panel**: if what you want is a hierarchy with cascading and `disabled_by`, keys
  validated at startup, a minimal API with no state written at runtime, and a TOML file reviewed
  through PRs. Auditing, versioning and rollback are Git's; `on_change` notifies every applied
  change.

## License

Licensed under either of

- Apache License, Version 2.0 ([LICENSE-APACHE](LICENSE-APACHE) or
  <https://www.apache.org/licenses/LICENSE-2.0>)
- MIT license ([LICENSE-MIT](LICENSE-MIT) or <https://opensource.org/licenses/MIT>)

at your option.

### Contribution

Unless you explicitly state otherwise, any contribution intentionally submitted for inclusion in
the work by you, as defined in the Apache-2.0 license, shall be dual licensed as above, without
any additional terms or conditions.

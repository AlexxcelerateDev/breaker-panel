# Changelog

All notable changes to this crate are documented here. The format follows
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/), and the crate follows
[Semantic Versioning](https://semver.org/): while in `0.x`, a minor release is a breaking one.

## [Unreleased]

### Fixed

- The minimum `arc-swap` is now 1.9.5. Up to 1.9.4, `load` could return a value stored into a
  different instance when the allocator reused the address of a freed one
  ([vorner/arc-swap#210](https://github.com/vorner/arc-swap/issues/210)), which is the pattern
  `Flags` uses: `replace` stores new snapshots and `require` reads them with `load`.
- A panic in the `tracing` subscriber while it logged a panicking `on_change` callback left the
  thread marked as notifying, and every later `Flags::replace` or `Watcher::reload` from that
  thread panicked, blaming a callback that did not exist.

### Changed

- `Snapshot`'s `Debug`, and so `Flags`', no longer includes the TOML text next to the resolved
  flags: with `Flags` in the app state, a `?state` in a log wrote the whole file on every
  request.
- Minimum versions: `toml` 1.2.1 and `linkme` 0.3.38.
- docs.rs shows on each item which feature it needs.
- The `Flags` docs say that a thread's first query may allocate `arc-swap`'s per-thread state,
  once.

## [0.1.4] - 2026-10-08

### Changed

- The minimum `toml` is now 1.1.8. Its changes are in the serializer, which this crate does not
  enable.

## [0.1.3] - 2026-10-07

### Changed

- The minimum `toml` is now 1.1.7: it rejects duplicate keys in one more case and parses with
  fewer allocations.

## [0.1.2] - 2026-10-06

### Changed

- The MSRV is now 1.85, down from 1.98. No API or behavior change.

## [0.1.1] - 2026-10-06

### Fixed

- `TomlError`'s `Debug` no longer includes the whole input file: a `main` returning the error, or
  a `tracing::error!(?e)`, wrote it in one log line.

### Changed

- The docs describe `serde::de::IgnoredAny` as the `M` that loads any file and ignores `meta`,
  and say that the registry only covers the crates the binary links.

## [0.1.0] - 2026-10-05

First release: hierarchical kill switches from a local TOML file, with the cascade resolved at
load time, hot reloading (`watch` feature) and keys declared in code (`registry` feature).

[Unreleased]: https://github.com/AlexxcelerateDev/breaker-panel/compare/v0.1.4...HEAD
[0.1.4]: https://github.com/AlexxcelerateDev/breaker-panel/compare/v0.1.3...v0.1.4
[0.1.3]: https://github.com/AlexxcelerateDev/breaker-panel/compare/v0.1.2...v0.1.3
[0.1.2]: https://github.com/AlexxcelerateDev/breaker-panel/compare/v0.1.1...v0.1.2
[0.1.1]: https://github.com/AlexxcelerateDev/breaker-panel/compare/v0.1.0...v0.1.1
[0.1.0]: https://github.com/AlexxcelerateDev/breaker-panel/releases/tag/v0.1.0

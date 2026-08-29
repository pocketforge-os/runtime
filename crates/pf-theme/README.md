# pf-theme

`pf-theme` loads PocketForge's data-only theme packages, rejects a package as a
whole when any contract gate fails, resolves semantic scene style keys and motion
intents, and emits deterministic flattened CSS. `load_or_flagship` returns both
the typed rejection and the embedded Quiet Console fallback.

The files under `vendor/package` are copied verbatim from
`pocketforge-os/design/theme-quiet-console/package`. After a design change, run
`scripts/check-flagship-sync.sh /path/to/design`; update the vendor files and
`vendor/SOURCE.sha256` together. `vendor/tokens.generated.css` is the canonical
runtime transform of the design package's `tokens.json`; its first comment is the
only generator header.

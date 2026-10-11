# pf-problem-report

Shared device/server validator and deterministic CBOR codec for PocketForge
problem report v1. The normative schema, privacy rules, and cross-implementation
fixtures are in the platform repository at `schemas/problem-report-v1/`.

The public API accepts the documented JSON mapping: `encode` validates and
returns canonical wire bytes, while `decode` refuses non-canonical or invalid
input and returns the JSON mapping. Every refusal carries one of the stable
typed reasons from the schema contract.

Tests require a real platform checkout. Set `PF_PLATFORM_DIR` to its root; a
sibling `platform` checkout is used when the variable is absent. Missing
fixtures are a test failure, never a skip.

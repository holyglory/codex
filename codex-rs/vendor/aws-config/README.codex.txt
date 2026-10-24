aws-config 1.8.12 local cryptographic dependency update

This directory preserves the Apache-2.0 upstream source and complete feature set.
CODEX_PROVENANCE.json identifies the checksum-pinned crates.io archive, original
file hashes, upstream test fixtures, and authenticated validation evidence.
CODEX_CHANGES.patch records the source and manifest changes against that archive.

Local changes upgrade p256 to 0.14 and rand to 0.10 for the production dependency
graph. Login DPoP uses ToSec1Point and seeds its random generator from SysRng,
propagating operating-system entropy errors. No login feature is disabled.

The crate has a separate workspace so its upstream development dependencies do
not enter Codex's production graph. Its Cargo.lock preserves reproducible local
tests. The restored test-data files are upstream synthetic fixtures, not machine
credentials. Run the upstream suite with:
  cargo +1.95.0 test --locked --manifest-path codex-rs/vendor/aws-config/Cargo.toml --lib

Remove this local patch when an upstream aws-config release provides the updated
cryptographic dependencies, after retaining the login and compatibility checks.

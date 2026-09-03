# npm releases

Use the staging helper in the repo root to generate npm tarballs for a release. For
example, to stage the downstream CLI package for version `0.6.0+multi.1`:

```bash
./scripts/stage_npm_packages.py \
  --release-version 0.6.0+multi.1 \
  --package codex
```

This downloads the required native package archive artifacts, hydrates `vendor/` for
each package, and writes tarballs to `dist/npm/`.

When `--package codex` is provided, the staging helper builds the lightweight
`@holyglory/codex` meta package plus all platform-native `@holyglory/codex` variants
that are later published under platform-specific dist-tags.

Direct `build_npm_package.py` invocations are still useful for package-specific
debugging, but native packages expect `--vendor-src` to point at a prehydrated
`vendor/` tree. Release packaging should use `scripts/stage_npm_packages.py`.

The downstream release workflow is `.github/workflows/downstream-npm-release.yml`.
An artifact-only run builds and verifies all six native payloads plus the root
wrapper without contacting npm. Once the package has a stage-only trusted
publisher, dispatch the same workflow from an immutable `npm-v<version>` tag
with `stage_to_npm` enabled. It submits platform payloads first and the root
`latest` wrapper last. A maintainer must still approve the staged versions
through npm with 2FA.

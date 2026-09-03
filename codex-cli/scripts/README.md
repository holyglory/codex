# npm releases

Use the staging helper in the repo root to generate npm tarballs for a release. For
example, to stage the downstream CLI package for version `0.6.0-multi.1`:

```bash
./scripts/stage_npm_packages.py \
  --release-version 0.6.0-multi.1 \
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

`.github/workflows/downstream-candidate.yml` builds and verifies all six native
payloads plus the root wrapper without npm authority. It uploads the seven
tarballs as candidate artifacts only.

Publication is isolated in `.github/workflows/downstream-npm-publish.yml`. It
accepts only an annotated `npm-v<version>` tag at the exact commit tested by the
selected candidate run. Its `stage_to_npm` input defaults to false, and the
stage job additionally requires approval through the protected `npm` GitHub
environment before OIDC credentials are issued. It submits platform payloads
first and the root `latest` wrapper last; staged versions still require the npm
approval step before becoming public.

Rust release versions retain truthful build metadata such as
`0.153.0+multi.1`. npm strips SemVer build metadata, so the workflow uses the
collision-safe equivalent `0.153.0-multi.1` and requires the annotated tag
`npm-v0.153.0-multi.1` for any future publication run.

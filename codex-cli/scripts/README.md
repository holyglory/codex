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

Staged trusted publishing requires each package name to exist first and the npm
trust relationship to allow `stage publish` without allowing unattended direct
publication. Bootstrap and trust configuration are separate release operations;
the candidate workflow never performs them.

For the first release, bootstrap the package name with the verified Linux x64
platform tarball under the `linux-x64` tag, using the operator's interactive
2FA-protected npm session. Do not publish a placeholder or the root `latest`
launcher before its platform dependencies are available. Then configure the
approved stage-only trust relationship and run the protected workflow. It
checks every tarball before staging, skips the bootstrap only when its public
SHA-512 integrity and dist-tag exactly match, and refuses conflicting versions
or registry errors. Remaining platforms are staged first and the root last;
approve platform stages before approving the root stage. An already-staged but
unapproved version is not silently replaced by this helper.

Rust release versions retain truthful build metadata such as
`0.153.0+multi.1`. npm strips SemVer build metadata, so the workflow uses the
collision-safe equivalent `0.153.0-multi.1` and requires the annotated tag
`npm-v0.153.0-multi.1` for any future publication run.

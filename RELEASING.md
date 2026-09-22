# Releasing

`oxinsider` is published to crates.io by `.github/workflows/publish.yml` when a
GitHub release is published, through crates.io trusted publishing: no API token
is stored anywhere.

## One-time setup

crates.io accepts a trusted publisher only for a crate that already exists, so
the first version is published by hand.

1. On crates.io, create an API token with the `publish-new` scope, limited to
   the crate name `oxinsider`.
2. From a clean checkout of `main` at the commit you are releasing:

   ```sh
   cargo publish --dry-run
   cargo publish            # asks for the token, or: CARGO_REGISTRY_TOKEN=... cargo publish
   git tag v0.1.0 && git push origin v0.1.0
   ```

3. On crates.io, open the crate's Settings, Trusted Publishing, and add a GitHub
   publisher: owner `0xinsider`, repository `0xinsider-rust`, workflow
   `publish.yml`, environment `crates-io`.
4. Revoke the token from step 1.
5. In this repository's Settings, Environments, create `crates-io`. Add a
   required reviewer if a release should need a second approval; the trusted
   publisher entry already names the environment.

Renaming `publish.yml` or the environment breaks publishing until the crates.io
entry is updated to match.

## Every release

1. Regenerate if the contract moved (`python3 scripts/generate.py`, or merge the
   weekly regenerate pull request), then bump `version` in `Cargo.toml`. While
   the crate is `0.x`, a regenerate that renames or removes a type is a minor
   bump; one that only adds fields, values or operations is a patch.
2. Merge to `main` with CI green.
3. Dispatch `Publish to crates.io` with `dry_run` checked to rehearse, then
   publish a GitHub release whose tag is `v<version>`. The workflow checks the
   tag against `Cargo.toml`, runs the tests, packages the crate, exchanges its
   OIDC token for a short-lived crates.io token and publishes. A version already
   on crates.io is skipped, so re-running a finished release is harmless.
4. Check `https://crates.io/crates/oxinsider` and `https://docs.rs/oxinsider`.

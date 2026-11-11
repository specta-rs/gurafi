# Contributing

## Development

```bash
cargo check --workspace --all-targets
cargo test --workspace
```

Documentation lives in [`docs/`](docs) as plain Markdown, starting at
[`docs/README.md`](docs/README.md).

## Releasing

Publishing uses crates.io trusted publishing: the `release` workflow exchanges
its GitHub OIDC identity for a short-lived crates.io token, so **no
`CARGO_REGISTRY_TOKEN` is stored** in this repository or in Actions secrets.

**One-time setup** — a crate owner adds a trusted publisher in the crates.io UI
(the crate's **Settings → Trusted Publishing**) for every published crate. These
values must match `.github/workflows/release.yml` exactly, or the token exchange
is refused:

| Field | Value |
| --- | --- |
| Owner | `specta-rs` |
| Repository | `gurafi` |
| Workflow filename | `release.yml` |
| Environment | `release` |

The workflow publishes the whole workspace, so each of these crates needs its
own trusted publisher: `gurafi`, `gql`, `gurafi-macros`, `gurafi-relay` and `suku`.
The trusted publisher and "Require trusted publishing" setting are already configured for this
repository; repeat the setup only when adding a new crate.
Examples are `publish = false` and are skipped.

**Cutting a release:**

1. Bump the `version` of every published workspace crate and merge to `main`.
   The tag names the `gurafi` crate version.
2. Tag the merge commit and push the tag:

   ```bash
   git tag v0.1.1
   git push origin v0.1.1
   ```

The `release` workflow then verifies that the tag matches `gurafi`'s version,
builds and tests the workspace, publishes every crate in dependency order, and
creates the GitHub release.

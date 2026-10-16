# Contributing to pghacker-mcp

This project welcomes contributions and suggestions. Most contributions require you to agree to a
Contributor License Agreement (CLA) declaring that you have the right to, and actually do, grant us
the rights to use your contribution. For details, visit [Contributor License Agreements](https://cla.opensource.microsoft.com).

When you submit a pull request, a CLA bot will automatically determine whether you need to provide
a CLA and decorate the PR appropriately (e.g., status check, comment). Simply follow the instructions
provided by the bot. You will only need to do this once across all repos using our CLA.

This project has adopted the [Microsoft Open Source Code of Conduct](https://opensource.microsoft.com/codeofconduct/).
For more information see the [Code of Conduct FAQ](https://opensource.microsoft.com/codeofconduct/faq/) or
contact [opencode@microsoft.com](mailto:opencode@microsoft.com) with any additional questions or comments.

## Issues

Search the existing issues before you open one. For a bug, give the steps that reproduce it, what you expected and
what happened, and the versions involved: the commit, your OS, and, for a deployment, its Terraform settings without
secrets. Propose a new feature in an issue before you build it, so that we can agree on its design first.

Don't report security vulnerabilities in issues: follow [SECURITY.md](SECURITY.md).

## Development

The [README](README.md#quick-start) sets up a local copy: Node.js 24 or later, the Rust toolchain in
`rust-toolchain.toml`, and Docker. The [reference](docs/reference.md) describes the settings, the tools and the
repository's layout.

Before you open a pull request, run the checks for what you changed:

| What changed | Checks |
| --- | --- |
| Anything | `npm test`: the launcher, configuration, routes and the home page's unit tests |
| Rust | `npm run rust:check`: rustfmt, clippy with warnings as errors, and the Rust tests, which need the local PostgreSQL that `npm start` runs |
| The home page (`home/`) | `npm --prefix home test` and `npm --prefix home run build` |
| Terraform (`deploy/terraform/`) | In `deploy/terraform`: `terraform init -backend=false -input=false -lockfile=readonly`, then `terraform fmt -recursive -check`, `terraform validate` and `terraform test` |

## License headers

Every source file starts with this header, in its language's comment syntax, after any `#!` line, Dockerfile parser
directive or HTML doctype:

```text
Copyright (c) Microsoft Corporation.
Licensed under the MIT license.
```

`npm run license-headers` lists the files that lack it, and `npm run license-headers -- --fix` adds it to them.
[scripts/lib/license-headers.mjs](scripts/lib/license-headers.mjs) lists the file types that take it; `vendor/`
doesn't. A migration's checksum leaves the header out, so adding it to a migration that a database has applied is safe.

## Third-party code

`vendor/` holds third-party code under its own license: keep its license files, don't add the header to it, and don't
reformat it. [NOTICE](NOTICE) describes each piece and its license. Before you add third-party code, open an issue:
its license must allow its use here, and it needs a NOTICE entry.

## Pull requests

Keep a pull request to one change, with tests that cover it, and update the documentation it affects. Describe
what changed and why, and how you checked it. A maintainer reviews it, and may ask for changes before merging.

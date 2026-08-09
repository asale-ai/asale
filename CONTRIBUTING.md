# Contributing

Thanks for looking. Asale is a Tauri desktop client: a Rust core and daemon, a TypeScript front end, and the `asale` command line. Bug reports, fixes and features are all welcome.

## Before you start

Architecture, local development, packaging and the release process live in [docs/DEVELOPMENT.md](docs/DEVELOPMENT.md); the command line is documented in [docs/CLI.md](docs/CLI.md). Read those first — the build has prerequisites (Rust, Node 22, pnpm 11, and webkit2gtk on Linux), and the packaging scripts are shared with CI, so a build that works locally works there too.

For anything larger than a bug fix, open an issue first and describe what you intend to do. Disagreeing about an approach in an issue is much cheaper than disagreeing about it in a finished pull request.

## Reporting bugs

Open an issue with the bug report form. Include the client version, your OS, which upstream provider was involved, and the steps that reproduce the problem. Logs help — `asale logs` prints them, and request bodies are never logged — but read through them for credentials before pasting anything in public.

Security problems do not belong in public issues. See [SECURITY.md](SECURITY.md).

## Pull requests

Keep a pull request to one topic. A small, obviously correct change gets merged quickly; a large one that mixes a refactor with a behaviour change does not.

Commit messages follow Conventional Commits — `feat(client): …`, `fix(daemon): …`, `docs: …`, `ci(release): …`. Release notes are generated from them, so write the subject for someone skimming a changelog.

Code and commit messages are in English. Run `cargo fmt` and `cargo clippy` for Rust, and the repository's formatter and type check for TypeScript, before you push.

Say in the description how you tested the change and on which platform. Bundles behave differently on macOS, Windows and Linux, and a maintainer cannot always reproduce all three.

## Releases

A release is cut from a `v*` tag and built by [.github/workflows/release.yml](.github/workflows/release.yml). The workflow produces a **draft**; it is published by hand after the checklist in the run summary has been worked through.

Tagging is deliberate rather than automatic. Batch changes up and cut a release when there is something worth telling people about — as a rule of thumb at most one a week, security and crash fixes excepted. Every published tag notifies every watcher and replaces the download links, so a stream of near-identical versions spends attention without buying anything. Merged work waits on `main` until then.

## Code of conduct

Taking part in this project means agreeing to the [Code of Conduct](CODE_OF_CONDUCT.md).

## Licence

Contributions are accepted under the Apache License 2.0, the same licence as the project.

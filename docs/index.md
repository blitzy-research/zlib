# blitzy-zlib

> **This directory is not the published documentation root.** The site is built
> from `doc/`, because `mkdocs.yml` sets `docs_dir: doc`. Start at
> **[`doc/index.md`](../doc/index.md)** — that page is canonical.

*zlib-rs — a memory-safe, idiomatic Rust rewrite of the zlib 1.3.2.1 compression
library with a C-compatible FFI drop-in layer.*

## Where the documentation actually lives

MkDocs resolves every `nav` entry relative to `docs_dir`, so the three published
pages are `doc/index.md`, `doc/project-guide.md`, and
`doc/technical-specifications.md`. Backstage TechDocs builds through that same
`mkdocs.yml` — `catalog-info.yaml` sets `backstage.io/techdocs-ref: dir:.` — so
it publishes `doc/` too. No file under `docs/` is reachable from either build.

| Start here | What it covers |
| --- | --- |
| [`doc/index.md`](../doc/index.md) | The canonical documentation landing page. |
| [`doc/project-guide.md`](../doc/project-guide.md) | Setup, build, test, lint, and benchmark commands, plus the risk assessment and module architecture. |
| [`doc/technical-specifications.md`](../doc/technical-specifications.md) | The full migration technical specification: target design and module layering, the file-by-file transformation mapping, the design patterns applied, the unsafe-boundary and memory-ownership analysis, and the dependency inventory. |
| [`README.md`](../README.md) | The crate-facing overview: installation, the idiomatic Rust and C drop-in usage guides, the feature-flag table, and the build and test commands. |

## Why this page is kept

This page previously carried its own description of the project. That duplicated
the canonical landing page, and the two copies had already drifted apart — the
defect this convergence resolves.

It is deliberately retained rather than removed, so that anyone arriving at the
conventional `docs/` path — from a tool, a stale link, or habit — is redirected
instead of meeting a dead end. It just as deliberately carries no project detail
of its own: with nothing duplicated here, there is nothing left to drift.

Documentation changes therefore belong in `doc/`, alongside the page they
affect. Please do not restore project substance to this file.

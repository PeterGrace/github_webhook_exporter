# Diataxis documentation site

## Problem

Documentation lived in a handful of long, mixed-purpose files — `README.md`, `docs/operations.md`,
`RELEASE.md` — that each blended task instructions, exhaustive contracts, and design rationale
together. There was no GitHub Pages presence, and finding "how do I do X" versus "what exactly does
Y return" meant scanning the same long document either way.

## Change

Added an mdBook site under `book/`, organized by the Diataxis framework:

- **Tutorials**: one end-to-end walkthrough (`tutorials/getting-started.md`) that runs the exporter
  in Docker, registers a repository, hand-signs and delivers a real webhook, and confirms it lands
  in `/metrics`.
- **How-to guides**: deploying with Helm, upgrading a running deployment, backing up and restoring
  SQLite, validating the Helm package, configuring remote telemetry, and releasing a new version.
- **Reference**: environment variables, the HTTP API, metrics, traces, the remote telemetry export
  pipeline, the container image, release and packaging (see below), startup/retention/shutdown, and
  a map into the chart's `values.yaml`.
- **Explanation**: architecture (why a converter-not-a-store, why SQLite implies a singleton) and
  design decisions (why payloads are never persisted, why identifiers are span-only rather than
  metric labels, why merge-group and pull-request queue statistics stay independent, why workflow
  traces use an unrelated trace identity).

`.github/workflows/docs.yml` builds and deploys the book to GitHub Pages on pushes to `main` that
touch `book/**`.

`docs/operations.md` and `RELEASE.md` are now short pointers into the book, kept only so links from
past changelog entries and design documents keep resolving. `README.md` keeps its quick start and
links out to the book instead of repeating the configuration, HTTP API, and metrics tables.
`charts/github-webhook-exporter/README.md` and `docs/build-spec.md` had their one inbound link each
retargeted.

### The release-workflow contract test

`scripts/github-actions-test.sh` asserts that specific prose fragments — the GHCR immutability
rules, the image/chart publication state matrix, the Helm validation command set — literally exist
in `docs/operations.md`, so that document can't drift from what `.github/workflows/helm-package-ci.yml`
actually does. That content, plus the four `Cargo.toml` `pre-release-replacements` entries that kept
its pinned version examples current, moved to `book/src/reference/release-and-packaging.md` as one
merged reference page (it doesn't split along the how-to/reference line as cleanly as everything
else, because the two originally-separate operations.md sections share required fragments). Both
the test's file paths and the `pre-release-replacements` entries were updated to match; two new
`pre-release-replacements` entries were added for the tutorial's pinned image tag and the "deploy
with Helm" guide's `--version` example, so `cargo release` keeps every pinned version in the book
current the same way it already did for the README and chart README.

## Verification

- `just workflow-test` and `just release-version-test` pass against the new file paths.
- `mdbook build book` succeeds with no warnings.
- Every internal link and in-page anchor in `book/src` resolves (checked mechanically against
  mdBook's heading-slug algorithm).
- A Python re-implementation of `require_fragment`'s whitespace-normalized substring match confirms
  every fragment `scripts/github-actions-test.sh` checks for is present in
  `book/src/reference/release-and-packaging.md`.

## Files

- `book/` — new: `book.toml`, `book/src/SUMMARY.md`, and 19 content pages across the four sections.
- `.github/workflows/docs.yml` — new: build and deploy to GitHub Pages.
- `Cargo.toml` — `pre-release-replacements` retargeted from `docs/operations.md` to
  `book/src/reference/release-and-packaging.md`; two entries added for the book's other pinned
  version examples.
- `scripts/github-actions-test.sh` — fragment checks retargeted to the new file path.
- `docs/operations.md`, `RELEASE.md` — replaced with pointers to the book.
- `README.md` — trimmed configuration/HTTP API/metrics tables and the Helm value-group table in
  favor of links; releasing and documentation sections point at the book.
- `charts/github-webhook-exporter/README.md`, `docs/build-spec.md` — one inbound link each
  retargeted to the book.

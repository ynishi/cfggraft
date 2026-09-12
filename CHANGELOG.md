# Changelog

All notable changes to this project are documented here. The format follows
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/), and the project
adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

`cfggraft` and `cfggraft-cli` are versioned together and released as one.

## [0.1.1] - 2026-09-12

### Fixed

- `region` now records its digest when it adopts a span that already holds the
  declared body. The decision in that case is a skip, and the command line
  decided whether to write the file by asking the report what had changed — so
  the digest was rendered and then thrown away. A region adopted rather than
  created by `--init` therefore never became claimable, and the hand-edit check
  that the markers exist for never activated. The file is now written whenever
  the rendered document differs from what was read, which is also what the
  "leave the file alone when nothing changed" promise actually meant.

## [0.1.0] - 2026-09-12

First release.

### Added

- `merge`: manage items of a JSON document, addressed by dotted key path. A
  declaration expands into one item per leaf, so each key is decided separately
  and a disagreement about one never blocks the rest.
- `region`: manage a marker-delimited span of a text file as a single item,
  with the provenance digest recorded inline in the begin marker.
- Three-way decision over what is declared, what this tool wrote on its last
  run, and what is in the target now. A value that is provably ours can be
  updated or retracted; a value that is not is left in place and reported.
- Arrays are treated as sets whose elements are addressed by value rather than
  by index, so a withdrawn element can be found and removed instead of
  accumulating, and an element the other program added is never touched.
- `--retract` to remove items that are no longer declared, off by default.
- `--force` to overwrite values the tool cannot claim.
- `--check` for a read-only drift check, `--verbose` to list settled items.
- Provenance for JSON targets in `$XDG_STATE_HOME/cfggraft/priors.json`,
  overridable with `--prior-store`. Losing it degrades the run to add-only
  rather than losing data.
- Atomic in-place writes: a temporary file in the same directory, then a
  rename, with the original mode carried over.
- Exit status 2 reserved for drift alone, distinct from failure (1).

[0.1.1]: https://github.com/ynishi/cfggraft/releases/tag/v0.1.1
[0.1.0]: https://github.com/ynishi/cfggraft/releases/tag/v0.1.0

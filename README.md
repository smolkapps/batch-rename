# batch-rename

A safe, composable batch file renamer for the command line. Rename many files at
once with regex substitutions, literal replacements, prefixes/suffixes, case
folding, extension changes, and sequential numbering with templates.

> ## ⚠️ Dry-run is the default
>
> By default `batch-rename` **changes nothing**. It prints the planned
> `old -> new` table and exits. To actually perform the renames you must pass
> **`--commit`** (or its short alias **`-y`**). This is deliberate: a batch
> rename is easy to get wrong, so you always see the plan first.

## Why

- **Safe by default** — dry-run unless you opt in with `--commit`.
- **Never silently overwrites** — colliding renames (two files → one name, or a
  target that already exists) are detected and **refused** with a non-zero exit
  code. Opt into `--collision suffix` to auto-disambiguate with ` (1)`, ` (2)`…
- **Composable rules** — stack regex, replace, prefix/suffix, case and extension
  changes in one pass.
- **Pure planner core** — the rename plan is computed by a pure function and is
  thoroughly unit-tested; the binary is a thin shell that applies it.

## Install

```sh
cargo build --release
# binary at target/release/batch-rename
```

## Usage

```
batch-rename [OPTIONS] <PATH_OR_GLOB>...
```

Inputs are file paths and/or a glob (quote globs so your shell doesn't expand
them first, e.g. `'*.jpeg'`). Use `--recursive` to descend into directories.

### Options

| Flag | Description |
|------|-------------|
| `--regex 's/PAT/REPL/'` | sed-style substitution on the file name. `$1`, `$2` = capture groups. **Repeatable**, applied in order. |
| `--replace FROM TO` | Replace every literal `FROM` with `TO`. Repeatable. |
| `--prefix STR` | Prepend `STR` to the stem. |
| `--suffix STR` | Append `STR` to the stem (**before** the extension). |
| `--lower` / `--upper` | Lower/upper-case the file name. |
| `--ext NEWEXT` | Replace the extension (no leading dot; empty removes it). |
| `--seq` | Sequential numbering; exposes the `{n}` template token. |
| `--start N` | First sequence number (default `1`). |
| `--pad W` | Zero-pad the number to width `W` (e.g. `3` → `001`). |
| `--template '{name}_{n}.{ext}'` | Template for the whole new name. Tokens: `{name}`, `{ext}`, `{n}`. |
| `--recursive` | Walk directories given as inputs. |
| `--collision refuse\|suffix` | On collision: `refuse` (default) or `suffix` (append ` (1)`). |
| `--commit`, `-y` | **Apply** the renames. Without this it's a dry-run. |

### Rule order

When several rules are combined they apply in this fixed, documented order:

1. `--regex` substitutions (in the order given)
2. `--replace` literal substitutions
3. `--prefix`, then `--suffix`
4. `--lower` / `--upper`
5. `--ext`
6. `--template` (defines the entire final name, last)

## Examples

Preview renaming camera files (dry-run — nothing changes):

```sh
batch-rename --regex 's/IMG_(\d+)/photo_$1/' '*.jpg'
```

Actually do it:

```sh
batch-rename --regex 's/IMG_(\d+)/photo_$1/' --commit '*.jpg'
```

Lowercase everything and normalise `.JPEG` → `.jpg`, recursively:

```sh
batch-rename --lower --ext jpg --recursive --commit ./photos
```

Replace spaces with underscores:

```sh
batch-rename --replace ' ' '_' --commit '*.txt'
```

Add a prefix and a version suffix (suffix lands before the extension):

```sh
batch-rename --prefix '2026_' --suffix '_v1' --commit report.md
# report.md -> 2026_report_v1.md
```

Sequentially number a set with zero-padding and a template:

```sh
batch-rename --seq --start 1 --pad 3 --template 'shot_{n}.{ext}' --commit a.png b.png c.png
# a.png -> shot_001.png, b.png -> shot_002.png, c.png -> shot_003.png
```

Auto-disambiguate instead of refusing on collision:

```sh
batch-rename --regex 's/^.*\.txt$/note.txt/' --collision suffix --commit *.txt
# note.txt, note (1).txt, note (2).txt, ...
```

## Safety details

- **Collision = refuse.** If two sources would land on the same target, or a
  target already exists on disk (and isn't itself being renamed away in the same
  batch), the whole operation is refused with exit code `2` and nothing is
  touched. The intended plan is still printed so you can see what went wrong.
- **Swaps are safe.** A batch that renames `a -> b` and `b -> a` is applied via
  temporary staging names so no file is lost.
- **Extensions are respected.** Stem-only operations (prefix, suffix, case of the
  stem) never accidentally eat the extension; `--ext` changes it explicitly.

## Exit codes

| Code | Meaning |
|------|---------|
| `0` | Success (including a dry-run that printed a plan). |
| `1` | Usage / argument / I/O error. |
| `2` | Collision detected under the default `refuse` policy. |

## Development

```sh
cargo test            # unit tests on the planner + integration tests on the CLI
cargo build --release
```

## License

MIT — see [LICENSE](LICENSE).

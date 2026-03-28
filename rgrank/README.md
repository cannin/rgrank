# rgrank

`rgrank` is a single Rust binary that wraps ripgrep's crates instead of forking ripgrep itself.

The query is interpreted as a regex by default, like `rg`. Use `-F` or `--fixed-strings` for the ranked literal-term mode.

It uses:

- `ignore` for recursive walking with `.gitignore` and `.ignore` support
- `grep-regex` for regex and fixed-string matching
- `grep-searcher` for efficient line collection

Ranking is file-level and combines:

- BM25-like term weighting
- query coverage bonuses
- filename boosts
- phrase bonuses
- nearby-match proximity bonuses

Build and run:

```bash
. "$HOME/.cargo/env"
cargo build --release
./target/release/rgrank 'timeout|config' /path/to/codebase
```

By default, `rgrank` now prints rg-style output. Ranked output is still available behind `--ranked`.

Default rg-style examples:

```bash
./target/release/rgrank 'timeout|config' .
./target/release/rgrank -n 'timeout|config' .
./target/release/rgrank --column 'timeout|config' .
./target/release/rgrank --heading 'timeout|config' .
./target/release/rgrank --json 'timeout|config' .
./target/release/rgrank -B1 -A2 'timeout|config' .
```

Ranked examples:

```bash
./target/release/rgrank --ranked --top-k 10 --context 3 --max-snippets 2 "timeout config" .
./target/release/rgrank --ranked --all "timeout config" .
./target/release/rgrank -i -g '*.rs' -tpy 'timeout|config' .
./target/release/rgrank -w 'python' .
./target/release/rgrank -x '^python$' .
./target/release/rgrank -l 'timeout|config' .
./target/release/rgrank -L 'timeout|config' .
./target/release/rgrank -c 'timeout|config' .
./target/release/rgrank --hidden --no-ignore "timeout config" .
./target/release/rgrank 'whisper|process_all_mp4s|python' .
./target/release/rgrank -F "cortisol level" .
./target/release/rgrank --files ./hypotheses
```

`--files` switches `rgrank` into path-listing mode similar to `rg --files`. In that mode it does not require a query and prints one file path per line while still respecting `.gitignore`, hidden-file, and symlink settings.

Common `rg`-style flags supported now:

- `-i`, `-s`, `-S` for case mode control
- `-g` for include/exclude globs
- `-t` and `-T` for file type filtering
- `-w` and `-x` for word and whole-line matching
- `-A`, `-B`, and `-C` for rg-style context output
- `-n`, `--column`, `--heading`, `--no-heading`, `--json`, and `--color`
- `-l`, `-L`, and `-c` for file-list and count output modes
- `--follow-links` for symlink traversal

# Output modes

Standard output is the default. It behaves like `rg` for the common text modes:

- single-file searches print just matching lines by default
- directory searches print `path:line` style prefixes as needed
- `-A`, `-B`, and `-C` use rg-style `:` for matches and `-` for context lines
- standard output defaults to zero context
- `--json` emits an `rg`-style event stream with `begin`, `context`, `match`, `end`, and `summary`

Ranked output is enabled with `--ranked`. Ranked mode keeps the scoring report and defaults to 2 lines of snippet context around each chosen hit.

Use `--all` with `--ranked` to disable both ranked caps:

- no `--top-k` truncation
- no `--max-snippets` truncation

## Ranked example

Example:

```text
./hypotheses/pmc_hypotheses/23055545_hypothesis.json
  score=8.862 matched_terms=2/2 match_lines=9
  snippet 7-13 score=2.800
       7 | ...
      13 | ...
```

What this means:

- `score=8.862` is the total file-level rank score.
- `matched_terms=2/2` means both query terms were found in this file.
- `match_lines=9` means 9 matched lines from this file were collected for scoring.
- `snippet 7-13` means the displayed excerpt covers file lines 7 through 13.
- `snippet ... score=2.800` is the local relevance score for that excerpt only, not the whole file.

Snippet scores come from the matched lines inside that excerpt and are used to pick the best passages to show. File scores are larger because they combine all matches in the file plus filename, phrase, coverage, and proximity bonuses.

# Other examples

```bash
./rgrank/target/release/rgrank --ranked "cortisol level" ./hypotheses
./rgrank/target/release/rgrank --files ./hypotheses
```

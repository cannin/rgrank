# rgrank

`rgrank` is a single Rust binary that wraps ripgrep's crates instead of forking ripgrep itself.

The query is interpreted as a regex by default, like `rg`. Use `-F` or `--fixed-strings` for the ranked literal-term mode.

It uses:

- `ignore` for recursive walking with `.gitignore` and `.ignore` support
- `grep-regex` for literal query-term matching
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
./target/release/rgrank "timeout config" /path/to/codebase
```

Useful flags:

```bash
./target/release/rgrank --top-k 10 --context 3 --max-snippets 2 "timeout config" .
./target/release/rgrank --hidden --no-ignore "timeout config" .
./target/release/rgrank 'whisper|process_all_mp4s|python' .
./target/release/rgrank -F "cortisol level" .
./target/release/rgrank --files ./hypotheses
```

`--files` switches `rgrank` into path-listing mode similar to `rg --files`. In that mode it does not require a query and prints one file path per line while still respecting `.gitignore`, hidden-file, and symlink settings.

# Output format

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
./rgrank/target/release/rgrank "cortisol level" ./hypotheses
./rgrank/target/release/rgrank --files ./hypotheses
```

# rgrank

`rgrank` is a single Rust binary that wraps ripgrep's crates instead of forking ripgrep itself.

The query is interpreted as a regex by default, like `rg`. Use `-F` or `--fixed-strings` for the ranked literal-term mode.

It uses:

- `ignore` for recursive walking with `.gitignore` and `.ignore` support
- `grep-regex` for regex and fixed-string matching
- `grep-searcher` for efficient line collection
- in-process extractors for Office documents, PDFs, and archives

Ranking is file-level and combines:

- BM25-like term weighting
- query coverage bonuses
- filename boosts
- phrase bonuses
- nearby-match proximity bonuses

## BM25-like ranking

`rgrank` does not build a persistent index. In ranked mode it first collects matching lines, groups them by file, and then assigns each file a score.

The core of the file score is BM25-like:

- each distinct query term is scored separately
- term frequency is based on exact hits in collected match text
- partial token hits count less than exact hits
- filename/path hits count, but with a reduced weight
- inverse document frequency is computed across the matched candidate files in the current search
- document length normalization uses the amount of collected matched text for that file

In simplified form, the per-term part is:

```text
tf = exact_hits
   + partial_hits * 0.35
   + path_hits * 0.75

idf = ln(((total_docs - doc_freq + 0.5) / (doc_freq + 0.5)) + 1.0)

term_score = idf * (tf * (k1 + 1)) /
             (tf + k1 * (1 - b + b * doc_len / avg_doc_len))
```

with:

- `k1 = 1.2`
- `b = 0.75`

That BM25-like core is then adjusted with extra retrieval heuristics:

- coverage bonus: files matching more distinct query terms rank higher
- all-terms bonus: files matching every query term get an extra bump
- path term bonus: files whose path contains a query term get additional weight
- phrase bonus: exact normalized query phrase in the file gets a larger boost
- path phrase bonus: the phrase in the path gets a smaller boost
- same-line bonus: multiple query terms on the same line help
- proximity bonus: files where all terms occur within a tighter line span rank higher
- match-density bonus: more matched lines help, but only sublinearly

Important limits of this approach:

- IDF is computed over the current candidate set, not the whole repository
- document length is based on collected matched lines, not the full original file
- ranking is strongest for word-like queries and `-F`; very complex regex patterns still match correctly, but their ranking semantics are only approximate

Snippet ranking is separate from file ranking. The file score chooses which files rank highest. Snippet scores choose which local passages to show inside each ranked file.

Build and run:

```bash
. "$HOME/.cargo/env"
cargo build --manifest-path rgrank/Cargo.toml --release
./rgrank/target/release/rgrank 'timeout|config' /path/to/codebase
```

By default, `rgrank` now prints rg-style output. Ranked output is still available behind `--ranked`.

Default rg-style examples:

```bash
./rgrank/target/release/rgrank 'timeout|config' .
./rgrank/target/release/rgrank -n 'timeout|config' .
./rgrank/target/release/rgrank --column 'timeout|config' .
./rgrank/target/release/rgrank --heading 'timeout|config' .
./rgrank/target/release/rgrank --json 'timeout|config' .
./rgrank/target/release/rgrank -B1 -A2 'timeout|config' .
```

Ranked examples:

```bash
./rgrank/target/release/rgrank --ranked --top-k 10 --context 3 --max-snippets 2 "timeout config" .
./rgrank/target/release/rgrank --ranked --all "timeout config" .
./rgrank/target/release/rgrank -i -g '*.rs' -tpy 'timeout|config' .
./rgrank/target/release/rgrank -w 'python' .
./rgrank/target/release/rgrank -x '^python$' .
./rgrank/target/release/rgrank -l 'timeout|config' .
./rgrank/target/release/rgrank -L 'timeout|config' .
./rgrank/target/release/rgrank -c 'timeout|config' .
./rgrank/target/release/rgrank --hidden --no-ignore "timeout config" .
./rgrank/target/release/rgrank 'whisper|process_all_mp4s|python' .
./rgrank/target/release/rgrank -F "cortisol level" .
./rgrank/target/release/rgrank --files ./hypotheses
./rgrank/target/release/rgrank --version
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
- `-v`, `--version` for compile-time version output
- `--follow-links` for symlink traversal

# Extracted formats

`rgrank` can search plain text files directly and can also extract searchable text in-process from:

- `.docx`
- `.pdf`
- `.pptx`
- `.xlsx`
- `.zip`
- `.tar`
- `.tar.gz`
- `.tgz`

Archive handling stays inside the same Rust binary. There are no subprocess adapters. For supported archive members, `rgrank` extracts and searches their contents recursively. Nested archives are also supported up to a bounded recursion depth.

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

Use `--max-candidate-lines` to raise or lower the global pool of matched lines collected before ranking. The default is `500`.

## Ranked example

Example:

```text
query: cortisol
scanned_files: 40
candidate_lines: 500 (truncated at candidate limit)
results: 5

./hypotheses/pmc_hypotheses/23055545_hypothesis.json
  score=8.862 matched_terms=2/2 match_lines=9
  snippet 7-13 score=2.800
       7 | ...
      13 | ...
```

What this means:

- `query: cortisol` is the search query shown back in ranked output.
- `scanned_files: 40` means 40 files were visited and searched during ranked collection.
- `candidate_lines: 500` means 500 matching lines were collected globally across all files before ranking.
- `truncated at candidate limit` means ranked collection stopped early because the `--max-candidate-lines` cap was hit, so later files were not considered for ranking.
- `results: 5` means 5 ranked files are being shown after any `--top-k` truncation.
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
./rgrank/target/release/rgrank "tumor" ./examples/pmid.tar
./rgrank/target/release/rgrank "bioinformatics" ./examples/pmid.zip
```

mod extract;

use std::collections::BTreeSet;
use std::env;
use std::error::Error;
use std::fs;
use std::io::{self, IsTerminal};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use grep_matcher::Matcher;
use grep_regex::RegexMatcherBuilder;
use grep_searcher::{
    BinaryDetection, Searcher, SearcherBuilder, Sink, SinkContext, SinkFinish, SinkMatch,
};
use ignore::WalkBuilder;
use ignore::overrides::OverrideBuilder;
use ignore::types::TypesBuilder;
use std::io::Cursor;

const DEFAULT_TOP_K: usize = 5;
const DEFAULT_MAX_CANDIDATE_LINES: usize = 500;
const DEFAULT_CONTEXT_LINES: usize = 2;
const DEFAULT_MAX_SNIPPETS: usize = 3;
const BM25_K1: f64 = 1.2;
const BM25_B: f64 = 0.75;
const PARTIAL_HIT_WEIGHT: f64 = 0.35;
const PATH_HIT_WEIGHT: f64 = 0.75;
const PATH_TERM_BONUS: f64 = 0.9;
const COVERAGE_BONUS: f64 = 2.5;
const ALL_TERMS_BONUS: f64 = 1.5;
const PHRASE_BONUS: f64 = 2.0;
const PATH_PHRASE_BONUS: f64 = 1.25;
const MATCH_DENSITY_WEIGHT: f64 = 0.25;
const SAME_LINE_TERM_BONUS: f64 = 0.45;
const PROXIMITY_BONUS_WEIGHT: f64 = 1.6;
const VERSION_TEXT: &str = concat!("rgrank ", env!("CARGO_PKG_VERSION"));
const HELP_SUMMARY: &str = "ripgrep-style search with file-level ranking";
const HELP_BODY: &str = r#"Usage:
  rgrank [options] <query> [path ...]
  rgrank --files [options] [path ...]

Options:
      --files                 List matching file paths only
      --debug                 Show debug warnings for files that could not be searched
  -F, --fixed-strings         Treat the query as literal ranked terms instead of a regex
  -i, --ignore-case           Force case insensitive matching
  -s, --case-sensitive        Force case sensitive matching
  -S, --smart-case            Use smart case matching
  -k, --top-k <n>             Number of ranked files to print (default: 5)
  -m, --max-candidate-lines <n>
                              Global cap on matched lines collected before ranking (default: 500)
  -A, --after-context <n>     Lines shown after each hit
  -B, --before-context <n>    Lines shown before each hit
  -C, --context <n>           Lines shown before and after each hit
      --max-snippets <n>      Snippets shown per ranked file (default: 3)
  -g, --glob <pattern>        Include or exclude files using rg-style globs
  -t, --type <type>           Only search files of the given type
  -T, --type-not <type>       Exclude files of the given type
  -n                          Show line numbers in standard output
      --column                Show the first match column in standard output
      --heading               Group matches under file headings
      --no-heading            Disable grouped file headings
      --json                  Emit JSON lines for matches
      --color <when>          Colorize output: auto, always, never
      --ranked                Use ranked output instead of rg-style output
      --all                   In ranked mode, show all ranked files and snippets
  -w, --word-regexp           Match whole words only
  -x, --line-regexp           Match whole lines only
  -l, --files-with-matches    Print only files that contain matches
      --files-without-match   Print only files that do not contain matches
  -c, --count                 Print match counts per matching file
      --hidden                Include hidden files and directories
      --no-ignore             Disable .gitignore/.ignore filtering
  -L, --follow                Follow symbolic links
      --follow-links          Follow symbolic links
  -h, --help                  Show this help
  -v, --version               Show version

Behavior:
  - Uses ripgrep-style regex matching by default.
  - Use -F/--fixed-strings for literal term matching.
  - Standard rg-style output is the default.
  - Use --ranked for ranked file/snippet output.
  - Standard output defaults to zero context; ranked snippets default to 2 lines before/after.
  - Respects .gitignore/.ignore by default.
  - Ranks files with BM25-like scoring, term coverage, filename boosts, and phrase/proximity bonuses.
"#;

#[derive(Debug, Clone, Copy, Eq, PartialEq)]
enum MatchMode {
    Regex,
    FixedStrings,
}

#[derive(Debug, Clone, Copy, Eq, PartialEq)]
enum CaseMode {
    Smart,
    Sensitive,
    Insensitive,
}

#[derive(Debug, Clone, Copy, Eq, PartialEq)]
enum SearchOutputMode {
    Standard,
    Ranked,
    FilesWithMatches,
    FilesWithoutMatch,
    Count,
    Json,
}

#[derive(Debug, Clone, Copy, Eq, PartialEq)]
enum ColorMode {
    Auto,
    Always,
    Never,
}

#[derive(Debug, Clone)]
struct Cli {
    query: Option<String>,
    roots: Vec<PathBuf>,
    files_mode: bool,
    debug: bool,
    match_mode: MatchMode,
    case_mode: CaseMode,
    output_mode: SearchOutputMode,
    show_all: bool,
    top_k: usize,
    max_candidate_lines: usize,
    before_context: usize,
    after_context: usize,
    context_explicit: bool,
    max_snippets: usize,
    globs: Vec<String>,
    type_names: Vec<String>,
    type_not_names: Vec<String>,
    line_numbers: bool,
    show_column: bool,
    heading: bool,
    color_mode: ColorMode,
    word_regexp: bool,
    line_regexp: bool,
    hidden: bool,
    no_ignore: bool,
    follow_links: bool,
}

#[derive(Debug, Clone)]
struct Query {
    normalized_phrase: String,
    terms: Vec<String>,
}

#[derive(Debug, Clone)]
struct LineMatchRecord {
    line_number: usize,
    text: String,
    normalized_text: String,
    matched_terms: Vec<usize>,
    score_hint: f64,
}

#[derive(Debug, Clone)]
struct FileCandidate {
    path: PathBuf,
    normalized_path: String,
    path_tokens: Vec<String>,
    matches: Vec<LineMatchRecord>,
}

#[derive(Debug)]
struct CorpusStats {
    total_docs: usize,
    average_doc_len: f64,
    document_frequency: Vec<usize>,
}

#[derive(Debug)]
struct SearchReport {
    query: String,
    total_terms: usize,
    scanned_files: usize,
    candidate_lines: usize,
    truncated: bool,
    results: Vec<RankedFile>,
}

#[derive(Debug)]
struct CountEntry {
    path: PathBuf,
    count: usize,
}

#[derive(Debug, Clone)]
struct MatchEvent {
    path: PathBuf,
    line_number: usize,
    column: Option<usize>,
    text: String,
    submatches: Vec<(usize, usize)>,
}

#[derive(Debug, Clone)]
struct JsonLineEvent {
    kind: JsonLineEventKind,
    line_number: usize,
    absolute_offset: u64,
    text: String,
    submatches: Vec<(usize, usize)>,
}

#[derive(Debug, Clone, Copy, Eq, PartialEq)]
enum JsonLineEventKind {
    Context,
    Match,
}

#[derive(Debug)]
struct JsonFileResult {
    path: PathBuf,
    matched_lines: usize,
    matches: usize,
    binary_offset: Option<u64>,
    bytes_searched: u64,
    elapsed: Duration,
    events: Vec<JsonLineEvent>,
}

#[derive(Debug, Default)]
struct JsonTotals {
    searches: usize,
    searches_with_match: usize,
    matched_lines: usize,
    matches: usize,
    bytes_searched: u64,
}

#[derive(Debug)]
struct RunOutcome {
    output: String,
    exit_code: i32,
}

enum ParseOutcome {
    Cli(Cli),
    Help,
    Version,
}

#[derive(Debug)]
struct JsonSearchResult {
    output: String,
    had_match: bool,
}

#[derive(Debug, Clone)]
struct OutputBlock {
    path: PathBuf,
    lines: Vec<OutputLine>,
}

#[derive(Debug, Clone)]
struct OutputLine {
    line_number: usize,
    column: Option<usize>,
    text: String,
    submatches: Vec<(usize, usize)>,
    is_match: bool,
}

#[derive(Debug)]
struct RankedFile {
    path: PathBuf,
    score: f64,
    matched_terms: usize,
    match_count: usize,
    snippets: Vec<Snippet>,
}

#[derive(Debug)]
struct Snippet {
    start_line: usize,
    end_line: usize,
    score: f64,
    lines: Vec<(usize, String)>,
}

#[derive(Debug)]
struct CollectingSink {
    query: Query,
    limit: usize,
    lines: Vec<LineMatchRecord>,
    hit_limit: bool,
}

#[derive(Debug, Default)]
struct CountingSink {
    match_count: usize,
    stop_after_first: bool,
}

#[derive(Debug, Clone)]
struct MatchEventSink {
    path: PathBuf,
    matcher: grep_regex::RegexMatcher,
    capture_matches: bool,
    events: Vec<MatchEvent>,
}

#[derive(Debug, Clone)]
struct JsonEventSink {
    path: PathBuf,
    matcher: grep_regex::RegexMatcher,
    matched_lines: usize,
    matches: usize,
    events: Vec<JsonLineEvent>,
    started_at: Instant,
    finished: Option<SinkFinish>,
}

impl CollectingSink {
    fn new(query: Query, limit: usize) -> Self {
        Self {
            query,
            limit,
            lines: Vec::new(),
            hit_limit: false,
        }
    }
}

impl CountingSink {
    fn new(stop_after_first: bool) -> Self {
        Self {
            match_count: 0,
            stop_after_first,
        }
    }
}

impl MatchEventSink {
    fn new(path: PathBuf, matcher: grep_regex::RegexMatcher, capture_matches: bool) -> Self {
        Self {
            path,
            matcher,
            capture_matches,
            events: Vec::new(),
        }
    }
}

impl JsonEventSink {
    fn new(path: PathBuf, matcher: grep_regex::RegexMatcher) -> Self {
        Self {
            path,
            matcher,
            matched_lines: 0,
            matches: 0,
            events: Vec::new(),
            started_at: Instant::now(),
            finished: None,
        }
    }

    fn into_result(self) -> JsonFileResult {
        let elapsed = self.started_at.elapsed();
        let (binary_offset, bytes_searched) = match self.finished {
            Some(finish) => (finish.binary_byte_offset(), finish.byte_count()),
            None => (None, 0),
        };
        JsonFileResult {
            path: self.path,
            matched_lines: self.matched_lines,
            matches: self.matches,
            binary_offset,
            bytes_searched,
            elapsed,
            events: self.events,
        }
    }
}

impl Sink for CollectingSink {
    type Error = io::Error;

    fn matched(&mut self, _searcher: &Searcher, mat: &SinkMatch<'_>) -> Result<bool, Self::Error> {
        let mut line_number = mat.line_number().unwrap_or(1);
        for line in mat.lines() {
            if self.lines.len() >= self.limit {
                self.hit_limit = true;
                return Ok(false);
            }
            let text = trim_line_ending(&String::from_utf8_lossy(line)).to_owned();
            let normalized_text = normalize_text(&text);
            let (matched_terms, score_hint) =
                score_line_against_query(&self.query, &normalized_text);
            self.lines.push(LineMatchRecord {
                line_number: line_number as usize,
                text,
                normalized_text,
                matched_terms,
                score_hint,
            });
            line_number += 1;
        }
        Ok(true)
    }
}

impl Sink for CountingSink {
    type Error = io::Error;

    fn matched(&mut self, _searcher: &Searcher, mat: &SinkMatch<'_>) -> Result<bool, Self::Error> {
        self.match_count += mat.lines().count();
        Ok(!self.stop_after_first)
    }
}

impl Sink for MatchEventSink {
    type Error = io::Error;

    fn matched(&mut self, _searcher: &Searcher, mat: &SinkMatch<'_>) -> Result<bool, Self::Error> {
        let mut line_number = mat.line_number().unwrap_or(1);
        for line in mat.lines() {
            let line_bytes = trim_line_ending_bytes(line);
            let submatches = if self.capture_matches {
                find_submatches(&self.matcher, line_bytes)
            } else {
                Vec::new()
            };
            let column = submatches.first().map(|(start, _)| start + 1);
            self.events.push(MatchEvent {
                path: self.path.clone(),
                line_number: line_number as usize,
                column,
                text: String::from_utf8_lossy(line_bytes).to_string(),
                submatches,
            });
            line_number += 1;
        }
        Ok(true)
    }
}

impl Sink for JsonEventSink {
    type Error = io::Error;

    fn matched(&mut self, _searcher: &Searcher, mat: &SinkMatch<'_>) -> Result<bool, Self::Error> {
        let mut line_number = mat.line_number().unwrap_or(1);
        let mut absolute_offset = mat.absolute_byte_offset();
        for line in mat.lines() {
            let submatches = find_submatches(&self.matcher, line);
            self.matched_lines += 1;
            self.matches += submatches.len().max(1);
            self.events.push(JsonLineEvent {
                kind: JsonLineEventKind::Match,
                line_number: line_number as usize,
                absolute_offset,
                text: String::from_utf8_lossy(line).to_string(),
                submatches,
            });
            line_number += 1;
            absolute_offset += line.len() as u64;
        }
        Ok(true)
    }

    fn context(
        &mut self,
        _searcher: &Searcher,
        context: &SinkContext<'_>,
    ) -> Result<bool, Self::Error> {
        self.events.push(JsonLineEvent {
            kind: JsonLineEventKind::Context,
            line_number: context.line_number().unwrap_or(1) as usize,
            absolute_offset: context.absolute_byte_offset(),
            text: String::from_utf8_lossy(context.bytes()).to_string(),
            submatches: Vec::new(),
        });
        Ok(true)
    }

    fn finish(&mut self, _searcher: &Searcher, finish: &SinkFinish) -> Result<(), Self::Error> {
        self.finished = Some(finish.clone());
        Ok(())
    }
}

fn main() {
    match run() {
        Ok(outcome) => {
            if !outcome.output.is_empty() {
                println!("{}", outcome.output);
            }
            std::process::exit(outcome.exit_code);
        }
        Err(message) => {
            eprintln!("{message}");
            std::process::exit(2);
        }
    }
}

fn run() -> Result<RunOutcome, String> {
    match parse_args()? {
        ParseOutcome::Cli(cli) => execute_cli(&cli),
        ParseOutcome::Help => Ok(RunOutcome {
            output: help_text(),
            exit_code: 0,
        }),
        ParseOutcome::Version => Ok(RunOutcome {
            output: VERSION_TEXT.to_owned(),
            exit_code: 0,
        }),
    }
}

fn execute_cli(cli: &Cli) -> Result<RunOutcome, String> {
    if cli.files_mode {
        let files = list_files(&cli).map_err(|error| error.to_string())?;
        return Ok(RunOutcome {
            output: render_file_list(&files),
            exit_code: 0,
        });
    }
    match cli.output_mode {
        SearchOutputMode::Standard => {
            let events = execute_standard_search(&cli).map_err(|error| error.to_string())?;
            Ok(RunOutcome {
                output: render_standard_output(&events, &cli),
                exit_code: if events.is_empty() { 1 } else { 0 },
            })
        }
        SearchOutputMode::Ranked => {
            let report = execute_search(&cli).map_err(|error| error.to_string())?;
            Ok(RunOutcome {
                output: render_report(&report),
                exit_code: if report.candidate_lines == 0 { 1 } else { 0 },
            })
        }
        SearchOutputMode::FilesWithMatches => {
            let files = execute_files_with_matches(&cli).map_err(|error| error.to_string())?;
            Ok(RunOutcome {
                output: render_file_list(&files),
                exit_code: if files.is_empty() { 1 } else { 0 },
            })
        }
        SearchOutputMode::FilesWithoutMatch => {
            let files = execute_files_without_match(&cli).map_err(|error| error.to_string())?;
            Ok(RunOutcome {
                output: render_file_list(&files),
                exit_code: if files.is_empty() { 1 } else { 0 },
            })
        }
        SearchOutputMode::Count => {
            let counts = execute_count(&cli).map_err(|error| error.to_string())?;
            Ok(RunOutcome {
                output: render_count_list(&counts, &cli),
                exit_code: if counts.is_empty() { 1 } else { 0 },
            })
        }
        SearchOutputMode::Json => {
            let result = execute_json_search(&cli).map_err(|error| error.to_string())?;
            Ok(RunOutcome {
                output: result.output,
                exit_code: if result.had_match { 0 } else { 1 },
            })
        }
    }
}

fn parse_args() -> Result<ParseOutcome, String> {
    parse_args_from(env::args().skip(1))
}

fn parse_args_from<I>(args: I) -> Result<ParseOutcome, String>
where
    I: IntoIterator<Item = String>,
{
    let mut top_k = DEFAULT_TOP_K;
    let mut max_candidate_lines = DEFAULT_MAX_CANDIDATE_LINES;
    let mut before_context = 0usize;
    let mut after_context = 0usize;
    let mut context_explicit = false;
    let mut max_snippets = DEFAULT_MAX_SNIPPETS;
    let mut hidden = false;
    let mut no_ignore = false;
    let mut follow_links = false;
    let mut files_mode = false;
    let mut debug = false;
    let mut match_mode = MatchMode::Regex;
    let mut case_mode = CaseMode::Smart;
    let mut output_mode = SearchOutputMode::Standard;
    let mut show_all = false;
    let mut globs = Vec::new();
    let mut type_names = Vec::new();
    let mut type_not_names = Vec::new();
    let mut line_numbers = false;
    let mut show_column = false;
    let mut heading = false;
    let mut color_mode = ColorMode::Auto;
    let mut word_regexp = false;
    let mut line_regexp = false;
    let mut positionals: Vec<String> = Vec::new();
    let mut args = args.into_iter();

    while let Some(argument) = args.next() {
        match argument.as_str() {
            "-h" | "--help" => return Ok(ParseOutcome::Help),
            "-v" | "--version" => return Ok(ParseOutcome::Version),
            "--files" => files_mode = true,
            "--debug" => debug = true,
            "-F" | "--fixed-strings" => match_mode = MatchMode::FixedStrings,
            "-i" | "--ignore-case" => case_mode = CaseMode::Insensitive,
            "-s" | "--case-sensitive" => case_mode = CaseMode::Sensitive,
            "-S" | "--smart-case" => case_mode = CaseMode::Smart,
            "-n" => line_numbers = true,
            "--column" => show_column = true,
            "--heading" => heading = true,
            "--no-heading" => heading = false,
            "--json" => output_mode = SearchOutputMode::Json,
            "--ranked" => output_mode = SearchOutputMode::Ranked,
            "--all" => show_all = true,
            "--color" => {
                color_mode = parse_color_mode(&parse_string_value(args.next(), &argument)?)?;
            }
            "-w" | "--word-regexp" => word_regexp = true,
            "-x" | "--line-regexp" => line_regexp = true,
            "-l" | "--files-with-matches" => output_mode = SearchOutputMode::FilesWithMatches,
            "--files-without-match" => output_mode = SearchOutputMode::FilesWithoutMatch,
            "-c" | "--count" => output_mode = SearchOutputMode::Count,
            "--hidden" => hidden = true,
            "--no-ignore" => no_ignore = true,
            "-L" | "--follow" | "--follow-links" => follow_links = true,
            "-k" | "--top-k" => {
                top_k = parse_usize_value(args.next(), &argument)?;
            }
            "-m" | "--max-candidate-lines" => {
                max_candidate_lines = parse_usize_value(args.next(), &argument)?;
            }
            "-A" | "--after-context" => {
                context_explicit = true;
                after_context = parse_usize_value(args.next(), &argument)?;
            }
            "-B" | "--before-context" => {
                context_explicit = true;
                before_context = parse_usize_value(args.next(), &argument)?;
            }
            "-C" | "--context" => {
                context_explicit = true;
                let context = parse_usize_value(args.next(), &argument)?;
                before_context = context;
                after_context = context;
            }
            "--max-snippets" => {
                max_snippets = parse_usize_value(args.next(), &argument)?;
            }
            "-g" | "--glob" => {
                globs.push(parse_string_value(args.next(), &argument)?);
            }
            "-t" | "--type" => {
                type_names.push(parse_string_value(args.next(), &argument)?);
            }
            "-T" | "--type-not" => {
                type_not_names.push(parse_string_value(args.next(), &argument)?);
            }
            _ if argument.starts_with("--top-k=") => {
                top_k = parse_usize_inline(&argument, "--top-k=")?;
            }
            _ if argument.starts_with("--max-candidate-lines=") => {
                max_candidate_lines = parse_usize_inline(&argument, "--max-candidate-lines=")?;
            }
            _ if argument.starts_with("--after-context=") => {
                context_explicit = true;
                after_context = parse_usize_inline(&argument, "--after-context=")?;
            }
            _ if argument.starts_with("--before-context=") => {
                context_explicit = true;
                before_context = parse_usize_inline(&argument, "--before-context=")?;
            }
            _ if argument.starts_with("--context=") => {
                context_explicit = true;
                let context = parse_usize_inline(&argument, "--context=")?;
                before_context = context;
                after_context = context;
            }
            _ if argument.starts_with("--max-snippets=") => {
                max_snippets = parse_usize_inline(&argument, "--max-snippets=")?;
            }
            _ if argument.starts_with("--glob=") => {
                globs.push(parse_string_inline(&argument, "--glob=")?);
            }
            _ if argument.starts_with("--type=") => {
                type_names.push(parse_string_inline(&argument, "--type=")?);
            }
            _ if argument.starts_with("--type-not=") => {
                type_not_names.push(parse_string_inline(&argument, "--type-not=")?);
            }
            _ if argument.starts_with("--color=") => {
                color_mode = parse_color_mode(&parse_string_inline(&argument, "--color=")?)?;
            }
            _ if argument.starts_with("-A") && argument.len() > 2 => {
                context_explicit = true;
                after_context = parse_short_usize_inline(&argument, "-A")?;
            }
            _ if argument.starts_with("-B") && argument.len() > 2 => {
                context_explicit = true;
                before_context = parse_short_usize_inline(&argument, "-B")?;
            }
            _ if argument.starts_with("-C") && argument.len() > 2 => {
                context_explicit = true;
                let context = parse_short_usize_inline(&argument, "-C")?;
                before_context = context;
                after_context = context;
            }
            _ if argument.starts_with("-g") && argument.len() > 2 => {
                globs.push(parse_short_string_inline(&argument, "-g")?);
            }
            _ if argument.starts_with("-t") && argument.len() > 2 => {
                type_names.push(parse_short_string_inline(&argument, "-t")?);
            }
            _ if argument.starts_with("-T") && argument.len() > 2 => {
                type_not_names.push(parse_short_string_inline(&argument, "-T")?);
            }
            "--" => {
                positionals.extend(args);
                break;
            }
            _ if argument.starts_with('-') => {
                return Err(format!("unknown flag: {argument}\n\n{}", help_text()));
            }
            _ => positionals.push(argument),
        }
    }

    let (query, roots) = if files_mode {
        let roots = if positionals.is_empty() {
            vec![PathBuf::from(".")]
        } else {
            positionals.into_iter().map(PathBuf::from).collect()
        };
        (None, roots)
    } else {
        if positionals.is_empty() {
            return Err(format!("missing query\n\n{}", help_text()));
        }
        let query = positionals.remove(0);
        if query.trim().is_empty() {
            return Err("query must not be empty".to_owned());
        }
        let roots = if positionals.is_empty() {
            vec![PathBuf::from(".")]
        } else {
            positionals.into_iter().map(PathBuf::from).collect()
        };
        (Some(query), roots)
    };

    Ok(ParseOutcome::Cli(Cli {
        query,
        roots,
        files_mode,
        debug,
        match_mode,
        case_mode,
        output_mode,
        show_all,
        top_k,
        max_candidate_lines,
        before_context,
        after_context,
        context_explicit,
        max_snippets,
        globs,
        type_names,
        type_not_names,
        line_numbers,
        show_column,
        heading,
        color_mode,
        word_regexp,
        line_regexp,
        hidden,
        no_ignore,
        follow_links,
    }))
}

fn help_text() -> String {
    format!("{VERSION_TEXT}: {HELP_SUMMARY}\n\n{HELP_BODY}")
}

fn search_warning_message(cli: &Cli, path: &Path, error: &dyn std::fmt::Display) -> Option<String> {
    if cli.debug {
        Some(format!(
            "warning: failed to search {}: {error}",
            path.display()
        ))
    } else {
        None
    }
}

fn maybe_print_search_warning(cli: &Cli, path: &Path, error: &dyn std::fmt::Display) {
    if let Some(message) = search_warning_message(cli, path, error) {
        eprintln!("{message}");
    }
}

fn parse_usize_value(value: Option<String>, flag: &str) -> Result<usize, String> {
    let Some(raw) = value else {
        return Err(format!("missing value for {flag}"));
    };
    raw.parse::<usize>()
        .map_err(|_| format!("invalid numeric value for {flag}: {raw}"))
}

fn parse_string_value(value: Option<String>, flag: &str) -> Result<String, String> {
    value.ok_or_else(|| format!("missing value for {flag}"))
}

fn parse_usize_inline(argument: &str, prefix: &str) -> Result<usize, String> {
    let value = argument
        .strip_prefix(prefix)
        .ok_or_else(|| format!("invalid flag syntax: {argument}"))?;
    value
        .parse::<usize>()
        .map_err(|_| format!("invalid numeric value for {prefix}: {value}"))
}

fn parse_string_inline(argument: &str, prefix: &str) -> Result<String, String> {
    argument
        .strip_prefix(prefix)
        .map(ToOwned::to_owned)
        .ok_or_else(|| format!("invalid flag syntax: {argument}"))
}

fn parse_short_usize_inline(argument: &str, prefix: &str) -> Result<usize, String> {
    let value = argument
        .strip_prefix(prefix)
        .ok_or_else(|| format!("invalid flag syntax: {argument}"))?;
    value
        .parse::<usize>()
        .map_err(|_| format!("invalid numeric value for {prefix}: {value}"))
}

fn parse_short_string_inline(argument: &str, prefix: &str) -> Result<String, String> {
    argument
        .strip_prefix(prefix)
        .map(ToOwned::to_owned)
        .ok_or_else(|| format!("invalid flag syntax: {argument}"))
}

fn parse_color_mode(value: &str) -> Result<ColorMode, String> {
    match value {
        "auto" => Ok(ColorMode::Auto),
        "always" => Ok(ColorMode::Always),
        "never" => Ok(ColorMode::Never),
        _ => Err(format!("invalid value for --color: {value}")),
    }
}

fn build_walk_builder(cli: &Cli) -> Result<WalkBuilder, Box<dyn Error>> {
    for root in &cli.roots {
        if !root.exists() {
            return Err(format!("search root does not exist: {}", root.display()).into());
        }
    }

    let mut walker_builder = WalkBuilder::new(&cli.roots[0]);
    for root in cli.roots.iter().skip(1) {
        walker_builder.add(root);
    }
    walker_builder.follow_links(cli.follow_links);
    walker_builder.sort_by_file_path(|left, right| left.cmp(right));
    if cli.hidden {
        walker_builder.hidden(false);
    }
    if cli.no_ignore {
        walker_builder.parents(false);
        walker_builder.ignore(false);
        walker_builder.git_ignore(false);
        walker_builder.git_global(false);
        walker_builder.git_exclude(false);
    }
    if !cli.globs.is_empty() {
        let mut override_builder = OverrideBuilder::new(".");
        for glob in &cli.globs {
            override_builder.add(glob)?;
        }
        walker_builder.overrides(override_builder.build()?);
    }
    if !cli.type_names.is_empty() || !cli.type_not_names.is_empty() {
        let mut types_builder = TypesBuilder::new();
        types_builder.add_defaults();
        for type_name in &cli.type_names {
            types_builder.select(type_name);
        }
        for type_name in &cli.type_not_names {
            types_builder.negate(type_name);
        }
        walker_builder.types(types_builder.build()?);
    }
    walker_builder.require_git(false);
    Ok(walker_builder)
}

fn list_files(cli: &Cli) -> Result<Vec<PathBuf>, Box<dyn Error>> {
    let walker_builder = build_walk_builder(cli)?;
    let mut files = Vec::new();

    for entry in walker_builder.build() {
        let Ok(dir_entry) = entry else {
            continue;
        };
        if matches!(dir_entry.file_type(), Some(file_type) if file_type.is_file()) {
            files.push(dir_entry.path().to_path_buf());
        }
    }

    Ok(files)
}

fn execute_search(cli: &Cli) -> Result<SearchReport, Box<dyn Error>> {
    let query_text = cli
        .query
        .as_deref()
        .ok_or_else(|| "query is required unless --files is set".to_owned())?;
    let query = Query::from_raw(query_text)?;
    let matcher = build_matcher(cli, query_text, &query)?;
    let mut searcher = build_searcher(true);

    let walker_builder = build_walk_builder(cli)?;

    let mut candidates = Vec::new();
    let mut scanned_files = 0usize;
    let mut candidate_lines = 0usize;
    let mut truncated = false;

    for entry in walker_builder.build() {
        let Ok(dir_entry) = entry else {
            continue;
        };
        if !matches!(dir_entry.file_type(), Some(file_type) if file_type.is_file()) {
            continue;
        }
        if candidate_lines >= cli.max_candidate_lines {
            truncated = true;
            break;
        }

        scanned_files += 1;
        let remaining_budget = cli.max_candidate_lines.saturating_sub(candidate_lines);
        let mut sink = CollectingSink::new(query.clone(), remaining_budget);
        if let Err(error) = search_target(&mut searcher, &matcher, dir_entry.path(), &mut sink) {
            maybe_print_search_warning(cli, dir_entry.path(), &error);
            continue;
        }
        if sink.lines.is_empty() {
            continue;
        }
        candidate_lines += sink.lines.len();
        truncated |= sink.hit_limit;
        candidates.push(FileCandidate::new(
            dir_entry.path().to_path_buf(),
            sink.lines,
        ));
    }

    let mut ranked = rank_candidates(&query, candidates);
    if !cli.show_all {
        ranked.truncate(cli.top_k);
    }
    let max_snippets = if cli.show_all {
        usize::MAX
    } else {
        cli.max_snippets
    };
    let before_context = ranked_before_context(cli);
    let after_context = ranked_after_context(cli);
    for result in &mut ranked {
        result.snippets = build_snippets(
            &result.path,
            before_context,
            after_context,
            max_snippets,
            &result.snippets,
        );
    }

    Ok(SearchReport {
        query: query_text.to_owned(),
        total_terms: query.terms.len(),
        scanned_files,
        candidate_lines,
        truncated,
        results: ranked,
    })
}

fn execute_files_with_matches(cli: &Cli) -> Result<Vec<PathBuf>, Box<dyn Error>> {
    let query_text = cli
        .query
        .as_deref()
        .ok_or_else(|| "query is required unless --files is set".to_owned())?;
    let query = Query::from_raw(query_text)?;
    let matcher = build_matcher(cli, query_text, &query)?;
    let walker_builder = build_walk_builder(cli)?;
    let mut searcher = build_searcher(false);
    let mut files = Vec::new();

    for entry in walker_builder.build() {
        let Ok(dir_entry) = entry else {
            continue;
        };
        if !matches!(dir_entry.file_type(), Some(file_type) if file_type.is_file()) {
            continue;
        }
        let count = match search_file_count(&mut searcher, &matcher, dir_entry.path(), true) {
            Ok(count) => count,
            Err(error) => {
                maybe_print_search_warning(cli, dir_entry.path(), &error);
                continue;
            }
        };
        if count > 0 {
            files.push(dir_entry.path().to_path_buf());
        }
    }

    Ok(files)
}

fn execute_files_without_match(cli: &Cli) -> Result<Vec<PathBuf>, Box<dyn Error>> {
    let query_text = cli
        .query
        .as_deref()
        .ok_or_else(|| "query is required unless --files is set".to_owned())?;
    let query = Query::from_raw(query_text)?;
    let matcher = build_matcher(cli, query_text, &query)?;
    let walker_builder = build_walk_builder(cli)?;
    let mut searcher = build_searcher(false);
    let mut files = Vec::new();

    for entry in walker_builder.build() {
        let Ok(dir_entry) = entry else {
            continue;
        };
        if !matches!(dir_entry.file_type(), Some(file_type) if file_type.is_file()) {
            continue;
        }
        let count = match search_file_count(&mut searcher, &matcher, dir_entry.path(), true) {
            Ok(count) => count,
            Err(error) => {
                maybe_print_search_warning(cli, dir_entry.path(), &error);
                continue;
            }
        };
        if count == 0 {
            files.push(dir_entry.path().to_path_buf());
        }
    }

    Ok(files)
}

fn execute_count(cli: &Cli) -> Result<Vec<CountEntry>, Box<dyn Error>> {
    let query_text = cli
        .query
        .as_deref()
        .ok_or_else(|| "query is required unless --files is set".to_owned())?;
    let query = Query::from_raw(query_text)?;
    let matcher = build_matcher(cli, query_text, &query)?;
    let walker_builder = build_walk_builder(cli)?;
    let mut searcher = build_searcher(false);
    let mut counts = Vec::new();

    for entry in walker_builder.build() {
        let Ok(dir_entry) = entry else {
            continue;
        };
        if !matches!(dir_entry.file_type(), Some(file_type) if file_type.is_file()) {
            continue;
        }
        let count = match search_file_count(&mut searcher, &matcher, dir_entry.path(), false) {
            Ok(count) => count,
            Err(error) => {
                maybe_print_search_warning(cli, dir_entry.path(), &error);
                continue;
            }
        };
        if count > 0 {
            counts.push(CountEntry {
                path: dir_entry.path().to_path_buf(),
                count,
            });
        }
    }

    Ok(counts)
}

fn build_matcher(
    cli: &Cli,
    query_text: &str,
    query: &Query,
) -> Result<grep_regex::RegexMatcher, Box<dyn Error>> {
    let mut matcher_builder = RegexMatcherBuilder::new();
    match cli.case_mode {
        CaseMode::Smart => {
            matcher_builder.case_insensitive(false);
            matcher_builder.case_smart(true);
        }
        CaseMode::Sensitive => {
            matcher_builder.case_insensitive(false);
            matcher_builder.case_smart(false);
        }
        CaseMode::Insensitive => {
            matcher_builder.case_insensitive(true);
            matcher_builder.case_smart(false);
        }
    }
    matcher_builder.word(cli.word_regexp);
    matcher_builder.whole_line(cli.line_regexp);
    let matcher = match cli.match_mode {
        MatchMode::Regex => matcher_builder.build(query_text)?,
        MatchMode::FixedStrings => matcher_builder.build_literals(&query.terms)?,
    };
    Ok(matcher)
}

fn build_searcher(line_numbers: bool) -> Searcher {
    let mut searcher_builder = SearcherBuilder::new();
    searcher_builder.line_number(line_numbers);
    searcher_builder.binary_detection(BinaryDetection::quit(0));
    searcher_builder.build()
}

fn build_searcher_with_context(
    line_numbers: bool,
    before_context: usize,
    after_context: usize,
) -> Searcher {
    let mut searcher_builder = SearcherBuilder::new();
    searcher_builder.line_number(line_numbers);
    searcher_builder.before_context(before_context);
    searcher_builder.after_context(after_context);
    searcher_builder.binary_detection(BinaryDetection::quit(0));
    searcher_builder.build()
}

fn search_file_count(
    searcher: &mut Searcher,
    matcher: &grep_regex::RegexMatcher,
    path: &Path,
    stop_after_first: bool,
) -> Result<usize, Box<dyn Error>> {
    let mut sink = CountingSink::new(stop_after_first);
    search_target(searcher, matcher, path, &mut sink)?;
    Ok(sink.match_count)
}

fn search_target<S>(
    searcher: &mut Searcher,
    matcher: &grep_regex::RegexMatcher,
    path: &Path,
    sink: &mut S,
) -> io::Result<()>
where
    S: Sink<Error = io::Error>,
{
    match extract::extract_searchable_text(path) {
        Ok(Some(text)) => searcher.search_reader(matcher, Cursor::new(text.into_bytes()), sink),
        Ok(None) => searcher.search_path(matcher, path, sink),
        Err(error) => Err(io::Error::other(error.to_string())),
    }
}

fn execute_standard_search(cli: &Cli) -> Result<Vec<MatchEvent>, Box<dyn Error>> {
    let query_text = cli
        .query
        .as_deref()
        .ok_or_else(|| "query is required unless --files is set".to_owned())?;
    let query = Query::from_raw(query_text)?;
    let matcher = build_matcher(cli, query_text, &query)?;
    let walker_builder = build_walk_builder(cli)?;
    let mut searcher = build_searcher(true);
    let mut events = Vec::new();
    let capture_matches =
        cli.show_column || color_enabled(cli) || cli.output_mode == SearchOutputMode::Json;

    for entry in walker_builder.build() {
        let Ok(dir_entry) = entry else {
            continue;
        };
        if !matches!(dir_entry.file_type(), Some(file_type) if file_type.is_file()) {
            continue;
        }
        let mut sink = MatchEventSink::new(
            dir_entry.path().to_path_buf(),
            matcher.clone(),
            capture_matches,
        );
        if let Err(error) = search_target(&mut searcher, &matcher, dir_entry.path(), &mut sink) {
            maybe_print_search_warning(cli, dir_entry.path(), &error);
            continue;
        }
        events.extend(sink.events);
    }

    Ok(events)
}

fn execute_json_search(cli: &Cli) -> Result<JsonSearchResult, Box<dyn Error>> {
    let query_text = cli
        .query
        .as_deref()
        .ok_or_else(|| "query is required unless --files is set".to_owned())?;
    let query = Query::from_raw(query_text)?;
    let matcher = build_matcher(cli, query_text, &query)?;
    let walker_builder = build_walk_builder(cli)?;
    let mut searcher = build_searcher_with_context(true, cli.before_context, cli.after_context);
    let started_at = Instant::now();
    let mut totals = JsonTotals::default();
    let mut file_results = Vec::new();

    for entry in walker_builder.build() {
        let Ok(dir_entry) = entry else {
            continue;
        };
        if !matches!(dir_entry.file_type(), Some(file_type) if file_type.is_file()) {
            continue;
        }
        totals.searches += 1;
        let mut sink = JsonEventSink::new(dir_entry.path().to_path_buf(), matcher.clone());
        if let Err(error) = search_target(&mut searcher, &matcher, dir_entry.path(), &mut sink) {
            maybe_print_search_warning(cli, dir_entry.path(), &error);
            continue;
        }
        let result = sink.into_result();
        totals.bytes_searched += result.bytes_searched;
        totals.matched_lines += result.matched_lines;
        totals.matches += result.matches;
        if result.matched_lines > 0 {
            totals.searches_with_match += 1;
            file_results.push(result);
        }
    }

    let had_match = totals.searches_with_match > 0;
    Ok(JsonSearchResult {
        output: render_json_output(&file_results, totals, started_at.elapsed()),
        had_match,
    })
}

fn rank_candidates(query: &Query, candidates: Vec<FileCandidate>) -> Vec<RankedFile> {
    if candidates.is_empty() {
        return Vec::new();
    }

    let stats = CorpusStats::from_candidates(query, &candidates);
    let mut ranked: Vec<RankedFile> = candidates
        .into_iter()
        .map(|candidate| score_candidate(query, &stats, candidate))
        .collect();

    ranked.sort_by(|left, right| {
        right
            .score
            .total_cmp(&left.score)
            .then_with(|| left.path.cmp(&right.path))
    });
    ranked
}

fn score_candidate(query: &Query, stats: &CorpusStats, candidate: FileCandidate) -> RankedFile {
    let body_tokens = body_tokens(&candidate);
    let joined_body = join_normalized_lines(&candidate.matches);
    let doc_len = body_tokens.len().max(1) as f64;
    let matched_terms = matched_term_count(query, &candidate, &body_tokens);
    let mut score = 0.0;

    for (index, term) in query.terms.iter().enumerate() {
        let exact_hits = count_exact_hits(&body_tokens, term);
        let partial_hits = count_partial_hits(&body_tokens, term, exact_hits > 0);
        let path_hits = count_exact_hits(&candidate.path_tokens, term);
        let tf = exact_hits as f64
            + partial_hits as f64 * PARTIAL_HIT_WEIGHT
            + path_hits as f64 * PATH_HIT_WEIGHT;
        if tf == 0.0 {
            continue;
        }
        let doc_frequency = stats.document_frequency[index] as f64;
        let total_docs = stats.total_docs as f64;
        let idf = ((total_docs - doc_frequency + 0.5) / (doc_frequency + 0.5) + 1.0).ln();
        let length_norm =
            tf + BM25_K1 * (1.0 - BM25_B + BM25_B * doc_len / stats.average_doc_len.max(1.0));
        score += idf * (tf * (BM25_K1 + 1.0)) / length_norm;

        if path_hits > 0 || contains_partial_token(&candidate.path_tokens, term) {
            score += PATH_TERM_BONUS;
        }
    }

    let coverage_ratio = matched_terms as f64 / query.terms.len() as f64;
    score += coverage_ratio * COVERAGE_BONUS;
    if matched_terms == query.terms.len() {
        score += ALL_TERMS_BONUS;
    }
    if joined_body.contains(&query.normalized_phrase) {
        score += PHRASE_BONUS;
    } else if candidate.normalized_path.contains(&query.normalized_phrase) {
        score += PATH_PHRASE_BONUS;
    }
    score += best_same_line_bonus(&candidate.matches);
    if let Some(span) = minimal_cover_span(query.terms.len(), &candidate.matches) {
        score += PROXIMITY_BONUS_WEIGHT / (span as f64).sqrt();
    }
    score += (candidate.matches.len() as f64).sqrt() * MATCH_DENSITY_WEIGHT;

    RankedFile {
        path: candidate.path,
        score,
        matched_terms,
        match_count: candidate.matches.len(),
        snippets: candidate
            .matches
            .iter()
            .map(|item| Snippet {
                start_line: item.line_number,
                end_line: item.line_number,
                score: item.score_hint,
                lines: vec![(item.line_number, item.text.clone())],
            })
            .collect(),
    }
}

fn build_snippets(
    path: &Path,
    before_context: usize,
    after_context: usize,
    max_snippets: usize,
    placeholder_snippets: &[Snippet],
) -> Vec<Snippet> {
    let Ok(file_lines) = read_file_lines(path) else {
        return placeholder_snippets
            .iter()
            .take(max_snippets)
            .map(clone_snippet)
            .collect();
    };
    if file_lines.is_empty() {
        return placeholder_snippets
            .iter()
            .take(max_snippets)
            .map(clone_snippet)
            .collect();
    }

    let mut merged = Vec::<Snippet>::new();
    for snippet in placeholder_snippets {
        let start_line = snippet.start_line.saturating_sub(before_context).max(1);
        let end_line = (snippet.end_line + after_context).min(file_lines.len());
        if let Some(previous) = merged.last_mut() {
            if start_line <= previous.end_line + 1 {
                previous.end_line = previous.end_line.max(end_line);
                previous.score += snippet.score;
                continue;
            }
        }
        merged.push(Snippet {
            start_line,
            end_line,
            score: snippet.score,
            lines: Vec::new(),
        });
    }

    for snippet in &mut merged {
        snippet.lines = (snippet.start_line..=snippet.end_line)
            .map(|line_number| (line_number, file_lines[line_number - 1].clone()))
            .collect();
    }

    merged.sort_by(|left, right| {
        right
            .score
            .total_cmp(&left.score)
            .then_with(|| left.start_line.cmp(&right.start_line))
    });
    merged.truncate(max_snippets);
    merged.sort_by(|left, right| left.start_line.cmp(&right.start_line));
    merged
}

fn read_file_lines(path: &Path) -> io::Result<Vec<String>> {
    match extract::extract_searchable_text(path) {
        Ok(Some(text)) => return Ok(text.lines().map(ToOwned::to_owned).collect()),
        Ok(None) => {}
        Err(error) => return Err(io::Error::other(error.to_string())),
    }
    let bytes = fs::read(path)?;
    let content = String::from_utf8_lossy(&bytes);
    Ok(content.lines().map(ToOwned::to_owned).collect())
}

fn render_report(report: &SearchReport) -> String {
    let mut output = String::new();
    output.push_str(&format!(
        "query: {}\nscanned_files: {}\ncandidate_lines: {}{}\n",
        report.query,
        report.scanned_files,
        report.candidate_lines,
        if report.truncated {
            " (truncated at candidate limit)"
        } else {
            ""
        }
    ));

    if report.results.is_empty() {
        output.push_str("results: 0\n");
        return output;
    }

    output.push_str(&format!("results: {}\n", report.results.len()));
    for result in &report.results {
        output.push('\n');
        output.push_str(&format!(
            "{}\n  score={:.3} matched_terms={}/{} match_lines={}\n",
            result.path.display(),
            result.score,
            result.matched_terms,
            report.total_terms.max(1),
            result.match_count
        ));
        for snippet in &result.snippets {
            output.push_str(&format!(
                "  snippet {}-{} score={:.3}\n",
                snippet.start_line, snippet.end_line, snippet.score
            ));
            for (line_number, text) in &snippet.lines {
                output.push_str(&format!("    {line_number:>6} | {text}\n"));
            }
        }
    }
    output.trim_end().to_owned()
}

fn render_file_list(files: &[PathBuf]) -> String {
    files
        .iter()
        .map(|path| path.display().to_string())
        .collect::<Vec<_>>()
        .join("\n")
}

fn render_count_list(counts: &[CountEntry], cli: &Cli) -> String {
    let show_path = show_path_in_standard_output(cli);
    counts
        .iter()
        .map(|entry| {
            if show_path {
                format!("{}:{}", entry.path.display(), entry.count)
            } else {
                entry.count.to_string()
            }
        })
        .collect::<Vec<_>>()
        .join("\n")
}

fn render_standard_output(events: &[MatchEvent], cli: &Cli) -> String {
    let blocks = build_output_blocks(events, cli);
    let mut lines = Vec::new();
    let use_color = color_enabled(cli);
    let show_path = show_path_in_standard_output(cli);
    let show_heading = cli.heading && show_path;
    let has_context = cli.before_context > 0 || cli.after_context > 0;
    let mut last_path: Option<&Path> = None;

    for (index, block) in blocks.iter().enumerate() {
        if show_heading && last_path != Some(block.path.as_path()) {
            if !lines.is_empty() {
                lines.push(String::new());
            }
            lines.push(block.path.display().to_string());
            last_path = Some(block.path.as_path());
        } else if has_context && index > 0 {
            lines.push("--".to_owned());
        }

        for line in &block.lines {
            let rendered_text = if use_color && line.is_match {
                highlight_text(&line.text, &line.submatches)
            } else {
                line.text.clone()
            };
            lines.push(render_standard_line(
                &block.path,
                line,
                &rendered_text,
                cli,
                show_heading,
            ));
        }
    }

    lines.join("\n")
}

fn render_json_output(
    file_results: &[JsonFileResult],
    totals: JsonTotals,
    elapsed_total: Duration,
) -> String {
    let mut lines = Vec::new();
    let mut total_bytes_printed = 0usize;

    for result in file_results {
        let file_lines = render_json_file(result);
        total_bytes_printed += rendered_bytes(&file_lines);
        lines.extend(file_lines);
    }

    lines.push(render_json_summary(
        &totals,
        elapsed_total,
        total_bytes_printed,
    ));
    lines.join("\n")
}

fn render_json_file(result: &JsonFileResult) -> Vec<String> {
    let path = json_escape(&result.path.display().to_string());
    let mut lines = Vec::new();
    lines.push(format!(
        "{{\"type\":\"begin\",\"data\":{{\"path\":{{\"text\":\"{}\"}}}}}}",
        path
    ));
    for event in &result.events {
        lines.push(render_json_line_event(&path, event));
    }
    let elapsed = json_duration(result.elapsed);
    let mut bytes_printed = rendered_bytes(&lines);
    let end_line = render_json_end(result, &path, &elapsed, bytes_printed);
    bytes_printed += end_line.len() + 1;
    let end_line = render_json_end(result, &path, &elapsed, bytes_printed);
    lines.push(end_line);
    lines
}

fn render_json_line_event(path: &str, event: &JsonLineEvent) -> String {
    let event_type = match event.kind {
        JsonLineEventKind::Context => "context",
        JsonLineEventKind::Match => "match",
    };
    let submatches = render_json_submatches(&event.text, &event.submatches);
    format!(
        "{{\"type\":\"{}\",\"data\":{{\"path\":{{\"text\":\"{}\"}},\"lines\":{{\"text\":\"{}\"}},\"line_number\":{},\"absolute_offset\":{},\"submatches\":[{}]}}}}",
        event_type,
        path,
        json_escape(&event.text),
        event.line_number,
        event.absolute_offset,
        submatches
    )
}

fn render_json_end(
    result: &JsonFileResult,
    path: &str,
    elapsed: &JsonDuration,
    bytes_printed: usize,
) -> String {
    format!(
        "{{\"type\":\"end\",\"data\":{{\"path\":{{\"text\":\"{}\"}},\"binary_offset\":{},\"stats\":{{\"elapsed\":{},\"searches\":1,\"searches_with_match\":1,\"bytes_searched\":{},\"bytes_printed\":{},\"matched_lines\":{},\"matches\":{}}}}}}}",
        path,
        json_optional_u64(result.binary_offset),
        elapsed.render(),
        result.bytes_searched,
        bytes_printed,
        result.matched_lines,
        result.matches
    )
}

fn render_json_summary(
    totals: &JsonTotals,
    elapsed_total: Duration,
    bytes_printed: usize,
) -> String {
    let elapsed = json_duration(elapsed_total);
    format!(
        "{{\"data\":{{\"elapsed_total\":{},\"stats\":{{\"bytes_printed\":{},\"bytes_searched\":{},\"elapsed\":{},\"matched_lines\":{},\"matches\":{},\"searches\":{},\"searches_with_match\":{}}}}},\"type\":\"summary\"}}",
        elapsed.render(),
        bytes_printed,
        totals.bytes_searched,
        elapsed.render(),
        totals.matched_lines,
        totals.matches,
        totals.searches,
        totals.searches_with_match
    )
}

fn render_json_submatches(text: &str, submatches: &[(usize, usize)]) -> String {
    submatches
        .iter()
        .map(|(start, end)| {
            format!(
                "{{\"match\":{{\"text\":\"{}\"}},\"start\":{},\"end\":{}}}",
                json_escape(&text[*start..*end]),
                start,
                end
            )
        })
        .collect::<Vec<_>>()
        .join(",")
}

fn rendered_bytes(lines: &[String]) -> usize {
    if lines.is_empty() {
        0
    } else {
        lines.iter().map(|line| line.len() + 1).sum::<usize>()
    }
}

fn build_output_blocks(events: &[MatchEvent], cli: &Cli) -> Vec<OutputBlock> {
    let mut blocks = Vec::new();
    let mut start = 0usize;

    while start < events.len() {
        let path = events[start].path.clone();
        let mut end = start + 1;
        while end < events.len() && events[end].path == path {
            end += 1;
        }
        blocks.extend(build_file_output_blocks(&path, &events[start..end], cli));
        start = end;
    }

    blocks
}

fn build_file_output_blocks(path: &Path, events: &[MatchEvent], cli: &Cli) -> Vec<OutputBlock> {
    let Ok(file_lines) = read_file_lines(path) else {
        return vec![OutputBlock {
            path: path.to_path_buf(),
            lines: events
                .iter()
                .map(|event| OutputLine {
                    line_number: event.line_number,
                    column: event.column,
                    text: event.text.clone(),
                    submatches: event.submatches.clone(),
                    is_match: true,
                })
                .collect(),
        }];
    };

    let mut ranges = Vec::<(usize, usize)>::new();
    for event in events {
        let start_line = event.line_number.saturating_sub(cli.before_context).max(1);
        let end_line = (event.line_number + cli.after_context).min(file_lines.len());
        if let Some(previous) = ranges.last_mut() {
            if start_line <= previous.1 + 1 {
                previous.1 = previous.1.max(end_line);
                continue;
            }
        }
        ranges.push((start_line, end_line));
    }

    ranges
        .into_iter()
        .map(|(start_line, end_line)| OutputBlock {
            path: path.to_path_buf(),
            lines: (start_line..=end_line)
                .map(|line_number| {
                    if let Some(event) = events.iter().find(|item| item.line_number == line_number)
                    {
                        OutputLine {
                            line_number,
                            column: event.column,
                            text: event.text.clone(),
                            submatches: event.submatches.clone(),
                            is_match: true,
                        }
                    } else {
                        OutputLine {
                            line_number,
                            column: None,
                            text: file_lines[line_number - 1].clone(),
                            submatches: Vec::new(),
                            is_match: false,
                        }
                    }
                })
                .collect(),
        })
        .collect()
}

fn render_standard_line(
    path: &Path,
    line: &OutputLine,
    rendered_text: &str,
    cli: &Cli,
    show_heading: bool,
) -> String {
    let separator = if line.is_match { ":" } else { "-" };
    let show_line_numbers =
        cli.line_numbers || cli.show_column || cli.before_context > 0 || cli.after_context > 0;

    if show_heading {
        render_standard_suffix(line, rendered_text, show_line_numbers, separator)
    } else if show_path_in_standard_output(cli) {
        format!(
            "{}{}{}",
            path.display(),
            separator,
            render_standard_suffix(line, rendered_text, show_line_numbers, separator)
        )
    } else {
        render_standard_suffix(line, rendered_text, show_line_numbers, separator)
    }
}

fn render_standard_suffix(
    line: &OutputLine,
    rendered_text: &str,
    show_line_numbers: bool,
    separator: &str,
) -> String {
    if show_line_numbers {
        if let Some(column) = line.column {
            if line.is_match {
                format!(
                    "{}{}{}:{}",
                    line.line_number, separator, column, rendered_text
                )
            } else {
                format!("{}{}{}", line.line_number, separator, rendered_text)
            }
        } else {
            format!("{}{}{}", line.line_number, separator, rendered_text)
        }
    } else {
        rendered_text.to_owned()
    }
}

fn color_enabled(cli: &Cli) -> bool {
    match cli.color_mode {
        ColorMode::Always => true,
        ColorMode::Never => false,
        ColorMode::Auto => io::stdout().is_terminal(),
    }
}

fn ranked_before_context(cli: &Cli) -> usize {
    if cli.context_explicit {
        cli.before_context
    } else {
        DEFAULT_CONTEXT_LINES
    }
}

fn ranked_after_context(cli: &Cli) -> usize {
    if cli.context_explicit {
        cli.after_context
    } else {
        DEFAULT_CONTEXT_LINES
    }
}

fn highlight_text(text: &str, submatches: &[(usize, usize)]) -> String {
    if submatches.is_empty() {
        return text.to_owned();
    }
    let bytes = text.as_bytes();
    let mut rendered = String::new();
    let mut last = 0usize;
    for (start, end) in submatches {
        if *start > last {
            rendered.push_str(&String::from_utf8_lossy(&bytes[last..*start]));
        }
        rendered.push_str("\u{1b}[31m");
        rendered.push_str(&String::from_utf8_lossy(&bytes[*start..*end]));
        rendered.push_str("\u{1b}[0m");
        last = *end;
    }
    if last < bytes.len() {
        rendered.push_str(&String::from_utf8_lossy(&bytes[last..]));
    }
    rendered
}

fn find_submatches(matcher: &grep_regex::RegexMatcher, line_bytes: &[u8]) -> Vec<(usize, usize)> {
    let mut submatches = Vec::new();
    let mut at = 0usize;
    while at <= line_bytes.len() {
        let Some(found) = matcher.find_at(line_bytes, at).ok().flatten() else {
            break;
        };
        submatches.push((found.start(), found.end()));
        at = if found.end() > at {
            found.end()
        } else {
            at + 1
        };
    }
    submatches
}

fn trim_line_ending_bytes(line: &[u8]) -> &[u8] {
    if let Some(stripped) = line.strip_suffix(b"\r\n") {
        stripped
    } else if let Some(stripped) = line.strip_suffix(b"\n") {
        stripped
    } else if let Some(stripped) = line.strip_suffix(b"\r") {
        stripped
    } else {
        line
    }
}

fn json_escape(text: &str) -> String {
    let mut escaped = String::with_capacity(text.len());
    for character in text.chars() {
        match character {
            '"' => escaped.push_str("\\\""),
            '\\' => escaped.push_str("\\\\"),
            '\n' => escaped.push_str("\\n"),
            '\r' => escaped.push_str("\\r"),
            '\t' => escaped.push_str("\\t"),
            c if c.is_control() => escaped.push_str(&format!("\\u{:04x}", c as u32)),
            c => escaped.push(c),
        }
    }
    escaped
}

struct JsonDuration {
    secs: u64,
    nanos: u32,
    human: String,
}

impl JsonDuration {
    fn render(&self) -> String {
        format!(
            "{{\"secs\":{},\"nanos\":{},\"human\":\"{}\"}}",
            self.secs, self.nanos, self.human
        )
    }
}

fn json_duration(duration: Duration) -> JsonDuration {
    JsonDuration {
        secs: duration.as_secs(),
        nanos: duration.subsec_nanos(),
        human: format!("{:.6}s", duration.as_secs_f64()),
    }
}

fn json_optional_u64(value: Option<u64>) -> String {
    value
        .map(|number| number.to_string())
        .unwrap_or_else(|| "null".to_owned())
}

fn show_path_in_standard_output(cli: &Cli) -> bool {
    !(cli.roots.len() == 1 && cli.roots[0].is_file())
}

fn score_line_against_query(query: &Query, normalized_text: &str) -> (Vec<usize>, f64) {
    let tokens = tokenize(normalized_text);
    let mut matched_terms = Vec::new();
    let mut score = 0.0;

    for (index, term) in query.terms.iter().enumerate() {
        let exact_hits = count_exact_hits(&tokens, term);
        if exact_hits > 0 {
            matched_terms.push(index);
            score += 1.0 + (exact_hits.saturating_sub(1) as f64 * 0.2);
            continue;
        }
        if contains_partial_token(&tokens, term) {
            matched_terms.push(index);
            score += PARTIAL_HIT_WEIGHT;
        }
    }

    if matched_terms.len() > 1 {
        score += SAME_LINE_TERM_BONUS * (matched_terms.len() - 1) as f64;
    }
    if normalized_text.contains(&query.normalized_phrase) {
        score += PHRASE_BONUS;
    }
    (matched_terms, score)
}

fn minimal_cover_span(term_count: usize, matches: &[LineMatchRecord]) -> Option<usize> {
    if term_count == 0 {
        return None;
    }
    let mut counts = vec![0usize; term_count];
    let mut covered = 0usize;
    let mut best_span: Option<usize> = None;
    let mut start = 0usize;

    for end in 0..matches.len() {
        for term_index in unique_term_indexes(&matches[end].matched_terms) {
            if counts[*term_index] == 0 {
                covered += 1;
            }
            counts[*term_index] += 1;
        }

        while covered == term_count && start <= end {
            let span = matches[end]
                .line_number
                .saturating_sub(matches[start].line_number)
                + 1;
            best_span = Some(best_span.map_or(span, |current| current.min(span)));

            for term_index in unique_term_indexes(&matches[start].matched_terms) {
                counts[*term_index] = counts[*term_index].saturating_sub(1);
                if counts[*term_index] == 0 {
                    covered = covered.saturating_sub(1);
                }
            }
            start += 1;
        }
    }

    best_span
}

fn unique_term_indexes(terms: &[usize]) -> BTreeSet<&usize> {
    terms.iter().collect()
}

fn best_same_line_bonus(matches: &[LineMatchRecord]) -> f64 {
    let max_terms = matches
        .iter()
        .map(|item| unique_term_indexes(&item.matched_terms).len())
        .max()
        .unwrap_or(0);
    if max_terms <= 1 {
        0.0
    } else {
        SAME_LINE_TERM_BONUS * (max_terms - 1) as f64
    }
}

fn matched_term_count(query: &Query, candidate: &FileCandidate, body_tokens: &[String]) -> usize {
    query
        .terms
        .iter()
        .filter(|term| {
            count_exact_hits(body_tokens, term) > 0
                || count_partial_hits(body_tokens, term, false) > 0
                || count_exact_hits(&candidate.path_tokens, term) > 0
                || contains_partial_token(&candidate.path_tokens, term)
        })
        .count()
}

fn body_tokens(candidate: &FileCandidate) -> Vec<String> {
    let mut tokens = Vec::new();
    for matched_line in &candidate.matches {
        tokens.extend(tokenize(&matched_line.normalized_text));
    }
    tokens
}

fn join_normalized_lines(lines: &[LineMatchRecord]) -> String {
    let mut joined = String::new();
    for line in lines {
        if !joined.is_empty() {
            joined.push(' ');
        }
        joined.push_str(&line.normalized_text);
    }
    joined
}

fn count_exact_hits(tokens: &[String], term: &str) -> usize {
    tokens.iter().filter(|token| token.as_str() == term).count()
}

fn count_partial_hits(tokens: &[String], term: &str, skip_exact: bool) -> usize {
    tokens
        .iter()
        .filter(|token| {
            let token = token.as_str();
            (!skip_exact || token != term) && token != term && token.contains(term)
        })
        .count()
}

fn contains_partial_token(tokens: &[String], term: &str) -> bool {
    tokens
        .iter()
        .any(|token| token.as_str() != term && token.contains(term))
}

fn tokenize(text: &str) -> Vec<String> {
    text.split_whitespace().map(ToOwned::to_owned).collect()
}

fn trim_line_ending(text: &str) -> &str {
    text.trim_end_matches(['\r', '\n'])
}

fn normalize_text(text: &str) -> String {
    let mut normalized = String::with_capacity(text.len());
    let mut previous_was_space = true;
    let mut previous_was_lower_or_digit = false;

    for character in text.chars() {
        if character.is_ascii_alphanumeric() {
            if character.is_ascii_uppercase() && previous_was_lower_or_digit && !previous_was_space
            {
                normalized.push(' ');
            }
            normalized.push(character.to_ascii_lowercase());
            previous_was_space = false;
            previous_was_lower_or_digit =
                character.is_ascii_lowercase() || character.is_ascii_digit();
        } else {
            if !previous_was_space {
                normalized.push(' ');
                previous_was_space = true;
            }
            previous_was_lower_or_digit = false;
        }
    }

    normalized.split_whitespace().collect::<Vec<_>>().join(" ")
}

impl Query {
    fn from_raw(raw: &str) -> Result<Self, Box<dyn Error>> {
        let normalized_phrase = normalize_text(raw);
        if normalized_phrase.is_empty() {
            return Err("query must contain at least one searchable term".into());
        }
        let mut terms = Vec::new();
        let mut seen = BTreeSet::new();
        for token in normalized_phrase.split_whitespace() {
            if seen.insert(token.to_owned()) {
                terms.push(token.to_owned());
            }
        }
        Ok(Self {
            normalized_phrase,
            terms,
        })
    }
}

impl FileCandidate {
    fn new(path: PathBuf, matches: Vec<LineMatchRecord>) -> Self {
        let normalized_path = normalize_text(&path.to_string_lossy());
        let path_tokens = tokenize(&normalized_path);
        Self {
            path,
            normalized_path,
            path_tokens,
            matches,
        }
    }
}

impl CorpusStats {
    fn from_candidates(query: &Query, candidates: &[FileCandidate]) -> Self {
        let total_docs = candidates.len().max(1);
        let mut total_doc_len = 0usize;
        let mut document_frequency = vec![0usize; query.terms.len()];

        for candidate in candidates {
            let body_tokens = body_tokens(candidate);
            total_doc_len += body_tokens.len().max(1);
            for (index, term) in query.terms.iter().enumerate() {
                if count_exact_hits(&body_tokens, term) > 0
                    || count_partial_hits(&body_tokens, term, false) > 0
                    || count_exact_hits(&candidate.path_tokens, term) > 0
                    || contains_partial_token(&candidate.path_tokens, term)
                {
                    document_frequency[index] += 1;
                }
            }
        }

        Self {
            total_docs,
            average_doc_len: total_doc_len as f64 / total_docs as f64,
            document_frequency,
        }
    }
}

fn clone_snippet(snippet: &Snippet) -> Snippet {
    Snippet {
        start_line: snippet.start_line,
        end_line: snippet.end_line,
        score: snippet.score,
        lines: snippet.lines.clone(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use flate2::Compression;
    use flate2::write::GzEncoder;
    use std::fs::File;
    use std::io::{Cursor, Write};
    use std::process;
    use std::time::{SystemTime, UNIX_EPOCH};
    use tar::Builder as TarBuilder;
    use zip::ZipWriter;
    use zip::write::SimpleFileOptions;

    fn test_cli(root: &Path, query: Option<&str>) -> Cli {
        Cli {
            query: query.map(ToOwned::to_owned),
            roots: vec![root.to_path_buf()],
            files_mode: query.is_none(),
            debug: false,
            match_mode: MatchMode::Regex,
            case_mode: CaseMode::Smart,
            output_mode: SearchOutputMode::Ranked,
            show_all: false,
            top_k: DEFAULT_TOP_K,
            max_candidate_lines: DEFAULT_MAX_CANDIDATE_LINES,
            before_context: 0,
            after_context: 0,
            context_explicit: false,
            max_snippets: DEFAULT_MAX_SNIPPETS,
            globs: Vec::new(),
            type_names: Vec::new(),
            type_not_names: Vec::new(),
            line_numbers: false,
            show_column: false,
            heading: false,
            color_mode: ColorMode::Never,
            word_regexp: false,
            line_regexp: false,
            hidden: false,
            no_ignore: false,
            follow_links: false,
        }
    }

    fn parse_test_cli(args: impl IntoIterator<Item = String>) -> Cli {
        match parse_args_from(args).expect("parse args") {
            ParseOutcome::Cli(cli) => cli,
            ParseOutcome::Help => panic!("expected cli parse result, got help"),
            ParseOutcome::Version => panic!("expected cli parse result, got version"),
        }
    }

    #[test]
    fn normalize_text_splits_camel_case_and_symbols() {
        assert_eq!(
            normalize_text("retryTimeout config.go"),
            "retry timeout config go"
        );
    }

    #[test]
    fn ranking_prefers_balanced_match_over_repetition() {
        let query = Query::from_raw("timeout config").expect("query");
        let balanced = FileCandidate::new(
            PathBuf::from("config.go"),
            vec![LineMatchRecord {
                line_number: 12,
                text: "timeout := 30 config loaded".to_owned(),
                normalized_text: normalize_text("timeout := 30 config loaded"),
                matched_terms: vec![0, 1],
                score_hint: 3.0,
            }],
        );
        let repeated = FileCandidate::new(
            PathBuf::from("server.go"),
            vec![LineMatchRecord {
                line_number: 8,
                text: "timeout timeout timeout".to_owned(),
                normalized_text: normalize_text("timeout timeout timeout"),
                matched_terms: vec![0],
                score_hint: 1.5,
            }],
        );
        let ranked = rank_candidates(&query, vec![repeated, balanced]);
        assert_eq!(ranked[0].path, PathBuf::from("config.go"));
    }

    #[test]
    fn execute_search_ranks_expected_file_first() {
        let root = create_test_dir("rank");
        fs::write(root.join("config.go"), "timeout := 30\nconfig loaded\n")
            .expect("write config.go");
        fs::write(root.join("server.go"), "cfg.Timeout = 30\n").expect("write server.go");
        fs::write(root.join("utils.go"), "retryTimeout := 5\n").expect("write utils.go");

        let cli = Cli {
            match_mode: MatchMode::FixedStrings,
            top_k: 3,
            before_context: 1,
            after_context: 1,
            context_explicit: true,
            max_snippets: 2,
            ..test_cli(&root, Some("timeout config"))
        };
        let report = execute_search(&cli).expect("report");
        assert_eq!(
            report.results.first().map(|item| item.path.file_name()),
            Some(Some(std::ffi::OsStr::new("config.go")))
        );

        fs::remove_dir_all(root).expect("cleanup");
    }

    #[test]
    fn execute_search_respects_gitignore_by_default() {
        let root = create_test_dir("ignore");
        fs::write(root.join(".gitignore"), "ignored.txt\n").expect("write .gitignore");
        fs::write(root.join("ignored.txt"), "timeout config\n").expect("write ignored.txt");
        fs::write(root.join("visible.txt"), "timeout config\n").expect("write visible.txt");

        let cli = Cli {
            match_mode: MatchMode::FixedStrings,
            top_k: 5,
            before_context: 1,
            after_context: 1,
            context_explicit: true,
            max_snippets: 2,
            ..test_cli(&root, Some("timeout config"))
        };
        let report = execute_search(&cli).expect("report");
        assert_eq!(report.results.len(), 1);
        assert_eq!(
            report.results[0].path.file_name(),
            Some(std::ffi::OsStr::new("visible.txt"))
        );

        fs::remove_dir_all(root).expect("cleanup");
    }

    #[test]
    fn parse_args_accepts_files_mode_without_query() {
        let cli = parse_test_cli(["--files".to_owned(), "./hypotheses".to_owned()]);
        assert!(cli.files_mode);
        assert!(cli.query.is_none());
        assert_eq!(cli.roots, vec![PathBuf::from("./hypotheses")]);
        assert_eq!(cli.match_mode, MatchMode::Regex);
        assert_eq!(cli.case_mode, CaseMode::Smart);
    }

    #[test]
    fn parse_args_accepts_fixed_strings_mode() {
        let cli = parse_test_cli([
            "-F".to_owned(),
            "cortisol level".to_owned(),
            "./hypotheses".to_owned(),
        ]);
        assert!(!cli.files_mode);
        assert_eq!(cli.query.as_deref(), Some("cortisol level"));
        assert_eq!(cli.match_mode, MatchMode::FixedStrings);
    }

    #[test]
    fn parse_args_accepts_common_rg_flags() {
        let cli = parse_test_cli([
            "--debug".to_owned(),
            "-i".to_owned(),
            "-L".to_owned(),
            "-g*.rs".to_owned(),
            "-tpy".to_owned(),
            "-Tjson".to_owned(),
            "-A2".to_owned(),
            "-B1".to_owned(),
            "-w".to_owned(),
            "-x".to_owned(),
            "-c".to_owned(),
            "python".to_owned(),
            ".".to_owned(),
        ]);
        assert_eq!(cli.case_mode, CaseMode::Insensitive);
        assert_eq!(cli.globs, vec!["*.rs".to_owned()]);
        assert_eq!(cli.type_names, vec!["py".to_owned()]);
        assert_eq!(cli.type_not_names, vec!["json".to_owned()]);
        assert_eq!(cli.before_context, 1);
        assert_eq!(cli.after_context, 2);
        assert!(cli.debug);
        assert!(cli.word_regexp);
        assert!(cli.line_regexp);
        assert!(cli.follow_links);
        assert_eq!(cli.output_mode, SearchOutputMode::Count);
    }

    #[test]
    fn parse_args_accepts_long_files_without_match_flag() {
        let cli = parse_test_cli([
            "--files-without-match".to_owned(),
            "python".to_owned(),
            ".".to_owned(),
        ]);
        assert_eq!(cli.output_mode, SearchOutputMode::FilesWithoutMatch);
    }

    #[test]
    fn search_warning_message_requires_debug() {
        let root = create_test_dir("debug-warning");
        let quiet_cli = test_cli(&root, Some("python"));
        let debug_cli = Cli {
            debug: true,
            ..test_cli(&root, Some("python"))
        };
        let error = io::Error::other("pdf extraction panicked: boom");

        assert!(search_warning_message(&quiet_cli, Path::new("broken.pdf"), &error).is_none());
        assert_eq!(
            search_warning_message(&debug_cli, Path::new("broken.pdf"), &error),
            Some("warning: failed to search broken.pdf: pdf extraction panicked: boom".to_owned())
        );

        fs::remove_dir_all(root).expect("cleanup");
    }

    #[test]
    fn parse_args_accepts_ranked_all_flag() {
        let cli = parse_test_cli([
            "--ranked".to_owned(),
            "--all".to_owned(),
            "timeout".to_owned(),
            ".".to_owned(),
        ]);
        assert_eq!(cli.output_mode, SearchOutputMode::Ranked);
        assert!(cli.show_all);
    }

    #[test]
    fn parse_args_accepts_version_flag() {
        let outcome = parse_args_from(["--version".to_owned()]).expect("parse args");
        assert!(matches!(outcome, ParseOutcome::Version));
    }

    #[test]
    fn help_text_includes_version_banner() {
        let help = help_text();
        assert!(help.starts_with(&format!("{VERSION_TEXT}: {HELP_SUMMARY}")));
    }

    #[test]
    fn list_files_respects_gitignore_by_default() {
        let root = create_test_dir("files-ignore");
        fs::write(root.join(".gitignore"), "ignored.txt\n").expect("write .gitignore");
        fs::write(root.join("ignored.txt"), "ignored\n").expect("write ignored.txt");
        fs::write(root.join("visible.txt"), "visible\n").expect("write visible.txt");

        let cli = test_cli(&root, None);
        let files = list_files(&cli).expect("list files");
        assert_eq!(files.len(), 1);
        assert_eq!(
            files[0].file_name(),
            Some(std::ffi::OsStr::new("visible.txt"))
        );

        fs::remove_dir_all(root).expect("cleanup");
    }

    #[test]
    fn execute_search_supports_regex_alternation_by_default() {
        let root = create_test_dir("regex");
        fs::write(root.join("alpha.txt"), "whisper is enabled\n").expect("write alpha.txt");
        fs::write(root.join("beta.txt"), "python worker\n").expect("write beta.txt");
        fs::write(root.join("gamma.txt"), "rust only\n").expect("write gamma.txt");

        let cli = Cli {
            top_k: 10,
            before_context: 1,
            after_context: 1,
            context_explicit: true,
            max_snippets: 1,
            ..test_cli(&root, Some("whisper|process_all_mp4s|python"))
        };
        let report = execute_search(&cli).expect("report");
        let file_names: Vec<_> = report
            .results
            .iter()
            .filter_map(|item| item.path.file_name().and_then(|name| name.to_str()))
            .collect();
        assert!(file_names.contains(&"alpha.txt"));
        assert!(file_names.contains(&"beta.txt"));
        assert!(!file_names.contains(&"gamma.txt"));

        fs::remove_dir_all(root).expect("cleanup");
    }

    #[test]
    fn ranked_all_shows_all_files_and_snippets() {
        let root = create_test_dir("ranked-all");
        fs::write(root.join("one.txt"), "alpha term\nterm again\n").expect("write one.txt");
        fs::write(root.join("two.txt"), "term here\nmiddle\nterm there\n").expect("write two.txt");

        let limited_cli = Cli {
            top_k: 1,
            before_context: 0,
            after_context: 0,
            context_explicit: true,
            max_snippets: 1,
            ..test_cli(&root, Some("term"))
        };
        let limited = execute_search(&limited_cli).expect("limited search");
        assert_eq!(limited.results.len(), 1);
        assert_eq!(limited.results[0].snippets.len(), 1);

        let all_cli = Cli {
            show_all: true,
            top_k: 1,
            before_context: 0,
            after_context: 0,
            context_explicit: true,
            max_snippets: 1,
            ..test_cli(&root, Some("term"))
        };
        let all = execute_search(&all_cli).expect("all search");
        assert_eq!(all.results.len(), 2);
        assert!(all.results.iter().any(|result| result.snippets.len() > 1));

        fs::remove_dir_all(root).expect("cleanup");
    }

    #[test]
    fn execute_search_respects_case_modes() {
        let root = create_test_dir("case");
        fs::write(root.join("sample.txt"), "Timeout value\n").expect("write sample.txt");

        let sensitive = Cli {
            case_mode: CaseMode::Sensitive,
            top_k: 5,
            before_context: 0,
            after_context: 0,
            context_explicit: true,
            max_snippets: 1,
            ..test_cli(&root, Some("timeout"))
        };
        assert_eq!(
            execute_search(&sensitive).expect("sensitive").results.len(),
            0
        );

        let insensitive = Cli {
            case_mode: CaseMode::Insensitive,
            ..sensitive.clone()
        };
        assert_eq!(
            execute_search(&insensitive)
                .expect("insensitive")
                .results
                .len(),
            1
        );

        fs::remove_dir_all(root).expect("cleanup");
    }

    #[test]
    fn execute_search_respects_glob_and_type_filters() {
        let root = create_test_dir("filters");
        fs::write(root.join("keep.rs"), "timeout config\n").expect("write keep.rs");
        fs::write(root.join("skip.py"), "timeout config\n").expect("write skip.py");

        let glob_cli = Cli {
            top_k: 5,
            before_context: 0,
            after_context: 0,
            context_explicit: true,
            max_snippets: 1,
            globs: vec!["*.rs".to_owned()],
            ..test_cli(&root, Some("timeout"))
        };
        let glob_report = execute_search(&glob_cli).expect("glob report");
        assert_eq!(glob_report.results.len(), 1);
        assert_eq!(
            glob_report.results[0].path.file_name(),
            Some(std::ffi::OsStr::new("keep.rs"))
        );

        let type_cli = Cli {
            globs: Vec::new(),
            type_names: vec!["py".to_owned()],
            ..glob_cli
        };
        let type_report = execute_search(&type_cli).expect("type report");
        assert_eq!(type_report.results.len(), 1);
        assert_eq!(
            type_report.results[0].path.file_name(),
            Some(std::ffi::OsStr::new("skip.py"))
        );

        fs::remove_dir_all(root).expect("cleanup");
    }

    #[test]
    fn execute_search_respects_word_and_line_regexp() {
        let root = create_test_dir("boundaries");
        fs::write(root.join("sample.txt"), "cpython\npython\npython worker\n")
            .expect("write sample.txt");

        let word_cli = Cli {
            top_k: 5,
            before_context: 0,
            after_context: 0,
            context_explicit: true,
            max_snippets: 5,
            word_regexp: true,
            ..test_cli(&root, Some("python"))
        };
        let word_report = execute_search(&word_cli).expect("word report");
        assert_eq!(word_report.results.len(), 1);
        assert_eq!(word_report.results[0].match_count, 2);

        let line_cli = Cli {
            line_regexp: true,
            ..word_cli
        };
        let line_report = execute_search(&line_cli).expect("line report");
        assert_eq!(line_report.results.len(), 1);
        assert_eq!(line_report.results[0].match_count, 1);

        fs::remove_dir_all(root).expect("cleanup");
    }

    #[test]
    fn execute_output_modes_and_context_work() {
        let root = create_test_dir("outputs");
        fs::write(root.join("one.txt"), "zero\none hit\ntwo\nthree\n").expect("write one.txt");
        fs::write(root.join("two.txt"), "zero\n").expect("write two.txt");

        let base_cli = Cli {
            top_k: 5,
            before_context: 1,
            after_context: 2,
            context_explicit: true,
            max_snippets: 5,
            ..test_cli(&root, Some("hit"))
        };

        let report = execute_search(&base_cli).expect("report");
        assert_eq!(report.results.len(), 1);
        assert_eq!(report.results[0].snippets[0].start_line, 1);
        assert_eq!(report.results[0].snippets[0].end_line, 4);

        let files_with_matches = execute_files_with_matches(&base_cli).expect("files with matches");
        assert_eq!(files_with_matches.len(), 1);
        assert_eq!(
            files_with_matches[0].file_name(),
            Some(std::ffi::OsStr::new("one.txt"))
        );

        let files_without_match =
            execute_files_without_match(&base_cli).expect("files without match");
        assert_eq!(files_without_match.len(), 1);
        assert_eq!(
            files_without_match[0].file_name(),
            Some(std::ffi::OsStr::new("two.txt"))
        );

        let counts = execute_count(&base_cli).expect("counts");
        assert_eq!(counts.len(), 1);
        assert_eq!(counts[0].count, 1);

        fs::remove_dir_all(root).expect("cleanup");
    }

    #[test]
    fn extracted_docx_snippets_use_human_readable_text() {
        let root = create_test_dir("docx-snippet");
        let file = root.join("sample.docx");
        create_test_docx(
            &file,
            &[
                "Human readable heading",
                "cortisol level in serum",
                "closing notes",
            ],
        );

        let cli = Cli {
            match_mode: MatchMode::FixedStrings,
            before_context: 1,
            after_context: 1,
            context_explicit: true,
            ..test_cli(&file, Some("cortisol"))
        };
        let report = execute_search(&cli).expect("report");
        assert_eq!(report.results.len(), 1);
        let snippet_lines: Vec<_> = report.results[0].snippets[0]
            .lines
            .iter()
            .map(|(_, text)| text.as_str())
            .collect();
        assert_eq!(
            snippet_lines,
            vec![
                "Human readable heading",
                "cortisol level in serum",
                "closing notes",
            ]
        );

        fs::remove_dir_all(root).expect("cleanup");
    }

    #[test]
    fn execute_search_supports_zip_archives() {
        let root = create_test_dir("zip-archive");
        let file = root.join("bundle.zip");
        write_zip(
            &file,
            &[(
                "notes.txt".to_owned(),
                b"archived cortisol finding\nsecondary line\n".to_vec(),
            )],
        );

        let cli = Cli {
            match_mode: MatchMode::FixedStrings,
            before_context: 1,
            after_context: 1,
            context_explicit: true,
            ..test_cli(&file, Some("cortisol"))
        };
        let report = execute_search(&cli).expect("report");
        assert_eq!(report.results.len(), 1);
        assert_eq!(
            report.results[0].path.file_name(),
            Some(std::ffi::OsStr::new("bundle.zip"))
        );
        let snippet_text = report.results[0].snippets[0]
            .lines
            .iter()
            .map(|(_, text)| text.as_str())
            .collect::<Vec<_>>()
            .join("\n");
        assert!(snippet_text.contains("Archive entry: notes.txt"));
        assert!(snippet_text.contains("archived cortisol finding"));

        fs::remove_dir_all(root).expect("cleanup");
    }

    #[test]
    fn execute_search_supports_tar_and_tgz_archives() {
        let root = create_test_dir("tar-archive");
        let tar_file = root.join("bundle.tar");
        let tgz_file = root.join("bundle.tgz");
        let entries = vec![(
            "sheet.txt".to_owned(),
            b"aldosterone archive payload\n".to_vec(),
        )];
        write_tar(&tar_file, &entries);
        write_tgz(&tgz_file, &entries);

        let cli = Cli {
            match_mode: MatchMode::FixedStrings,
            top_k: 10,
            before_context: 0,
            after_context: 0,
            context_explicit: true,
            ..test_cli(&root, Some("aldosterone"))
        };
        let report = execute_search(&cli).expect("report");
        let file_names = report
            .results
            .iter()
            .filter_map(|result| result.path.file_name().and_then(|name| name.to_str()))
            .collect::<Vec<_>>();
        assert!(file_names.contains(&"bundle.tar"));
        assert!(file_names.contains(&"bundle.tgz"));

        fs::remove_dir_all(root).expect("cleanup");
    }

    #[test]
    fn execute_search_supports_nested_archives() {
        let root = create_test_dir("nested-archive");
        let file = root.join("outer.zip");
        let inner_tar = tar_bytes(&[("deep.txt".to_owned(), b"nested tumor evidence\n".to_vec())]);
        write_zip(&file, &[("inner.tar".to_owned(), inner_tar)]);

        let cli = Cli {
            match_mode: MatchMode::FixedStrings,
            before_context: 2,
            after_context: 1,
            context_explicit: true,
            ..test_cli(&file, Some("tumor"))
        };
        let report = execute_search(&cli).expect("report");
        assert_eq!(report.results.len(), 1);
        let snippet_text = report.results[0].snippets[0]
            .lines
            .iter()
            .map(|(_, text)| text.as_str())
            .collect::<Vec<_>>()
            .join("\n");
        assert!(snippet_text.contains("Archive entry: inner.tar"));
        assert!(snippet_text.contains("Archive entry: deep.txt"));
        assert!(snippet_text.contains("nested tumor evidence"));

        fs::remove_dir_all(root).expect("cleanup");
    }

    #[test]
    fn count_output_omits_path_for_single_file() {
        let root = create_test_dir("count-single");
        let file = root.join("sample.txt");
        fs::write(&file, "foo\nfoo\n").expect("write sample.txt");

        let cli = Cli {
            output_mode: SearchOutputMode::Count,
            ..test_cli(&file, Some("foo"))
        };
        let counts = execute_count(&cli).expect("counts");
        let output = render_count_list(&counts, &cli);
        assert_eq!(output, "2");

        fs::remove_dir_all(root).expect("cleanup");
    }

    #[test]
    fn json_output_includes_rg_style_events_and_offsets() {
        let root = create_test_dir("json");
        let file = root.join("sample.txt");
        fs::write(&file, "zero\none hit\ntwo\n").expect("write sample.txt");

        let cli = Cli {
            output_mode: SearchOutputMode::Json,
            before_context: 1,
            after_context: 1,
            context_explicit: true,
            ..test_cli(&file, Some("hit"))
        };
        let result = execute_json_search(&cli).expect("json output");
        assert!(result.had_match);
        let output = result.output;
        assert!(output.contains("\"type\":\"begin\""));
        assert!(output.contains("\"type\":\"context\""));
        assert!(output.contains("\"type\":\"match\""));
        assert!(output.contains("\"type\":\"end\""));
        assert!(output.contains("\"type\":\"summary\""));
        assert!(output.contains("\"absolute_offset\":5"));

        fs::remove_dir_all(root).expect("cleanup");
    }

    #[test]
    fn json_output_reports_summary_for_no_match() {
        let root = create_test_dir("json-nohit");
        let file = root.join("sample.txt");
        fs::write(&file, "bar\n").expect("write sample.txt");

        let cli = Cli {
            output_mode: SearchOutputMode::Json,
            ..test_cli(&file, Some("foo"))
        };
        let result = execute_json_search(&cli).expect("json output");
        assert!(!result.had_match);
        let output = result.output;
        assert!(!output.contains("\"type\":\"begin\""));
        assert!(output.contains("\"type\":\"summary\""));
        assert!(output.contains("\"searches\":1"));
        assert!(output.contains("\"searches_with_match\":0"));

        fs::remove_dir_all(root).expect("cleanup");
    }

    #[test]
    fn run_returns_no_match_exit_code_for_standard_search() {
        let root = create_test_dir("exit-no-match");
        let file = root.join("sample.txt");
        fs::write(&file, "bar\n").expect("write sample.txt");

        let cli = Cli {
            output_mode: SearchOutputMode::Standard,
            ..test_cli(&file, Some("foo"))
        };
        let outcome = execute_cli(&cli).expect("outcome");
        assert_eq!(outcome.exit_code, 1);
        assert!(outcome.output.is_empty());

        fs::remove_dir_all(root).expect("cleanup");
    }

    #[test]
    fn standard_output_omits_path_for_single_file_by_default() {
        let root = create_test_dir("standard-single");
        let file = root.join("sample.txt");
        fs::write(&file, "zero\none hit\ntwo\n").expect("write sample.txt");

        let cli = Cli {
            output_mode: SearchOutputMode::Standard,
            ..test_cli(&file, Some("hit"))
        };
        let events = execute_standard_search(&cli).expect("events");
        let output = render_standard_output(&events, &cli);
        assert_eq!(output, "one hit");

        fs::remove_dir_all(root).expect("cleanup");
    }

    #[test]
    fn standard_output_context_uses_rg_style_separators() {
        let root = create_test_dir("standard-context");
        let file = root.join("sample.txt");
        fs::write(&file, "zero\none hit\ntwo\n").expect("write sample.txt");

        let cli = Cli {
            output_mode: SearchOutputMode::Standard,
            before_context: 1,
            after_context: 1,
            context_explicit: true,
            ..test_cli(&file, Some("hit"))
        };
        let events = execute_standard_search(&cli).expect("events");
        let output = render_standard_output(&events, &cli);
        assert_eq!(output, "1-zero\n2:one hit\n3-two");

        fs::remove_dir_all(root).expect("cleanup");
    }

    fn create_test_dir(label: &str) -> PathBuf {
        let timestamp = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("clock")
            .as_nanos();
        let root = env::temp_dir().join(format!("rgrank-{label}-{}-{timestamp}", process::id()));
        fs::create_dir_all(&root).expect("create test dir");
        root
    }

    fn create_test_docx(path: &Path, paragraphs: &[&str]) {
        let file = File::create(path).expect("create docx");
        let mut zip = ZipWriter::new(file);
        let options = SimpleFileOptions::default();
        zip.start_file("word/document.xml", options)
            .expect("start document.xml");

        let mut xml = String::from(
            r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<w:document xmlns:w="http://schemas.openxmlformats.org/wordprocessingml/2006/main">
  <w:body>"#,
        );
        for paragraph in paragraphs {
            xml.push_str("<w:p><w:r><w:t>");
            xml.push_str(paragraph);
            xml.push_str("</w:t></w:r></w:p>");
        }
        xml.push_str("</w:body></w:document>");

        zip.write_all(xml.as_bytes()).expect("write document.xml");
        zip.finish().expect("finish docx");
    }

    fn write_zip(path: &Path, entries: &[(String, Vec<u8>)]) {
        let file = File::create(path).expect("create zip");
        let mut zip = ZipWriter::new(file);
        let options = SimpleFileOptions::default();
        for (name, contents) in entries {
            zip.start_file(name, options).expect("start zip entry");
            zip.write_all(contents).expect("write zip entry");
        }
        zip.finish().expect("finish zip");
    }

    fn write_tar(path: &Path, entries: &[(String, Vec<u8>)]) {
        fs::write(path, tar_bytes(entries)).expect("write tar");
    }

    fn write_tgz(path: &Path, entries: &[(String, Vec<u8>)]) {
        let tar_data = tar_bytes(entries);
        let mut encoder = GzEncoder::new(Vec::new(), Compression::default());
        encoder.write_all(&tar_data).expect("write tgz");
        let compressed = encoder.finish().expect("finish tgz");
        fs::write(path, compressed).expect("write tgz file");
    }

    fn tar_bytes(entries: &[(String, Vec<u8>)]) -> Vec<u8> {
        let mut builder = TarBuilder::new(Vec::new());
        for (name, contents) in entries {
            let mut header = tar::Header::new_gnu();
            header.set_mode(0o644);
            header.set_size(contents.len() as u64);
            header.set_cksum();
            builder
                .append_data(&mut header, name, Cursor::new(contents.as_slice()))
                .expect("append tar entry");
        }
        builder.into_inner().expect("finish tar")
    }
}

use std::collections::BTreeSet;
use std::env;
use std::error::Error;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use grep_regex::RegexMatcherBuilder;
use grep_searcher::{BinaryDetection, Searcher, SearcherBuilder, Sink, SinkMatch};
use ignore::WalkBuilder;

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
const HELP_TEXT: &str = r#"rgrank: ripgrep-style search with file-level ranking

Usage:
  rgrank [options] <query> [path ...]
  rgrank --files [options] [path ...]

Options:
      --files                 List matching file paths only
  -F, --fixed-strings         Treat the query as literal ranked terms instead of a regex
  -k, --top-k <n>             Number of ranked files to print (default: 5)
  -m, --max-candidate-lines <n>
                              Global cap on matched lines collected before ranking (default: 500)
  -C, --context <n>           Context lines shown around each hit snippet (default: 2)
  -s, --max-snippets <n>      Snippets shown per ranked file (default: 3)
      --hidden                Include hidden files and directories
      --no-ignore             Disable .gitignore/.ignore filtering
  -L, --follow-links          Follow symbolic links
      --case-sensitive        Disable smart-case matching
  -h, --help                  Show this help

Behavior:
  - Uses ripgrep-style regex matching by default.
  - Use -F/--fixed-strings for literal term matching.
  - Respects .gitignore/.ignore by default.
  - Ranks files with BM25-like scoring, term coverage, filename boosts, and phrase/proximity bonuses.
"#;

#[derive(Debug, Clone, Copy, Eq, PartialEq)]
enum MatchMode {
    Regex,
    FixedStrings,
}

#[derive(Debug, Clone)]
struct Cli {
    query: Option<String>,
    roots: Vec<PathBuf>,
    files_mode: bool,
    match_mode: MatchMode,
    top_k: usize,
    max_candidate_lines: usize,
    context_lines: usize,
    max_snippets: usize,
    hidden: bool,
    no_ignore: bool,
    follow_links: bool,
    case_sensitive: bool,
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

fn main() {
    match run() {
        Ok(output) => {
            if !output.is_empty() {
                println!("{output}");
            }
        }
        Err(message) => {
            eprintln!("{message}");
            std::process::exit(1);
        }
    }
}

fn run() -> Result<String, String> {
    let cli = parse_args()?;
    if cli.files_mode {
        let files = list_files(&cli).map_err(|error| error.to_string())?;
        return Ok(render_file_list(&files));
    }
    let report = execute_search(&cli).map_err(|error| error.to_string())?;
    Ok(render_report(&report))
}

fn parse_args() -> Result<Cli, String> {
    parse_args_from(env::args().skip(1))
}

fn parse_args_from<I>(args: I) -> Result<Cli, String>
where
    I: IntoIterator<Item = String>,
{
    let mut top_k = DEFAULT_TOP_K;
    let mut max_candidate_lines = DEFAULT_MAX_CANDIDATE_LINES;
    let mut context_lines = DEFAULT_CONTEXT_LINES;
    let mut max_snippets = DEFAULT_MAX_SNIPPETS;
    let mut hidden = false;
    let mut no_ignore = false;
    let mut follow_links = false;
    let mut case_sensitive = false;
    let mut files_mode = false;
    let mut match_mode = MatchMode::Regex;
    let mut positionals: Vec<String> = Vec::new();
    let mut args = args.into_iter();

    while let Some(argument) = args.next() {
        match argument.as_str() {
            "-h" | "--help" => print_help(),
            "--files" => files_mode = true,
            "-F" | "--fixed-strings" => match_mode = MatchMode::FixedStrings,
            "--hidden" => hidden = true,
            "--no-ignore" => no_ignore = true,
            "-L" | "--follow-links" => follow_links = true,
            "--case-sensitive" => case_sensitive = true,
            "-k" | "--top-k" => {
                top_k = parse_usize_value(args.next(), &argument)?;
            }
            "-m" | "--max-candidate-lines" => {
                max_candidate_lines = parse_usize_value(args.next(), &argument)?;
            }
            "-C" | "--context" => {
                context_lines = parse_usize_value(args.next(), &argument)?;
            }
            "-s" | "--max-snippets" => {
                max_snippets = parse_usize_value(args.next(), &argument)?;
            }
            _ if argument.starts_with("--top-k=") => {
                top_k = parse_usize_inline(&argument, "--top-k=")?;
            }
            _ if argument.starts_with("--max-candidate-lines=") => {
                max_candidate_lines = parse_usize_inline(&argument, "--max-candidate-lines=")?;
            }
            _ if argument.starts_with("--context=") => {
                context_lines = parse_usize_inline(&argument, "--context=")?;
            }
            _ if argument.starts_with("--max-snippets=") => {
                max_snippets = parse_usize_inline(&argument, "--max-snippets=")?;
            }
            "--" => {
                positionals.extend(args);
                break;
            }
            _ if argument.starts_with('-') => {
                return Err(format!("unknown flag: {argument}\n\n{HELP_TEXT}"));
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
            return Err(format!("missing query\n\n{HELP_TEXT}"));
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

    Ok(Cli {
        query,
        roots,
        files_mode,
        match_mode,
        top_k,
        max_candidate_lines,
        context_lines,
        max_snippets,
        hidden,
        no_ignore,
        follow_links,
        case_sensitive,
    })
}

fn print_help() -> ! {
    println!("{HELP_TEXT}");
    std::process::exit(0);
}

fn parse_usize_value(value: Option<String>, flag: &str) -> Result<usize, String> {
    let Some(raw) = value else {
        return Err(format!("missing value for {flag}"));
    };
    raw.parse::<usize>()
        .map_err(|_| format!("invalid numeric value for {flag}: {raw}"))
}

fn parse_usize_inline(argument: &str, prefix: &str) -> Result<usize, String> {
    let value = argument
        .strip_prefix(prefix)
        .ok_or_else(|| format!("invalid flag syntax: {argument}"))?;
    value
        .parse::<usize>()
        .map_err(|_| format!("invalid numeric value for {prefix}: {value}"))
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
    let mut matcher_builder = RegexMatcherBuilder::new();
    if cli.case_sensitive {
        matcher_builder.case_insensitive(false);
        matcher_builder.case_smart(false);
    } else {
        matcher_builder.case_smart(true);
    }
    let matcher = match cli.match_mode {
        MatchMode::Regex => matcher_builder.build(query_text)?,
        MatchMode::FixedStrings => matcher_builder.build_literals(&query.terms)?,
    };

    let mut searcher_builder = SearcherBuilder::new();
    searcher_builder.line_number(true);
    searcher_builder.binary_detection(BinaryDetection::quit(0));
    let mut searcher = searcher_builder.build();

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
        if let Err(error) = searcher.search_path(&matcher, dir_entry.path(), &mut sink) {
            eprintln!(
                "warning: failed to search {}: {error}",
                dir_entry.path().display()
            );
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
    ranked.truncate(cli.top_k);
    for result in &mut ranked {
        result.snippets = build_snippets(
            &result.path,
            cli.context_lines,
            cli.max_snippets,
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
    context_lines: usize,
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
        let start_line = snippet.start_line.saturating_sub(context_lines).max(1);
        let end_line = (snippet.end_line + context_lines).min(file_lines.len());
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
    use std::process;
    use std::time::{SystemTime, UNIX_EPOCH};

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
            query: Some("timeout config".to_owned()),
            roots: vec![root.clone()],
            files_mode: false,
            match_mode: MatchMode::FixedStrings,
            top_k: 3,
            max_candidate_lines: DEFAULT_MAX_CANDIDATE_LINES,
            context_lines: 1,
            max_snippets: 2,
            hidden: false,
            no_ignore: false,
            follow_links: false,
            case_sensitive: false,
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
            query: Some("timeout config".to_owned()),
            roots: vec![root.clone()],
            files_mode: false,
            match_mode: MatchMode::FixedStrings,
            top_k: 5,
            max_candidate_lines: DEFAULT_MAX_CANDIDATE_LINES,
            context_lines: 1,
            max_snippets: 2,
            hidden: false,
            no_ignore: false,
            follow_links: false,
            case_sensitive: false,
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
        let cli =
            parse_args_from(["--files".to_owned(), "./hypotheses".to_owned()]).expect("parse args");
        assert!(cli.files_mode);
        assert!(cli.query.is_none());
        assert_eq!(cli.roots, vec![PathBuf::from("./hypotheses")]);
        assert_eq!(cli.match_mode, MatchMode::Regex);
    }

    #[test]
    fn parse_args_accepts_fixed_strings_mode() {
        let cli = parse_args_from([
            "-F".to_owned(),
            "cortisol level".to_owned(),
            "./hypotheses".to_owned(),
        ])
        .expect("parse args");
        assert!(!cli.files_mode);
        assert_eq!(cli.query.as_deref(), Some("cortisol level"));
        assert_eq!(cli.match_mode, MatchMode::FixedStrings);
    }

    #[test]
    fn list_files_respects_gitignore_by_default() {
        let root = create_test_dir("files-ignore");
        fs::write(root.join(".gitignore"), "ignored.txt\n").expect("write .gitignore");
        fs::write(root.join("ignored.txt"), "ignored\n").expect("write ignored.txt");
        fs::write(root.join("visible.txt"), "visible\n").expect("write visible.txt");

        let cli = Cli {
            query: None,
            roots: vec![root.clone()],
            files_mode: true,
            match_mode: MatchMode::Regex,
            top_k: DEFAULT_TOP_K,
            max_candidate_lines: DEFAULT_MAX_CANDIDATE_LINES,
            context_lines: DEFAULT_CONTEXT_LINES,
            max_snippets: DEFAULT_MAX_SNIPPETS,
            hidden: false,
            no_ignore: false,
            follow_links: false,
            case_sensitive: false,
        };
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
            query: Some("whisper|process_all_mp4s|python".to_owned()),
            roots: vec![root.clone()],
            files_mode: false,
            match_mode: MatchMode::Regex,
            top_k: 10,
            max_candidate_lines: DEFAULT_MAX_CANDIDATE_LINES,
            context_lines: 1,
            max_snippets: 1,
            hidden: false,
            no_ignore: false,
            follow_links: false,
            case_sensitive: false,
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

    fn create_test_dir(label: &str) -> PathBuf {
        let timestamp = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("clock")
            .as_nanos();
        let root = env::temp_dir().join(format!("rgrank-{label}-{}-{timestamp}", process::id()));
        fs::create_dir_all(&root).expect("create test dir");
        root
    }
}

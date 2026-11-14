//! Guard rail (umbrella DECISIONS 0528, plan M4): every call in `crates/*/src`
//! that changes the owner's Docker or deletes/overwrites a site's files must
//! name the real-Docker scenario that covers it, or be on the allowlist of
//! known gaps. Runs in plain `cargo test`; a new changing call without an
//! annotation fails the build.
//!
//! **Annotate** the statement (on its line, or in the comment lines directly
//! above it):
//!
//! ```text
//! // scenario: start_reconnects_a_db_the_stale_endpoint_clear_left_without_networks
//! // scenario: fn_one, fn_two
//! // scenario: none — <why no scenario covers it yet>
//! ```
//!
//! A named scenario must be a `#[test]` fn in `tests/scenarios/`. `none` must
//! also be listed in [`ALLOW`] (`docker_guard_allow.txt`): that file is the
//! backlog of changing calls no scenario covers. An allowlist entry that
//! matches no `none` call fails too, so the list never goes stale.
//!
//! **What counts as a changing call** (a tolerant scanner over the token
//! stream, not a Rust parser):
//!
//! - an argument list of a `docker` / `docker compose` command, recognised as
//!   string literals in an array (`[..]`, `&[..]`, `vec![..]`) or in
//!   `.arg(..)` / `.args(..)`, holding `rm`, `rmi`, `up`, `down`, `update`,
//!   `restart`, `start`, `stop`, `kill`, `tag`, `pull`, `prune`,
//!   `network connect|disconnect`, or `cp` into a container. Inside
//!   `.args`/`.arg` on a command built by `docker()`, `compose()` or
//!   `Command::new("docker")` the word may be anywhere (so `exec … rm -rf`
//!   counts); in a bare array it must be the first literal (the sub-command
//!   handed to a helper such as `run_recovering`). `.args` on a command the
//!   scanner can't trace is treated as Docker (conservative);
//!   `Command::new("<other>")` is not.
//! - an array starting `"docker"` (`docker desktop restart`) or a runtime
//!   tool (`orbctl`/`colima`/`rdctl`) with `start`/`stop`/`restart`/
//!   `shutdown`: restarting the owner's Docker.
//! - in `aincient-core` (every module but [`FILE_SCOPE_EXCLUDED`]): a file
//!   delete or overwrite: `remove_file`, `remove_dir(_all)`, `fs::write`,
//!   `fs::rename`, `File::create`, `.truncate(true)`, and the crate's own
//!   `write_atomic` / `write_private` (whose bodies are not sites).
//!
//! **Known limits** (what it can't see): a command whose words are built at
//! run time (`format!`, a variable, `push`) or passed through a non-literal;
//! a shell script run in a container (`exec … sh -c <script>`) that deletes
//! files; file operations through a helper other than the ones above; test
//! code is skipped (`#[cfg(test)]` items, `#[test]` fns, `tests.rs`). The
//! enclosing function is the nearest `fn` before the call.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

/// The allowlist of changing calls no scenario covers yet.
const ALLOW: &str = "tests/docker_guard_allow.txt";

/// `aincient-core` modules whose file writes never touch a site's home.
const FILE_SCOPE_EXCLUDED: &[(&str, &str)] = &[
    (
        "settings.rs",
        "the GUI's own settings file in the user's config dir",
    ),
    (
        "mcp.rs",
        "new component files in a theme dir the user names (scaffold)",
    ),
    (
        "ssh_key.rs",
        "creates a new key file (create_new), never overwrites",
    ),
    (
        "sites.rs",
        "the Manager's site registry (sites.toml), not a site's files",
    ),
    (
        "pack.rs",
        "a theme pack's own folder (scaffold refuses to overwrite; the preset copy)",
    ),
    (
        "oplock.rs",
        "opens the site's lock file without truncating it",
    ),
];

/// Docker sub-commands and words that change something.
const CHANGING: &[&str] = &[
    "rm", "rmi", "up", "down", "update", "restart", "start", "stop", "kill", "tag", "pull", "prune",
];

/// Runtime tools whose start/stop/restart restarts the owner's Docker.
const RUNTIME_TOOLS: &[&str] = &["orbctl", "colima", "rdctl"];
const RUNTIME_VERBS: &[&str] = &["start", "stop", "restart", "shutdown"];

/// File deletes/overwrites (callee names).
const FILE_OPS: &[&str] = &[
    "remove_file",
    "remove_dir_all",
    "remove_dir",
    "write_atomic",
    "write_private",
];
/// File ops that count only as `fs::<name>` / `File::<name>`.
const FS_PATH_OPS: &[(&str, &str)] = &[("fs", "write"), ("fs", "rename"), ("File", "create")];
/// Helpers whose own bodies are not sites (their callers are).
const PRIMITIVE_BODIES: &[&str] = &["write_atomic", "write_private"];

// ---------------------------------------------------------------- lexer --

#[derive(Debug, Clone, PartialEq)]
enum Tok {
    Ident(String),
    Str(String),
    Punct(char),
}

#[derive(Debug, Clone)]
struct Token {
    tok: Tok,
    line: usize,
}

struct Lexed {
    toks: Vec<Token>,
    /// line → `//` comment text on it.
    comments: BTreeMap<usize, String>,
    /// lines → whether they hold code (not only a comment / blank).
    code_lines: BTreeMap<usize, String>,
}

fn lex(src: &str) -> Lexed {
    let b: Vec<char> = src.chars().collect();
    let mut toks = Vec::new();
    let mut comments: BTreeMap<usize, String> = BTreeMap::new();
    let mut i = 0;
    let mut line = 1;
    while i < b.len() {
        let c = b[i];
        if c == '\n' {
            line += 1;
            i += 1;
        } else if c.is_whitespace() {
            i += 1;
        } else if c == '/' && b.get(i + 1) == Some(&'/') {
            let start = i;
            while i < b.len() && b[i] != '\n' {
                i += 1;
            }
            let text: String = b[start..i].iter().collect();
            comments.entry(line).or_default().push_str(&text);
        } else if c == '/' && b.get(i + 1) == Some(&'*') {
            let mut depth = 0;
            while i < b.len() {
                if b[i] == '/' && b.get(i + 1) == Some(&'*') {
                    depth += 1;
                    i += 2;
                } else if b[i] == '*' && b.get(i + 1) == Some(&'/') {
                    depth -= 1;
                    i += 2;
                    if depth == 0 {
                        break;
                    }
                } else {
                    if b[i] == '\n' {
                        line += 1;
                    }
                    i += 1;
                }
            }
        } else if c == '"'
            || ((c == 'r' || c == 'b')
                && matches!(
                    (b.get(i + 1), b.get(i + 2)),
                    (Some('"'), _) | (Some('#'), Some('"' | '#')) | (Some('r'), Some('"' | '#'))
                ))
        {
            // "..", r"..", r#".."#, b"..", br#".."#
            let start_line = line;
            let mut j = i;
            if b[j] == 'b' {
                j += 1;
            }
            let raw = b[j] == 'r';
            if raw {
                j += 1;
            }
            let mut hashes = 0;
            while b.get(j) == Some(&'#') {
                hashes += 1;
                j += 1;
            }
            // b[j] == '"'
            j += 1;
            let mut s = String::new();
            while j < b.len() {
                let d = b[j];
                if d == '\n' {
                    line += 1;
                }
                if !raw && d == '\\' {
                    if let Some(&e) = b.get(j + 1) {
                        if e == '\n' {
                            line += 1;
                        }
                        s.push(match e {
                            'n' => '\n',
                            't' => '\t',
                            other => other,
                        });
                    }
                    j += 2;
                    continue;
                }
                if d == '"' && (0..hashes).all(|k| b.get(j + 1 + k) == Some(&'#')) {
                    j += 1 + hashes;
                    break;
                }
                s.push(d);
                j += 1;
            }
            toks.push(Token {
                tok: Tok::Str(s),
                line: start_line,
            });
            i = j;
        } else if c == '\'' {
            // A char literal ('a', '\n', '\'', '"') or a lifetime ('a).
            if b.get(i + 1) == Some(&'\\') {
                let mut j = i + 3;
                while j < b.len() && b[j] != '\'' {
                    j += 1;
                }
                i = j + 1;
            } else if b.get(i + 2) == Some(&'\'') {
                i += 3;
            } else {
                i += 1; // lifetime: the ident follows
            }
        } else if c.is_alphanumeric() || c == '_' {
            let start = i;
            while i < b.len() && (b[i].is_alphanumeric() || b[i] == '_') {
                i += 1;
            }
            toks.push(Token {
                tok: Tok::Ident(b[start..i].iter().collect()),
                line,
            });
        } else {
            toks.push(Token {
                tok: Tok::Punct(c),
                line,
            });
            i += 1;
        }
    }
    let code_lines = src
        .lines()
        .enumerate()
        .map(|(n, l)| {
            let code = match l.find("//") {
                Some(at) => &l[..at],
                None => l,
            };
            (n + 1, code.trim().to_string())
        })
        .collect();
    Lexed {
        toks,
        comments,
        code_lines,
    }
}

// --------------------------------------------------------------- scanner --

/// One changing call the scanner found.
#[derive(Debug, Clone)]
struct Site {
    /// Relative to the workspace root, `/`-separated.
    path: String,
    line: usize,
    func: String,
    /// `docker: up -d`, `runtime: orbctl restart docker`, `fs: remove_file`.
    what: String,
}

impl Site {
    fn key(&self) -> String {
        format!("{} | {} | {}", self.path, self.func, self.what)
    }
}

fn is_ident(t: Option<&Token>, name: &str) -> bool {
    matches!(t, Some(Token { tok: Tok::Ident(s), .. }) if s == name)
}
fn is_punct(t: Option<&Token>, c: char) -> bool {
    matches!(t, Some(Token { tok: Tok::Punct(p), .. }) if *p == c)
}

/// The index of the token closing the group opened at `open`.
fn close_of(toks: &[Token], open: usize) -> usize {
    let mut depth = 0i32;
    for (k, t) in toks.iter().enumerate().skip(open) {
        if let Tok::Punct(p) = t.tok {
            match p {
                '(' | '[' | '{' => depth += 1,
                ')' | ']' | '}' => {
                    depth -= 1;
                    if depth == 0 {
                        return k;
                    }
                }
                _ => {}
            }
        }
    }
    toks.len().saturating_sub(1)
}

/// Token ranges of test-only code: an item after `#[cfg(..test..)]` or
/// `#[test]`.
fn test_ranges(toks: &[Token]) -> Vec<(usize, usize)> {
    let mut out = Vec::new();
    let mut i = 0;
    while i < toks.len() {
        if is_punct(toks.get(i), '#') && is_punct(toks.get(i + 1), '[') {
            let close = close_of(toks, i + 1);
            let attr = &toks[i + 2..close];
            let is_test = match attr.first() {
                Some(Token {
                    tok: Tok::Ident(a), ..
                }) if a == "test" => attr.len() == 1,
                Some(Token {
                    tok: Tok::Ident(a), ..
                }) if a == "cfg" => attr.iter().any(|t| t.tok == Tok::Ident("test".into())),
                _ => false,
            };
            if is_test {
                // The item: up to its `;` or through its `{..}` body.
                let mut k = close + 1;
                let end = loop {
                    match toks.get(k).map(|t| &t.tok) {
                        None => break toks.len() - 1,
                        Some(Tok::Punct('#')) if is_punct(toks.get(k + 1), '[') => {
                            k = close_of(toks, k + 1) + 1;
                        }
                        Some(Tok::Punct(';')) => break k,
                        Some(Tok::Punct('{')) => break close_of(toks, k),
                        Some(Tok::Punct('(' | '[')) => k = close_of(toks, k) + 1,
                        _ => k += 1,
                    }
                };
                out.push((i, end));
                i = end + 1;
                continue;
            }
            i = close + 1;
            continue;
        }
        i += 1;
    }
    out
}

/// What a command builder expression is.
#[derive(Debug, PartialEq, Clone, Copy)]
enum Cmd {
    Docker,
    Other,
    Unknown,
}

fn classify_builder(expr: &[Token]) -> Cmd {
    for (k, t) in expr.iter().enumerate() {
        match &t.tok {
            Tok::Ident(s)
                if (s == "docker" || s == "compose") && is_punct(expr.get(k + 1), '(') =>
            {
                return Cmd::Docker;
            }
            Tok::Ident(s)
                if (s == "new" || s == "tool")
                    && is_punct(expr.get(k + 1), '(')
                    && matches!(
                        expr.get(k + 2),
                        Some(Token {
                            tok: Tok::Str(_),
                            ..
                        })
                    ) =>
            {
                if let Some(Token {
                    tok: Tok::Str(p), ..
                }) = expr.get(k + 2)
                {
                    return if p == "docker" {
                        Cmd::Docker
                    } else {
                        Cmd::Other
                    };
                }
            }
            _ => {}
        }
    }
    Cmd::Unknown
}

/// The receiver of the `.args`/`.arg` whose `.` is at `dot`: walk back over
/// chained calls to the start, then (for a variable) to its `let`.
fn receiver(toks: &[Token], dot: usize) -> Cmd {
    // Find the expression start: skip `.name(..)` links and `path::` parts.
    let mut k = dot;
    loop {
        if k == 0 {
            return Cmd::Unknown;
        }
        let prev = &toks[k - 1];
        match &prev.tok {
            Tok::Punct(')') | Tok::Punct(']') => {
                // find its opener
                let mut depth = 0i32;
                let mut j = k - 1;
                loop {
                    match toks[j].tok {
                        Tok::Punct(')' | ']' | '}') => depth += 1,
                        Tok::Punct('(' | '[' | '{') => {
                            depth -= 1;
                            if depth == 0 {
                                break;
                            }
                        }
                        _ => {}
                    }
                    if j == 0 {
                        return Cmd::Unknown;
                    }
                    j -= 1;
                }
                k = j;
            }
            Tok::Ident(_) => {
                k -= 1;
                // `a.b`, `a::b`
                if k >= 1 && is_punct(toks.get(k - 1), '.') {
                    k -= 1;
                } else if k >= 2 && is_punct(toks.get(k - 1), ':') && is_punct(toks.get(k - 2), ':')
                {
                    k -= 2;
                } else {
                    break;
                }
            }
            _ => return Cmd::Unknown,
        }
    }
    let expr = &toks[k..dot];
    match classify_builder(expr) {
        Cmd::Unknown => {}
        known => return known,
    }
    // A plain variable: its nearest `let`.
    if let [Token {
        tok: Tok::Ident(var),
        ..
    }] = expr
    {
        let mut j = k;
        while j > 0 {
            j -= 1;
            if is_ident(toks.get(j), "let") {
                let mut n = j + 1;
                if is_ident(toks.get(n), "mut") {
                    n += 1;
                }
                if is_ident(toks.get(n), var) {
                    let mut end = n;
                    while end < toks.len() && !is_punct(toks.get(end), ';') {
                        end += 1;
                    }
                    return classify_builder(&toks[n..end]);
                }
            }
        }
    }
    Cmd::Unknown
}

/// An element of a group: its first string literal, and whether the element
/// *is* that literal (`"up"`, `"up".to_string()`, `&"up"`).
struct Elem {
    first_str: Option<String>,
    plain: Option<String>,
}

fn elements(toks: &[Token], open: usize, close: usize) -> Vec<Elem> {
    let mut out = Vec::new();
    let mut depth = 0i32;
    let mut start = open + 1;
    for k in open + 1..=close {
        let t = &toks[k].tok;
        let at_end = k == close;
        match t {
            Tok::Punct('(' | '[' | '{') if !at_end => depth += 1,
            Tok::Punct(')' | ']' | '}') if !at_end => depth -= 1,
            _ => {}
        }
        if (depth == 0 && *t == Tok::Punct(',')) || at_end {
            let part = &toks[start..k];
            if !part.is_empty() {
                let first_str = part.iter().find_map(|t| match &t.tok {
                    Tok::Str(s) => Some(s.clone()),
                    _ => None,
                });
                let lead = part
                    .iter()
                    .find(|t| t.tok != Tok::Punct('&'))
                    .map(|t| t.tok.clone());
                let plain = match lead {
                    Some(Tok::Str(s)) => Some(s),
                    _ => None,
                };
                out.push(Elem { first_str, plain });
            }
            start = k + 1;
        }
    }
    out
}

/// A `docker` argument list that changes something: the words, or None.
fn changing_docker(elems: &[Elem], anywhere: bool) -> Option<String> {
    let words: Vec<&str> = elems.iter().filter_map(|e| e.plain.as_deref()).collect();
    if words.is_empty() {
        return None;
    }
    let first = elems.first().and_then(|e| e.plain.as_deref());
    let hit = |w: &str| CHANGING.contains(&w);
    let mut changing = if anywhere {
        words.iter().any(|w| hit(w))
    } else {
        first.is_some_and(hit)
            || (matches!(first, Some("network" | "volume" | "image" | "container"))
                && elems
                    .get(1)
                    .and_then(|e| e.plain.as_deref())
                    .is_some_and(|w| hit(w) || w == "connect" || w == "disconnect"))
    };
    if anywhere && words.contains(&"network") {
        changing |= words.iter().any(|w| *w == "connect" || *w == "disconnect");
    }
    // `cp` into a container changes it; out of one (`app:/x` first) doesn't.
    let cp_at = elems.iter().position(|e| e.plain.as_deref() == Some("cp"));
    if let Some(at) = cp_at.filter(|at| anywhere || *at == 0) {
        let from_container = elems
            .get(at + 1)
            .and_then(|e| e.first_str.as_deref())
            .is_some_and(|s| {
                s.split('/')
                    .next()
                    .is_some_and(|p| p.ends_with(':') || p.contains(":{"))
                    || s.starts_with("app:")
            });
        changing |= !from_container;
    }
    changing.then(|| words.join(" "))
}

fn scan_file(path: &str, src: &str, file_ops: bool) -> (Vec<Site>, Lexed) {
    let lexed = lex(src);
    let toks = &lexed.toks;
    let skip = test_ranges(toks);
    let skipped = |k: usize| skip.iter().any(|(a, b)| k >= *a && k <= *b);
    let mut sites = Vec::new();
    let mut func_at: Vec<(usize, String)> = Vec::new();
    for (k, t) in toks.iter().enumerate() {
        if is_ident(Some(t), "fn") {
            if let Some(Token {
                tok: Tok::Ident(name),
                ..
            }) = toks.get(k + 1)
            {
                func_at.push((k, name.clone()));
            }
        }
    }
    let func_of = |k: usize| -> String {
        func_at
            .iter()
            .rev()
            .find(|(at, _)| *at < k)
            .map(|(_, n)| n.clone())
            .unwrap_or_else(|| "(top)".into())
    };
    let mut seen_lines: BTreeSet<(usize, String)> = BTreeSet::new();
    let mut push = |sites: &mut Vec<Site>, line: usize, f: String, what: String| {
        if seen_lines.insert((line, what.clone())) {
            sites.push(Site {
                path: path.to_string(),
                line,
                func: f,
                what,
            });
        }
    };
    for k in 0..toks.len() {
        if skipped(k) {
            continue;
        }
        let t = &toks[k];
        // `.arg(..)` / `.args(..)`
        if (is_ident(Some(t), "args") || is_ident(Some(t), "arg"))
            && k >= 1
            && is_punct(toks.get(k - 1), '.')
            && is_punct(toks.get(k + 1), '(')
        {
            let open = k + 1;
            let close = close_of(toks, open);
            let rcv = receiver(toks, k - 1);
            if rcv == Cmd::Other {
                continue;
            }
            // `.args([..])` / `.args(&[..])` / `.args(vec![..])`: the array's elements.
            let mut inner = open + 1;
            while is_punct(toks.get(inner), '&')
                || is_ident(toks.get(inner), "vec")
                || is_punct(toks.get(inner), '!')
            {
                inner += 1;
            }
            let elems = if is_punct(toks.get(inner), '[') && close_of(toks, inner) + 1 == close {
                elements(toks, inner, close_of(toks, inner))
            } else {
                elements(toks, open, close)
            };
            if let Some(words) = changing_docker(&elems, true) {
                push(&mut sites, t.line, func_of(k), format!("docker: {words}"));
            }
            continue;
        }
        // Arrays: `[..]`, `&[..]`, `vec![..]` (not an index, not an attribute).
        if is_punct(Some(t), '[') {
            let prev = if k == 0 { None } else { toks.get(k - 1) };
            let keyword = ["in", "return", "break", "else", "mut"]
                .iter()
                .any(|kw| is_ident(prev, kw));
            let indexing = !keyword
                && matches!(
                    prev.map(|p| &p.tok),
                    Some(Tok::Ident(_) | Tok::Punct(')' | ']' | '#'))
                );
            let is_vec = is_punct(prev, '!') && k >= 2 && is_ident(toks.get(k - 2), "vec");
            if indexing && !is_vec {
                continue;
            }
            // Inside `.args(` handled above.
            let mut back = k;
            while back > 0
                && (is_punct(toks.get(back - 1), '&')
                    || is_punct(toks.get(back - 1), '!')
                    || is_ident(toks.get(back - 1), "vec"))
            {
                back -= 1;
            }
            if back >= 2
                && is_punct(toks.get(back - 1), '(')
                && (is_ident(toks.get(back - 2), "args") || is_ident(toks.get(back - 2), "arg"))
            {
                continue;
            }
            let close = close_of(toks, k);
            let elems = elements(toks, k, close);
            let first = elems.first().and_then(|e| e.plain.as_deref());
            if first == Some("docker") {
                if let Some(words) = changing_docker(&elems[1..], true) {
                    push(
                        &mut sites,
                        t.line,
                        func_of(k),
                        format!("docker: docker {words}"),
                    );
                }
            } else if first.is_some_and(|f| RUNTIME_TOOLS.contains(&f)) {
                let words: Vec<&str> = elems.iter().filter_map(|e| e.plain.as_deref()).collect();
                if words.iter().any(|w| RUNTIME_VERBS.contains(w)) {
                    push(
                        &mut sites,
                        t.line,
                        func_of(k),
                        format!("runtime: {}", words.join(" ")),
                    );
                }
            } else if let Some(words) = changing_docker(&elems, false) {
                push(&mut sites, t.line, func_of(k), format!("docker: {words}"));
            }
            continue;
        }
        if !file_ops {
            continue;
        }
        // File deletes/overwrites.
        if let Tok::Ident(name) = &t.tok {
            let called = is_punct(toks.get(k + 1), '(');
            if !called {
                continue;
            }
            let f = func_of(k);
            if PRIMITIVE_BODIES.contains(&f.as_str()) {
                continue;
            }
            let defined = k >= 1 && is_ident(toks.get(k - 1), "fn");
            let helper = !defined && FILE_OPS.contains(&name.as_str());
            let fs_path = k >= 3
                && is_punct(toks.get(k - 1), ':')
                && is_punct(toks.get(k - 2), ':')
                && FS_PATH_OPS
                    .iter()
                    .any(|(m, n)| n == name && is_ident(toks.get(k - 3), m));
            let truncate = name == "truncate"
                && is_ident(toks.get(k + 2), "true")
                && k >= 1
                && is_punct(toks.get(k - 1), '.');
            if helper || fs_path || truncate {
                push(&mut sites, t.line, f, format!("fs: {name}"));
            }
        }
    }
    (sites, lexed)
}

/// The `// scenario:` annotation for a site at `line`: on that line, or in
/// the lines above up to the previous statement end (`;`, `{`, `}`) or the
/// previous site.
fn annotation(lexed: &Lexed, line: usize, other_sites: &BTreeSet<usize>) -> Option<String> {
    let mut l = line;
    loop {
        if let Some(c) = lexed.comments.get(&l) {
            if let Some(at) = c.find("scenario:") {
                return Some(c[at + "scenario:".len()..].trim().to_string());
            }
        }
        if l == 1 || line - l >= 15 {
            return None;
        }
        l -= 1;
        if other_sites.contains(&l) {
            return None;
        }
        let code = lexed.code_lines.get(&l).map(String::as_str).unwrap_or("");
        if code.ends_with(';') || code.ends_with('{') || code.ends_with('}') {
            return None;
        }
    }
}

// ---------------------------------------------------------------- driver --

fn workspace_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../..")
        .canonicalize()
        .expect("workspace root")
}

fn rust_files(dir: &Path, out: &mut Vec<PathBuf>) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for e in entries.flatten() {
        let p = e.path();
        if p.is_dir() {
            rust_files(&p, out);
        } else if p.extension().is_some_and(|x| x == "rs") {
            out.push(p);
        }
    }
}

/// Every `#[test]` fn name in `tests/scenarios/`.
fn scenario_tests(root: &Path) -> BTreeSet<String> {
    let mut files = Vec::new();
    rust_files(
        &root.join("crates/aincient-core/tests/scenarios"),
        &mut files,
    );
    let mut names = BTreeSet::new();
    for f in files {
        let src = std::fs::read_to_string(&f).expect("scenario file");
        let toks = lex(&src).toks;
        for k in 0..toks.len() {
            if is_punct(toks.get(k), '#')
                && is_punct(toks.get(k + 1), '[')
                && is_ident(toks.get(k + 2), "test")
                && is_punct(toks.get(k + 3), ']')
            {
                // The next `fn <name>` after its attributes.
                if let Some(j) = (k + 4..toks.len()).find(|&j| is_ident(toks.get(j), "fn")) {
                    if let Some(Token {
                        tok: Tok::Ident(n), ..
                    }) = toks.get(j + 1)
                    {
                        names.insert(n.clone());
                    }
                }
            }
        }
    }
    names
}

/// Allowlist entries: `<path> | <fn> | <what> | <reason>`.
fn allowlist(root: &Path) -> BTreeMap<String, String> {
    let text = std::fs::read_to_string(root.join("crates/aincient-core").join(ALLOW))
        .expect("tests/docker_guard_allow.txt");
    text.lines()
        .map(str::trim)
        .filter(|l| !l.is_empty() && !l.starts_with('#'))
        .map(|l| {
            let parts: Vec<&str> = l.splitn(4, '|').map(str::trim).collect();
            assert!(
                parts.len() == 4 && !parts[3].is_empty(),
                "allowlist line needs `<path> | <fn> | <what> | <reason>`: {l}"
            );
            (
                format!("{} | {} | {}", parts[0], parts[1], parts[2]),
                parts[3].to_string(),
            )
        })
        .collect()
}

struct Findings {
    sites: Vec<(Site, Option<String>)>,
}

fn scan_workspace(root: &Path) -> Findings {
    let mut files = Vec::new();
    for c in std::fs::read_dir(root.join("crates"))
        .expect("crates/")
        .flatten()
    {
        rust_files(&c.path().join("src"), &mut files);
    }
    files.sort();
    let mut sites = Vec::new();
    for f in files {
        let rel = f
            .strip_prefix(root)
            .unwrap()
            .to_string_lossy()
            .replace('\\', "/");
        if rel.ends_with("/tests.rs") {
            continue;
        }
        let file_ops = rel.starts_with("crates/aincient-core/src/")
            && !FILE_SCOPE_EXCLUDED
                .iter()
                .any(|(m, _)| rel.ends_with(&format!("/{m}")));
        let src = std::fs::read_to_string(&f).expect("source");
        let (found, lexed) = scan_file(&rel, &src, file_ops);
        let lines: BTreeSet<usize> = found.iter().map(|s| s.line).collect();
        for s in found {
            let mut others = lines.clone();
            others.remove(&s.line);
            let note = annotation(&lexed, s.line, &others);
            sites.push((s, note));
        }
    }
    Findings { sites }
}

/// Check every site; the problems, one per line.
fn check(root: &Path) -> (Vec<String>, usize, usize) {
    let tests = scenario_tests(root);
    let allow = allowlist(root);
    let findings = scan_workspace(root);
    let mut problems = Vec::new();
    let mut used = BTreeSet::new();
    let (mut covered, mut uncovered) = (0, 0);
    for (s, note) in &findings.sites {
        let at = format!("{}:{} (fn {}, `{}`)", s.path, s.line, s.func, s.what);
        match note.as_deref() {
            None => problems.push(format!(
                "{at}: changing call without `// scenario: <test fn>` (or `// scenario: none — \
                 <reason>` plus a line in {ALLOW}: `{} | <reason>`)",
                s.key()
            )),
            Some(n) if n.starts_with("none") => {
                uncovered += 1;
                if n.trim_start_matches("none")
                    .trim_start_matches([' ', '—', '-'])
                    .trim()
                    .is_empty()
                {
                    problems.push(format!("{at}: `scenario: none` needs a reason"));
                }
                if allow.contains_key(&s.key()) {
                    used.insert(s.key());
                } else {
                    problems.push(format!(
                        "{at}: `scenario: none` but not in {ALLOW}; add `{} | <reason>`",
                        s.key()
                    ));
                }
            }
            Some(n) => {
                covered += 1;
                for name in n.split(',').map(str::trim) {
                    if name.is_empty() || !tests.contains(name) {
                        problems.push(format!(
                            "{at}: names `{name}`, which is not a #[test] fn in tests/scenarios/"
                        ));
                    }
                }
            }
        }
    }
    for key in allow.keys() {
        if !used.contains(key) {
            problems.push(format!(
                "{ALLOW}: `{key}` matches no `scenario: none` call any more; remove it"
            ));
        }
    }
    (problems, covered, uncovered)
}

#[test]
fn every_changing_docker_or_file_call_names_its_scenario() {
    let root = workspace_root();
    let (problems, covered, uncovered) = check(&root);
    eprintln!("docker guard: {covered} changing calls covered by a scenario, {uncovered} on the allowlist");
    assert!(
        problems.is_empty(),
        "{} problem(s) — see crates/aincient-core/tests/docker_guard.rs:\n{}",
        problems.len(),
        problems.join("\n")
    );
}

/// `ATELIER_GUARD_LIST=1 cargo test --test docker_guard -- --nocapture list`:
/// print every site with its annotation (for annotating and reviewing).
#[test]
fn list() {
    if std::env::var_os("ATELIER_GUARD_LIST").is_none() {
        return;
    }
    for (s, note) in scan_workspace(&workspace_root()).sites {
        println!(
            "{}:{}\t{}\t{}\t{}",
            s.path,
            s.line,
            s.func,
            s.what,
            note.unwrap_or_else(|| "-".into())
        );
    }
}

// --------------------------------------------------- the scanner's own --

fn scan(src: &str) -> Vec<(String, Option<String>)> {
    let (sites, lexed) = scan_file("x.rs", src, true);
    let lines: BTreeSet<usize> = sites.iter().map(|s| s.line).collect();
    sites
        .iter()
        .map(|s| {
            let mut others = lines.clone();
            others.remove(&s.line);
            (s.what.clone(), annotation(&lexed, s.line, &others))
        })
        .collect()
}

#[test]
fn finds_docker_changes_through_builders_helpers_and_arrays() {
    let found = scan(
        r#"
fn a(stack: &Stack) {
    let mut c = docker::compose(stack);
    c.args(["up", "-d"]);
    let mut d = docker::docker();
    d.args([
        "network",
        "disconnect",
        "-f",
        net,
    ]);
    run_recovering(stack, &["up", "-d", "--no-recreate"], "start", r)?;
    let mut args = vec!["network".to_string(), "connect".to_string()];
    let mut e = compose(stack);
    e.arg("stop");
    let mut x = compose(stack);
    x.args(["exec", "-T", "app", "rm", "-rf"]).args(paths);
    Step::run(&["docker", "desktop", "restart"]);
    Step::run(&["orbctl", "restart", "docker"]);
    std::fs::remove_file(&p);
    std::fs::write(&p, "x");
}
"#,
    );
    let what: Vec<&str> = found.iter().map(|(w, _)| w.as_str()).collect();
    assert_eq!(
        what,
        [
            "docker: up -d",
            "docker: network disconnect -f",
            "docker: up -d --no-recreate",
            "docker: network connect",
            "docker: stop",
            "docker: exec -T app rm -rf",
            "docker: docker desktop restart",
            "runtime: orbctl restart docker",
            "fs: remove_file",
            "fs: write",
        ]
    );
}

#[test]
fn leaves_reads_other_programs_copies_out_and_test_code_alone() {
    let found = scan(
        r#"
fn a(stack: &Stack) {
    let mut c = docker::docker();
    c.args(["inspect", "-f", "{{.State.Restarting}}", name]);
    c.args(["logs", "--since", &start]);
    let mut w = Command::new("cmd");
    w.args(["/C", "start", "", url]);
    let mut cp = compose(stack);
    cp.args(["cp", &format!("app:{archive}"), &dest]);
    let ids = ["prepare", "pull"][0];
    let table = [("pull", "Download"), ("start", "Start")];
    words(&["app", "update"]);
    let v = args[..5];
    let s = "docker compose up -d";
}
#[cfg(test)]
mod tests {
    fn t() { let mut c = compose(stack); c.args(["up"]); std::fs::remove_file(&p); }
}
#[test]
fn u() { Step::run(&["docker", "rm"]); }
"#,
    );
    assert_eq!(found, Vec::<(String, Option<String>)>::new());
}

#[test]
fn a_copy_into_a_container_counts() {
    let found = scan(
        r#"
fn a(stack: &Stack) {
    let mut cp = compose(stack);
    cp.args(["cp", &file.to_string_lossy(), &format!("app:{archive}")]);
}
"#,
    );
    assert_eq!(found.len(), 1, "{found:?}");
}

#[test]
fn the_annotation_is_on_the_statement_or_directly_above_it() {
    let found = scan(
        r#"
fn a(stack: &Stack) {
    let mut c = compose(stack);
    // scenario: one_test
    c.args(["up", "-d"]);
    let mut d = compose(stack);
    d.args(["down"]); // scenario: none — reason
    // scenario: not_this_one
    let x = 1;
    let mut e = compose(stack);
    e.args(["stop"]);
    match a {
        // scenario: arm_test
        A => &["start"],
        B => &["up", "-d"],
    }
    run_recovering(
        // scenario: multi_line
        stack,
        &["up"],
        "x",
    );
}
"#,
    );
    let notes: Vec<Option<&str>> = found.iter().map(|(_, n)| n.as_deref()).collect();
    assert_eq!(
        notes,
        [
            Some("one_test"),
            Some("none — reason"),
            None,
            Some("arm_test"),
            None,
            Some("multi_line"),
        ]
    );
}

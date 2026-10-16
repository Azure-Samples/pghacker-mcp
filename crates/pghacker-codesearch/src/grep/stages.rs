// Copyright (c) Microsoft Corporation.
// Licensed under the MIT license.

//! The pipeline stages the grep tool runs on grep's output, as GNU
//! coreutils (in the C.UTF-8 locale) would: head, tail, wc, sort, uniq, cut
//! and cat. A further grep stage is handled by the caller.

use std::cmp::Ordering;

/// Lines of `input`, each with its newline if it had one.
fn lines(input: &[u8]) -> Vec<&[u8]> {
    let mut out = Vec::new();
    let mut start = 0;
    for (k, b) in input.iter().enumerate() {
        if *b == b'\n' {
            out.push(&input[start..=k]);
            start = k + 1;
        }
    }
    if start < input.len() {
        out.push(&input[start..]);
    }
    out
}

/// A line without its newline.
fn body(line: &[u8]) -> &[u8] {
    line.strip_suffix(b"\n").unwrap_or(line)
}

/// Numeric option values: `N`, `-N` (`all but`) or `+N` (`from`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Count {
    First(usize),
    AllBut(usize),
    From(usize),
}

fn parse_count(v: &str, what: &str) -> Result<Count, String> {
    let v = v.trim();
    let (kind, digits) = match v.as_bytes().first() {
        Some(b'-') => ('-', &v[1..]),
        Some(b'+') => ('+', &v[1..]),
        _ => (' ', v),
    };
    let n: usize = digits
        .parse()
        .map_err(|_| format!("{what}: invalid number of lines: '{v}'"))?;
    Ok(match kind {
        '-' => Count::AllBut(n),
        '+' => Count::From(n),
        _ => Count::First(n),
    })
}

/// Splits `args` into flags with values, GNU style: `-n5`, `-n 5`,
/// `--lines=5`, `--lines 5`, bundled flags and a numeric `-N`.
struct Args {
    flags: Vec<(String, Option<String>)>,
    operands: Vec<String>,
}

fn parse_args(
    args: &[String],
    with_value: &[&str],
    long: &[(&str, &str)],
    tool: &str,
) -> Result<Args, String> {
    let mut flags = Vec::new();
    let mut operands = Vec::new();
    let mut i = 0;
    while i < args.len() {
        let a = &args[i];
        if a == "--" {
            operands.extend(args[i + 1..].iter().cloned());
            break;
        }
        if let Some(l) = a.strip_prefix("--") {
            let (name, inline) = match l.split_once('=') {
                Some((n, v)) => (n, Some(v.to_owned())),
                None => (l, None),
            };
            let Some(&(_, short)) = long.iter().find(|(n, _)| n.starts_with(name)) else {
                return Err(format!("{tool}: unrecognized option '--{name}'"));
            };
            let value = if with_value.contains(&short) {
                match inline {
                    Some(v) => Some(v),
                    None => {
                        i += 1;
                        Some(args.get(i).cloned().ok_or_else(|| {
                            format!("{tool}: option '--{name}' requires an argument")
                        })?)
                    }
                }
            } else {
                None
            };
            flags.push((short.to_owned(), value));
            i += 1;
            continue;
        }
        if a.len() > 1 && a.starts_with('-') {
            if a[1..].chars().all(|c| c.is_ascii_digit()) && with_value.contains(&"n") {
                flags.push(("n".into(), Some(a[1..].to_owned())));
                i += 1;
                continue;
            }
            let chars: Vec<char> = a[1..].chars().collect();
            let mut k = 0;
            while k < chars.len() {
                let f = chars[k].to_string();
                if with_value.contains(&f.as_str()) {
                    let rest: String = chars[k + 1..].iter().collect();
                    let value = if rest.is_empty() {
                        i += 1;
                        args.get(i).cloned().ok_or_else(|| {
                            format!("{tool}: option requires an argument -- '{f}'")
                        })?
                    } else {
                        rest
                    };
                    flags.push((f, Some(value)));
                    break;
                }
                flags.push((f, None));
                k += 1;
            }
            i += 1;
            continue;
        }
        operands.push(a.clone());
        i += 1;
    }
    Ok(Args { flags, operands })
}

fn no_files(args: &Args, tool: &str) -> Result<(), String> {
    if args.operands.iter().any(|o| o != "-") {
        return Err(format!(
            "{tool} after | reads the previous output; it cannot take files"
        ));
    }
    Ok(())
}

fn unsupported(tool: &str, flag: &str) -> String {
    format!("{tool} -{flag} is not supported here")
}

/// `head`.
pub fn head(args: &[String], input: &[u8]) -> Result<Vec<u8>, String> {
    let a = parse_args(
        args,
        &["n", "c"],
        &[
            ("lines", "n"),
            ("bytes", "c"),
            ("quiet", "q"),
            ("silent", "q"),
            ("verbose", "v"),
        ],
        "head",
    )?;
    no_files(&a, "head")?;
    let mut lines_count = Count::First(10);
    let mut bytes: Option<Count> = None;
    for (f, v) in &a.flags {
        match f.as_str() {
            "n" => {
                lines_count = parse_count(v.as_deref().unwrap_or(""), "head")?;
                bytes = None;
            }
            "c" => bytes = Some(parse_count(v.as_deref().unwrap_or(""), "head")?),
            "q" | "v" => {}
            f => return Err(unsupported("head", f)),
        }
    }
    if let Some(b) = bytes {
        return Ok(match b {
            Count::First(n) | Count::From(n) => input[..n.min(input.len())].to_vec(),
            Count::AllBut(n) => input[..input.len().saturating_sub(n)].to_vec(),
        });
    }
    let all = lines(input);
    let keep = match lines_count {
        Count::First(n) | Count::From(n) => n.min(all.len()),
        Count::AllBut(n) => all.len().saturating_sub(n),
    };
    Ok(all[..keep].concat())
}

/// `tail`.
pub fn tail(args: &[String], input: &[u8]) -> Result<Vec<u8>, String> {
    // The obsolete `tail +N` form, as the first argument.
    let mut args = args.to_vec();
    if let Some(n) = args.first().and_then(|a| a.strip_prefix('+'))
        && !n.is_empty()
        && n.chars().all(|c| c.is_ascii_digit())
    {
        args[0] = format!("-n+{n}");
    }
    let a = parse_args(
        &args,
        &["n", "c"],
        &[
            ("lines", "n"),
            ("bytes", "c"),
            ("quiet", "q"),
            ("silent", "q"),
            ("verbose", "v"),
        ],
        "tail",
    )?;
    no_files(&a, "tail")?;
    let mut count = Count::First(10);
    let mut bytes: Option<Count> = None;
    for (f, v) in &a.flags {
        match f.as_str() {
            "n" => {
                let v = v.as_deref().unwrap_or("");
                // For tail, -n N and -n -N both mean the last N.
                count = match parse_count(v, "tail")? {
                    Count::AllBut(n) => Count::First(n),
                    c => c,
                };
                bytes = None;
            }
            "c" => {
                bytes = Some(match parse_count(v.as_deref().unwrap_or(""), "tail")? {
                    Count::AllBut(n) => Count::First(n),
                    c => c,
                })
            }
            "q" | "v" => {}
            f => return Err(unsupported("tail", f)),
        }
    }
    if let Some(b) = bytes {
        return Ok(match b {
            Count::First(n) | Count::AllBut(n) => input[input.len().saturating_sub(n)..].to_vec(),
            Count::From(n) => input[n.saturating_sub(1).min(input.len())..].to_vec(),
        });
    }
    let all = lines(input);
    let from = match count {
        Count::First(n) | Count::AllBut(n) => all.len().saturating_sub(n),
        Count::From(n) => n.saturating_sub(1).min(all.len()),
    };
    Ok(all[from..].concat())
}

/// `wc`.
pub fn wc(args: &[String], input: &[u8]) -> Result<Vec<u8>, String> {
    let a = parse_args(
        args,
        &[],
        &[
            ("lines", "l"),
            ("words", "w"),
            ("chars", "m"),
            ("bytes", "c"),
            ("max-line-length", "L"),
        ],
        "wc",
    )?;
    no_files(&a, "wc")?;
    let mut want = Vec::new();
    for (f, _) in &a.flags {
        match f.as_str() {
            "l" | "w" | "m" | "c" | "L" => want.push(f.clone()),
            f => return Err(unsupported("wc", f)),
        }
    }
    if want.is_empty() {
        want = vec!["l".into(), "w".into(), "c".into()];
    }
    let text = String::from_utf8_lossy(input);
    let mut values = Vec::new();
    for f in ["l", "w", "m", "c", "L"] {
        if !want.iter().any(|w| w == f) {
            continue;
        }
        values.push(match f {
            "l" => input.iter().filter(|b| **b == b'\n').count(),
            "w" => text.split_whitespace().count(),
            "m" => text.chars().count(),
            "c" => input.len(),
            _ => text.lines().map(|l| l.chars().count()).max().unwrap_or(0),
        });
    }
    let width = if values.len() == 1 { 1 } else { 7 };
    let fields: Vec<String> = values.iter().map(|v| format!("{v:>width$}")).collect();
    Ok(format!("{}\n", fields.join(" ")).into_bytes())
}

/// `cat` (and `cat -n`).
pub fn cat(args: &[String], input: &[u8]) -> Result<Vec<u8>, String> {
    let a = parse_args(args, &[], &[("number", "n")], "cat")?;
    no_files(&a, "cat")?;
    let mut number = false;
    for (f, _) in &a.flags {
        match f.as_str() {
            "n" => number = true,
            f => return Err(unsupported("cat", f)),
        }
    }
    if !number {
        return Ok(input.to_vec());
    }
    let mut out = Vec::new();
    for (k, line) in lines(input).into_iter().enumerate() {
        out.extend_from_slice(format!("{:>6}\t", k + 1).as_bytes());
        out.extend_from_slice(line);
    }
    Ok(out)
}

/// A sort key: fields `from..=to` (1-based), with characters.
#[derive(Clone, Debug, Default)]
struct Key {
    field: usize,
    char_start: usize,
    end_field: Option<usize>,
    end_char: usize,
    numeric: bool,
    human: bool,
    reverse: bool,
    fold: bool,
    blanks: bool,
}

fn parse_key(spec: &str) -> Result<Key, String> {
    let mut key = Key::default();
    let (start, end) = match spec.split_once(',') {
        Some((s, e)) => (s, Some(e)),
        None => (spec, None),
    };
    let parse_pos = |pos: &str, key: &mut Key| -> Result<(usize, usize), String> {
        let digits_end = pos
            .find(|c: char| !c.is_ascii_digit() && c != '.')
            .unwrap_or(pos.len());
        let (num, opts) = pos.split_at(digits_end);
        for o in opts.chars() {
            match o {
                'n' => key.numeric = true,
                'h' => key.human = true,
                'r' => key.reverse = true,
                'f' => key.fold = true,
                'b' => key.blanks = true,
                o => return Err(format!("sort: key option '{o}' is not supported here")),
            }
        }
        let (f, c) = match num.split_once('.') {
            Some((f, c)) => (f, c.parse().unwrap_or(0)),
            None => (num, 0),
        };
        let f: usize = f
            .parse()
            .map_err(|_| format!("sort: invalid field specification '{spec}'"))?;
        if f == 0 {
            return Err(format!(
                "sort: field number is zero: invalid field specification '{spec}'"
            ));
        }
        Ok((f, c))
    };
    let (f, c) = parse_pos(start, &mut key)?;
    key.field = f;
    key.char_start = c.max(1);
    if let Some(e) = end {
        let (f, c) = parse_pos(e, &mut key)?;
        key.end_field = Some(f);
        key.end_char = c;
    }
    Ok(key)
}

/// Fields of a line: split at `sep`, or at blank-to-nonblank transitions
/// (each field keeping its leading blanks).
fn fields(line: &[u8], sep: Option<u8>) -> Vec<(usize, usize)> {
    let mut out = Vec::new();
    match sep {
        Some(s) => {
            let mut start = 0;
            for (k, b) in line.iter().enumerate() {
                if *b == s {
                    out.push((start, k));
                    start = k + 1;
                }
            }
            out.push((start, line.len()));
        }
        None => {
            let mut start = 0;
            let mut k = 0;
            while k < line.len() {
                while k < line.len() && (line[k] == b' ' || line[k] == b'\t') {
                    k += 1;
                }
                while k < line.len() && line[k] != b' ' && line[k] != b'\t' {
                    k += 1;
                }
                out.push((start, k));
                start = k;
            }
            if out.is_empty() {
                out.push((0, 0));
            }
        }
    }
    out
}

fn key_slice<'a>(line: &'a [u8], key: &Key, sep: Option<u8>) -> &'a [u8] {
    let f = fields(line, sep);
    let Some(&(fs, fe)) = f.get(key.field - 1) else {
        return &[];
    };
    let mut start = fs;
    if key.blanks || sep.is_none() && key.numeric {
        while start < fe && (line[start] == b' ' || line[start] == b'\t') {
            start += 1;
        }
    }
    start = (start + key.char_start - 1).min(fe.max(start));
    let end = match key.end_field {
        None => line.len(),
        Some(ef) => match f.get(ef - 1) {
            None => line.len(),
            Some(&(es, ee)) => {
                if key.end_char == 0 {
                    ee
                } else {
                    (es + key.end_char).min(ee)
                }
            }
        },
    };
    if end <= start { &[] } else { &line[start..end] }
}

/// A number at the start of `s` (after blanks), as sort -n reads it.
fn numeric_value(s: &[u8], human: bool) -> f64 {
    let text = String::from_utf8_lossy(s);
    let t = text.trim_start_matches([' ', '\t']);
    let mut end = 0;
    let bytes = t.as_bytes();
    if bytes.first() == Some(&b'-') {
        end = 1;
    }
    let digits_start = end;
    while end < bytes.len() && bytes[end].is_ascii_digit() {
        end += 1;
    }
    if end < bytes.len() && bytes[end] == b'.' {
        end += 1;
        while end < bytes.len() && bytes[end].is_ascii_digit() {
            end += 1;
        }
    }
    let number = &t[..end];
    if end == digits_start || number == "-" || number == "." || number == "-." {
        return 0.0;
    }
    let mut v: f64 = number.parse().unwrap_or(0.0);
    if human {
        let scale = match bytes.get(end) {
            Some(b'K' | b'k') => 1e3,
            Some(b'M') => 1e6,
            Some(b'G') => 1e9,
            Some(b'T') => 1e12,
            _ => 1.0,
        };
        v *= scale;
    }
    v
}

/// `sort`.
pub fn sort(args: &[String], input: &[u8]) -> Result<Vec<u8>, String> {
    let a = parse_args(
        args,
        &["k", "t", "S", "T"],
        &[
            ("key", "k"),
            ("field-separator", "t"),
            ("numeric-sort", "n"),
            ("human-numeric-sort", "h"),
            ("reverse", "r"),
            ("unique", "u"),
            ("ignore-case", "f"),
            ("stable", "s"),
            ("ignore-leading-blanks", "b"),
        ],
        "sort",
    )?;
    no_files(&a, "sort")?;
    let mut global = Key {
        field: 1,
        char_start: 1,
        ..Key::default()
    };
    let mut keys = Vec::new();
    let mut sep = None;
    let (mut unique, mut stable) = (false, false);
    for (f, v) in &a.flags {
        match f.as_str() {
            "n" => global.numeric = true,
            "h" => global.human = true,
            "r" => global.reverse = true,
            "f" => global.fold = true,
            "b" => global.blanks = true,
            "u" => unique = true,
            "s" => stable = true,
            "S" | "T" => {}
            "k" => keys.push(parse_key(v.as_deref().unwrap_or(""))?),
            "t" => {
                let v = v.as_deref().unwrap_or("");
                if v.len() != 1 {
                    return Err("sort: multi-character tab".into());
                }
                sep = Some(v.as_bytes()[0]);
            }
            f => return Err(unsupported("sort", f)),
        }
    }
    // Keys without options of their own take the global ones.
    for k in &mut keys {
        if !(k.numeric || k.human || k.reverse || k.fold || k.blanks) {
            k.numeric = global.numeric;
            k.human = global.human;
            k.reverse = global.reverse;
            k.fold = global.fold;
            k.blanks = global.blanks;
        }
    }
    let whole = keys.is_empty();
    if whole {
        keys.push(global.clone());
    }
    let compare_key = |x: &[u8], y: &[u8], k: &Key| -> Ordering {
        let (a, b) = if whole {
            (x, y)
        } else {
            (key_slice(x, k, sep), key_slice(y, k, sep))
        };
        let ord = if k.numeric || k.human {
            numeric_value(a, k.human)
                .partial_cmp(&numeric_value(b, k.human))
                .unwrap_or(Ordering::Equal)
        } else if k.fold {
            a.to_ascii_uppercase().cmp(&b.to_ascii_uppercase())
        } else {
            a.cmp(b)
        };
        if k.reverse { ord.reverse() } else { ord }
    };
    let compare_keys = |x: &[u8], y: &[u8]| -> Ordering {
        for k in &keys {
            let o = compare_key(x, y, k);
            if o != Ordering::Equal {
                return o;
            }
        }
        Ordering::Equal
    };
    let mut all: Vec<&[u8]> = lines(input).into_iter().map(body).collect();
    all.sort_by(|x, y| {
        let o = compare_keys(x, y);
        if o != Ordering::Equal || stable || unique {
            return o;
        }
        let last = x.cmp(y);
        if global.reverse { last.reverse() } else { last }
    });
    if unique {
        all.dedup_by(|y, x| compare_keys(x, y) == Ordering::Equal);
    }
    let mut out = Vec::new();
    for l in all {
        out.extend_from_slice(l);
        out.push(b'\n');
    }
    Ok(out)
}

/// `uniq`.
pub fn uniq(args: &[String], input: &[u8]) -> Result<Vec<u8>, String> {
    let a = parse_args(
        args,
        &["f", "s", "w"],
        &[
            ("count", "c"),
            ("repeated", "d"),
            ("unique", "u"),
            ("ignore-case", "i"),
            ("skip-fields", "f"),
            ("skip-chars", "s"),
            ("check-chars", "w"),
        ],
        "uniq",
    )?;
    no_files(&a, "uniq")?;
    let (mut count, mut repeated, mut unique, mut fold) = (false, false, false, false);
    let (mut skip_fields, mut skip_chars, mut check) = (0usize, 0usize, usize::MAX);
    for (f, v) in &a.flags {
        let num = || -> Result<usize, String> {
            v.as_deref()
                .unwrap_or("")
                .parse()
                .map_err(|_| format!("uniq: invalid number: {}", v.as_deref().unwrap_or("")))
        };
        match f.as_str() {
            "c" => count = true,
            "d" => repeated = true,
            "u" => unique = true,
            "i" => fold = true,
            "f" => skip_fields = num()?,
            "s" => skip_chars = num()?,
            "w" => check = num()?,
            f => return Err(unsupported("uniq", f)),
        }
    }
    let key = |line: &[u8]| -> Vec<u8> {
        let mut k = 0;
        for _ in 0..skip_fields {
            while k < line.len() && (line[k] == b' ' || line[k] == b'\t') {
                k += 1;
            }
            while k < line.len() && line[k] != b' ' && line[k] != b'\t' {
                k += 1;
            }
        }
        k = (k + skip_chars).min(line.len());
        let end = k.saturating_add(check).min(line.len());
        let s = &line[k..end];
        if fold {
            s.to_ascii_lowercase()
        } else {
            s.to_vec()
        }
    };
    let all: Vec<&[u8]> = lines(input).into_iter().map(body).collect();
    let mut out = Vec::new();
    let mut k = 0;
    while k < all.len() {
        let first = key(all[k]);
        let mut j = k + 1;
        while j < all.len() && key(all[j]) == first {
            j += 1;
        }
        let n = j - k;
        if (!repeated || n > 1) && (!unique || n == 1) {
            if count {
                out.extend_from_slice(format!("{n:>7} ").as_bytes());
            }
            out.extend_from_slice(all[k]);
            out.push(b'\n');
        }
        k = j;
    }
    Ok(out)
}

/// A cut list such as `1,3-5,7-`.
fn parse_list(list: &str) -> Result<Vec<(usize, usize)>, String> {
    let mut out = Vec::new();
    for part in list.split([',', ' ']).filter(|p| !p.is_empty()) {
        let (lo, hi) = match part.split_once('-') {
            Some((a, b)) => (
                if a.is_empty() {
                    1
                } else {
                    a.parse().map_err(|_| bad_list(list))?
                },
                if b.is_empty() {
                    usize::MAX
                } else {
                    b.parse().map_err(|_| bad_list(list))?
                },
            ),
            None => {
                let n = part.parse().map_err(|_| bad_list(list))?;
                (n, n)
            }
        };
        if lo == 0 {
            return Err("cut: fields and positions are numbered from 1".into());
        }
        if lo > hi {
            return Err("cut: invalid decreasing range".into());
        }
        out.push((lo, hi));
    }
    if out.is_empty() {
        return Err(bad_list(list));
    }
    Ok(out)
}

fn bad_list(list: &str) -> String {
    format!("cut: invalid field value '{list}'")
}

fn in_list(list: &[(usize, usize)], n: usize, complement: bool) -> bool {
    list.iter().any(|&(lo, hi)| lo <= n && n <= hi) != complement
}

/// `cut`.
pub fn cut(args: &[String], input: &[u8]) -> Result<Vec<u8>, String> {
    let a = parse_args(
        args,
        &["b", "c", "f", "d"],
        &[
            ("bytes", "b"),
            ("characters", "c"),
            ("fields", "f"),
            ("delimiter", "d"),
            ("only-delimited", "s"),
            ("complement", "C"),
            ("output-delimiter", "O"),
        ],
        "cut",
    )?;
    no_files(&a, "cut")?;
    let mut bytes: Option<Vec<(usize, usize)>> = None;
    let mut fields_list: Option<Vec<(usize, usize)>> = None;
    let mut delim = b'\t';
    let (mut only, mut complement) = (false, false);
    let mut out_delim: Option<Vec<u8>> = None;
    for (f, v) in &a.flags {
        let v = v.clone().unwrap_or_default();
        match f.as_str() {
            "b" | "c" => bytes = Some(parse_list(&v)?),
            "f" => fields_list = Some(parse_list(&v)?),
            "d" => {
                if v.len() != 1 {
                    return Err("cut: the delimiter must be a single character".into());
                }
                delim = v.as_bytes()[0];
            }
            "s" => only = true,
            "C" => complement = true,
            "O" => out_delim = Some(v.into_bytes()),
            "n" => {}
            f => return Err(unsupported("cut", f)),
        }
    }
    let mut out = Vec::new();
    for line in lines(input) {
        let text = body(line);
        match (&bytes, &fields_list) {
            (Some(list), None) => {
                // GNU cut -c counts bytes.
                let mut prev: Option<usize> = None;
                for (k, b) in text.iter().enumerate() {
                    if in_list(list, k + 1, complement) {
                        if let (Some(od), Some(p)) = (&out_delim, prev)
                            && p + 1 != k
                        {
                            out.extend_from_slice(od);
                        }
                        out.push(*b);
                        prev = Some(k);
                    }
                }
                out.push(b'\n');
            }
            (None, Some(list)) => {
                if !text.contains(&delim) {
                    if !only {
                        out.extend_from_slice(text);
                        out.push(b'\n');
                    }
                    continue;
                }
                let parts: Vec<&[u8]> = text.split(|b| *b == delim).collect();
                let picked: Vec<&[u8]> = parts
                    .iter()
                    .enumerate()
                    .filter(|(k, _)| in_list(list, k + 1, complement))
                    .map(|(_, p)| *p)
                    .collect();
                let sep = out_delim.clone().unwrap_or_else(|| vec![delim]);
                out.extend_from_slice(&picked.join(&sep[..]));
                out.push(b'\n');
            }
            _ => return Err("cut: you must specify a list of bytes, characters, or fields".into()),
        }
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    type Stage = fn(&[String], &[u8]) -> Result<Vec<u8>, String>;

    fn run(f: Stage, args: &[&str], input: &str) -> String {
        let args: Vec<String> = args.iter().map(|s| (*s).to_owned()).collect();
        String::from_utf8(f(&args, input.as_bytes()).unwrap()).unwrap()
    }

    #[test]
    fn head_and_tail() {
        let input = "1\n2\n3\n4\n";
        assert_eq!(run(head, &["-2"], input), "1\n2\n");
        assert_eq!(run(head, &["-n", "-1"], input), "1\n2\n3\n");
        assert_eq!(run(head, &["--lines=3"], input), "1\n2\n3\n");
        assert_eq!(run(head, &["-c", "3"], input), "1\n2");
        assert_eq!(run(tail, &["-n", "+3"], input), "3\n4\n");
        assert_eq!(run(tail, &["-1"], input), "4\n");
        assert_eq!(run(tail, &["+2"], input), "2\n3\n4\n");
        assert_eq!(run(head, &["-n", "5"], "a\nb"), "a\nb");
    }

    #[test]
    fn counts() {
        assert_eq!(run(wc, &["-l"], "a b\nc\n"), "2\n");
        assert_eq!(run(wc, &[], "a b\nc\n"), "      2       3       6\n");
        assert_eq!(run(wc, &["-lc"], "a b\nc\n"), "      2       6\n");
        assert_eq!(run(wc, &["-l"], "a\nb"), "1\n");
        assert_eq!(run(uniq, &["-c"], "x\nx\ny\n"), "      2 x\n      1 y\n");
        assert_eq!(run(uniq, &["-d"], "x\nx\ny\n"), "x\n");
        assert_eq!(run(cat, &["-n"], "a\nb\n"), "     1\ta\n     2\tb\n");
    }

    #[test]
    fn sorting() {
        assert_eq!(run(sort, &[], "B\na\n_x\nA\n"), "A\nB\n_x\na\n");
        assert_eq!(
            run(sort, &["-n"], "10\nb\n9\n-1\n a\n2x\n"),
            "-1\n a\nb\n2x\n9\n10\n"
        );
        assert_eq!(
            run(sort, &["-rn"], "  3 a\n 10 b\n  3 c\n"),
            " 10 b\n  3 c\n  3 a\n"
        );
        assert_eq!(
            run(sort, &["-t:", "-k2", "-n"], "a:2\nb:10\nc:1\n"),
            "c:1\na:2\nb:10\n"
        );
        assert_eq!(run(sort, &["-u"], "b\na\nb\n"), "a\nb\n");
        assert_eq!(run(sort, &[], "b\na"), "a\nb\n");
        assert_eq!(run(sort, &["-k2,2"], "x b\ny a\n"), "y a\nx b\n");
    }

    #[test]
    fn cutting() {
        assert_eq!(
            run(cut, &["-d:", "-f1"], "a:b:c\nnodelim\n"),
            "a\nnodelim\n"
        );
        assert_eq!(run(cut, &["-d", ":", "-f2-"], "a:b:c\n"), "b:c\n");
        assert_eq!(run(cut, &["-s", "-d:", "-f2"], "a:b:c\nnodelim\n"), "b\n");
        assert_eq!(run(cut, &["-c1-3"], "abcdef\nxy\n"), "abc\nxy\n");
        assert_eq!(run(cut, &["-c", "2,4-"], "abcdef\n"), "bdef\n");
        let args = vec!["-c2-3".to_owned()];
        assert_eq!(
            cut(&args, "h\u{e9}llo\n".as_bytes()).unwrap(),
            b"\xc3\xa9\n"
        );
    }
}

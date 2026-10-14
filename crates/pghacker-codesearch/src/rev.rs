//! Revisions in the commit history: what the `version` argument and the
//! virtual folders name. Accepted, case-insensitively where git allows:
//!
//! - a major version (`19`, `PG19`, `v19`): its `REL_19_STABLE` branch, or
//!   master while it has none; before 10, the newest release line (`9` is
//!   `REL9_6_STABLE`);
//! - a release (`18.3`, `9.6.24`): its tag; a release line before 10
//!   (`9.6`): its branch;
//! - any branch or tag (`master`, `REL_18_STABLE`, `REL_18_3`);
//! - a commit SHA or a prefix of at least 7 hex digits;
//! - any of these with `@<date>` (`REL_17_STABLE@2024-06-01`): the newest
//!   commit in its history committed at or before the date's end (UTC);
//! - then `~N` or `^` steps back along first parents (`master~3`).
//!
//! The default is the head of the newest numbered release line.

use std::sync::OnceLock;

use anyhow::{Context, Result, anyhow, bail};
use chrono::{DateTime, NaiveDate, Utc};
use regex::Regex;
use tokio_postgres::GenericClient;

use crate::snapshot::Snapshot;

const MASTER_STEP: i64 = 1 << 32;
const BRANCH_BLOCK: i64 = 1 << 24;

/// The address of `addr`'s first parent: the previous commit on its lane,
/// or the fork point for a branch's first commit. None for the root.
pub fn parent(addr: i64) -> Option<i64> {
    if addr % MASTER_STEP == 0 {
        return (addr > 0).then(|| addr - MASTER_STEP);
    }
    let offset = addr % MASTER_STEP;
    let first = addr - offset + (offset - 1) / BRANCH_BLOCK * BRANCH_BLOCK + 1;
    Some(if addr == first {
        addr - offset
    } else {
        addr - 1
    })
}

/// A revision's parts: the name, an optional date, and steps back.
#[derive(Debug, PartialEq, Eq)]
pub struct Spec {
    pub name: String,
    pub date: Option<DateTime<Utc>>,
    pub back: u32,
}

/// Splits `REL_17_STABLE@2024-06-01~2` into its parts.
pub fn parse(rev: &str) -> Result<Spec> {
    static STEPS: OnceLock<Regex> = OnceLock::new();
    let steps = STEPS.get_or_init(|| Regex::new(r"((~[0-9]*)|\^)+$").unwrap());
    let rev = rev.trim();
    let (base, back) = match steps.find(rev) {
        Some(m) if m.start() > 0 => {
            let mut back = 0u32;
            let tail = &rev[m.start()..];
            let mut chars = tail.chars().peekable();
            while let Some(c) = chars.next() {
                if c == '^' {
                    back += 1;
                } else {
                    let mut n = String::new();
                    while let Some(d) = chars.peek().filter(|d| d.is_ascii_digit()) {
                        n.push(*d);
                        chars.next();
                    }
                    back += if n.is_empty() { 1 } else { n.parse::<u32>()? };
                }
            }
            (&rev[..m.start()], back)
        }
        Some(_) => bail!("{rev}: name a revision before ~ or ^"),
        None => (rev, 0),
    };
    let (name, date) = match base.rsplit_once('@') {
        Some((name, date)) if !name.is_empty() => (name, Some(parse_date(date)?)),
        _ => (base, None),
    };
    if name.is_empty() {
        bail!("an empty revision");
    }
    Ok(Spec {
        name: name.to_owned(),
        date,
        back,
    })
}

/// A date's end, or an exact time.
fn parse_date(text: &str) -> Result<DateTime<Utc>> {
    if let Ok(t) = DateTime::parse_from_rfc3339(text) {
        return Ok(t.with_timezone(&Utc));
    }
    let day = NaiveDate::parse_from_str(text, "%Y-%m-%d")
        .with_context(|| format!("{text} is not a date such as 2024-06-01"))?;
    Ok(day.and_hms_opt(23, 59, 59).unwrap().and_utc())
}

/// A branch or tag.
#[derive(Clone, Debug)]
pub struct Ref {
    pub kind: String,
    pub name: String,
    pub addr: i64,
    pub sha: String,
}

/// A numbered release line's branch: (major, minor) for `REL_17_STABLE`
/// (17, 0) and `REL9_6_STABLE` (9, 6); `REL6_5_PATCHES` is (6, 5).
fn release_line(name: &str) -> Option<(u32, u32)> {
    static LINE: OnceLock<Regex> = OnceLock::new();
    let re =
        LINE.get_or_init(|| Regex::new(r"^REL_?(\d+)(?:_(\d+))?_(?:STABLE|PATCHES)$").unwrap());
    let c = re.captures(name)?;
    let major: u32 = c[1].parse().ok()?;
    let minor: u32 = c.get(2).map_or(Some(0), |m| m.as_str().parse().ok())?;
    Some((major, minor))
}

/// The major version a ref belongs to, as the snapshots label it.
pub fn version_of(name: &str) -> Option<String> {
    if name.eq_ignore_ascii_case("master") {
        return Some("master".into());
    }
    static MAJOR: OnceLock<Regex> = OnceLock::new();
    let re = MAJOR.get_or_init(|| Regex::new(r"^REL_?(\d+)[_A-Z]").unwrap());
    re.captures(name).map(|c| c[1].to_owned())
}

/// Picks the ref a name means, of `refs`.
pub fn pick<'a>(refs: &'a [Ref], name: &str) -> Option<&'a Ref> {
    let exact = |n: &str| -> Option<&'a Ref> {
        refs.iter()
            .filter(|r| r.name.eq_ignore_ascii_case(n))
            // A branch over a tag of the same name.
            .min_by_key(|r| r.kind != "branch")
    };
    if let Some(r) = exact(name) {
        return Some(r);
    }
    let norm = name
        .trim()
        .to_ascii_lowercase()
        .trim_start_matches("postgresql")
        .trim_start_matches("postgres")
        .trim_start_matches("pg")
        .trim_start_matches('v')
        .trim()
        .to_owned();
    let parts: Vec<&str> = norm.split('.').collect();
    if parts.is_empty()
        || !parts
            .iter()
            .all(|p| !p.is_empty() && p.bytes().all(|b| b.is_ascii_digit()))
    {
        return None;
    }
    let major: u32 = parts[0].parse().ok()?;
    let lines = || {
        refs.iter()
            .filter(|r| r.kind == "branch")
            .filter_map(|r| release_line(&r.name).map(|l| (l, r)))
    };
    match (major >= 10, parts.len()) {
        // 18.3 is REL_18_3.
        (true, 2) => exact(&format!("REL_{}_{}", parts[0], parts[1])),
        // 9.6.24 is REL9_6_24.
        (false, 3) => exact(&format!("REL{}_{}_{}", parts[0], parts[1], parts[2])),
        // 9.6 is the REL9_6_STABLE line.
        (false, 2) => {
            let minor: u32 = parts[1].parse().ok()?;
            lines().find(|(l, _)| *l == (major, minor)).map(|(_, r)| r)
        }
        // 19 is REL_19_STABLE, or master while it has none.
        (true, 1) => lines()
            .find(|(l, _)| *l == (major, 0))
            .map(|(_, r)| r)
            .or_else(|| {
                let newest = lines().map(|(l, _)| l.0).max()?;
                (major == newest + 1).then(|| exact("master")).flatten()
            }),
        // 9 is the newest 9.x line.
        (false, 1) => lines()
            .filter(|(l, _)| l.0 == major)
            .max_by_key(|(l, _)| l.1)
            .map(|(_, r)| r),
        _ => None,
    }
}

/// The default: the head of the newest numbered release line, else master.
pub fn default_ref(refs: &[Ref]) -> Option<&Ref> {
    refs.iter()
        .filter(|r| r.kind == "branch")
        .filter_map(|r| release_line(&r.name).map(|l| (l, r)))
        .max_by_key(|(l, _)| *l)
        .map(|(_, r)| r)
        .or_else(|| refs.iter().find(|r| r.name == "master"))
}

/// Resolves `version` in the history in `schema` to a commit, as a
/// snapshot that the code tools read through the history.
pub async fn resolve(
    client: &impl GenericClient,
    schema: &str,
    repo: &str,
    version: Option<&str>,
) -> Result<Snapshot> {
    let s = crate::history::checked(schema)?;
    let refs: Vec<Ref> = client
        .query(
            &format!(
                "SELECT r.kind, r.name, r.addr, c.sha FROM {s}.code_refs r \
                 JOIN {s}.code_commits c ON c.addr = r.addr"
            ),
            &[],
        )
        .await
        .with_context(|| format!("cannot read the history's refs in schema {s}"))?
        .iter()
        .map(|row| Ref {
            kind: row.get(0),
            name: row.get(1),
            addr: row.get(2),
            sha: row.get(3),
        })
        .collect();
    let wanted = version.map(str::trim).filter(|v| !v.is_empty());
    let spec = match wanted {
        Some(v) => parse(v)?,
        None => Spec {
            name: default_ref(&refs)
                .context("the history has no branches")?
                .name
                .clone(),
            date: None,
            back: 0,
        },
    };
    let (git_ref, mut addr) = match pick(&refs, &spec.name) {
        Some(r) => (r.name.clone(), r.addr),
        None => {
            let lower = spec.name.to_ascii_lowercase();
            if lower.len() < 7 || !lower.bytes().all(|b| b.is_ascii_hexdigit()) {
                let mut names: Vec<&str> = refs
                    .iter()
                    .filter(|r| r.kind == "branch")
                    .map(|r| r.name.as_str())
                    .collect();
                names.sort();
                bail!(
                    "no revision {}: give a branch ({}), a tag such as REL_18_3, a release such \
                     as 18.3, a commit SHA, REL_17_STABLE@2024-06-01 or master~3",
                    spec.name,
                    names.join(", ")
                );
            }
            let rows = client
                .query(
                    &format!(
                        "SELECT addr, sha FROM {s}.code_commits WHERE sha LIKE $1 || '%' LIMIT 2"
                    ),
                    &[&lower],
                )
                .await?;
            match rows.as_slice() {
                [row] => (row.get::<_, String>(1), row.get(0)),
                [] => bail!("no commit {} in the history", spec.name),
                _ => bail!("{} names more than one commit; give more digits", spec.name),
            }
        }
    };
    if let Some(date) = spec.date {
        // The newest commit at or before the date among addr's ancestors:
        // its lane up to it, then master up to the fork.
        let row = client
            .query_opt(
                &format!(
                    "SELECT c.addr FROM {s}.code_commits c \
                     WHERE c.committed_at <= $2 AND ( \
                         (c.addr % {MASTER_STEP} = 0 AND c.addr <= $1) OR \
                         (c.addr % {MASTER_STEP} <> 0 AND c.addr <= $1 AND \
                          c.addr >= $1 - ($1 % {MASTER_STEP}) + (($1 % {MASTER_STEP}) - 1) / {BRANCH_BLOCK} * {BRANCH_BLOCK} + 1)) \
                     ORDER BY c.addr DESC LIMIT 1"
                ),
                &[&addr, &date],
            )
            .await?;
        addr = row
            .with_context(|| format!("{} has no commit at or before {date}", spec.name))?
            .get(0);
    }
    for _ in 0..spec.back {
        addr =
            parent(addr).ok_or_else(|| anyhow!("{} goes back past the first commit", spec.name))?;
    }
    let row = client
        .query_one(
            &format!(
                "SELECT c.sha, l.name, current_database()::text || ':' || h.built_at || ':' || h.weave_epoch \
                 FROM {s}.code_commits c JOIN {s}.code_lanes l ON l.id = c.lane, {s}.code_history h \
                 WHERE c.addr = $1"
            ),
            &[&addr],
        )
        .await?;
    let sha: String = row.get(0);
    let lane: String = row.get(1);
    // A ref names the commit as given; a SHA, a date or steps name its lane.
    let git_ref = if spec.date.is_none() && spec.back == 0 && git_ref != sha {
        git_ref
    } else {
        lane
    };
    Ok(Snapshot {
        id: 0,
        repo: repo.to_owned(),
        version: version_of(&git_ref),
        git_ref,
        sha,
        history: Some(crate::history::History {
            schema: s.to_owned(),
            addr,
            build: row.get(2),
        }),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn r(kind: &str, name: &str, addr: i64) -> Ref {
        Ref {
            kind: kind.into(),
            name: name.into(),
            addr,
            sha: format!("{addr:040x}"),
        }
    }

    fn refs() -> Vec<Ref> {
        vec![
            r("branch", "master", 900 * MASTER_STEP),
            r("branch", "REL_19_STABLE", 800 * MASTER_STEP + 5),
            r("branch", "REL_18_STABLE", 700 * MASTER_STEP + 5),
            r("branch", "REL9_6_STABLE", 100 * MASTER_STEP + 5),
            r("branch", "REL9_5_STABLE", 90 * MASTER_STEP + 5),
            r("branch", "REL6_5_PATCHES", 10 * MASTER_STEP + 5),
            r("tag", "REL_18_3", 700 * MASTER_STEP + 3),
            r("tag", "REL9_6_24", 100 * MASTER_STEP + 4),
        ]
    }

    #[test]
    fn steps_back_along_first_parents() {
        assert_eq!(parent(3 * MASTER_STEP), Some(2 * MASTER_STEP));
        assert_eq!(parent(0), None);
        let first = 3 * MASTER_STEP + BRANCH_BLOCK + 1;
        assert_eq!(parent(first), Some(3 * MASTER_STEP));
        assert_eq!(parent(first + 4), Some(first + 3));
        assert_eq!(parent(3 * MASTER_STEP + 1), Some(3 * MASTER_STEP));
    }

    #[test]
    fn parses_revisions() {
        assert_eq!(
            parse("REL_17_STABLE@2024-06-01~2").unwrap(),
            Spec {
                name: "REL_17_STABLE".into(),
                date: Some(parse_date("2024-06-01").unwrap()),
                back: 2,
            }
        );
        assert_eq!(parse("master^^").unwrap().back, 2);
        assert_eq!(parse("master~").unwrap().back, 1);
        assert_eq!(parse("abc1234~3^").unwrap().back, 4);
        assert_eq!(parse("19").unwrap().name, "19");
        assert!(parse("master@yesterday").is_err());
        assert!(parse("~2").is_err());
    }

    #[test]
    fn picks_versions_releases_and_refs() {
        let refs = refs();
        let name = |v: &str| pick(&refs, v).map(|r| r.name.as_str());
        assert_eq!(name("master"), Some("master"));
        assert_eq!(name("rel_18_stable"), Some("REL_18_STABLE"));
        assert_eq!(name("19"), Some("REL_19_STABLE"));
        assert_eq!(name("PG18"), Some("REL_18_STABLE"));
        assert_eq!(name("20"), Some("master"));
        assert_eq!(name("21"), None);
        assert_eq!(name("18.3"), Some("REL_18_3"));
        assert_eq!(name("9.6.24"), Some("REL9_6_24"));
        assert_eq!(name("9.6"), Some("REL9_6_STABLE"));
        assert_eq!(name("9"), Some("REL9_6_STABLE"));
        assert_eq!(name("6"), Some("REL6_5_PATCHES"));
        assert_eq!(name("nope"), None);
        assert_eq!(default_ref(&refs).unwrap().name, "REL_19_STABLE");
        assert_eq!(version_of("REL_19_STABLE").as_deref(), Some("19"));
        assert_eq!(version_of("REL9_6_STABLE").as_deref(), Some("9"));
        assert_eq!(version_of("REL_18_3").as_deref(), Some("18"));
        assert_eq!(version_of("master").as_deref(), Some("master"));
    }
}

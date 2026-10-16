// Copyright (c) Microsoft Corporation.
// Licensed under the MIT license.

//! Agent Skills (<https://agentskills.io>) served over MCP, as the
//! `io.modelcontextprotocol/skills` extension (SEP-2640) defines them. Each
//! file of a skill is a resource at `skill://<skill-path>/<file-path>`, where
//! the skill path's last segment is the skill's name. `skills/list` and
//! `skills/get` give a skill's frontmatter, verbatim, and the digest and size
//! of each of its files, so that hosts can check what they read.
//!
//! The skills are text files embedded in the binary. Startup refuses a skill
//! that breaks the Agent Skills rules or the extension's limits.

use std::collections::BTreeMap;
use std::sync::Arc;

use anyhow::{Context, Result, anyhow, bail, ensure};
use include_dir::{Dir, DirEntry};
use rmcp::model::Resource;
use serde_json::{Map, Value, json};
use sha2::{Digest, Sha256};

/// The extension's identifier, in the server's capabilities.
pub const EXTENSION: &str = "io.modelcontextprotocol/skills";
/// Every skill's instructions.
pub const SKILL_FILE: &str = "SKILL.md";
const SCHEME: &str = "skill://";
/// The extension's limits per skill, which every host accepts.
const MAX_FILES: usize = 512;
const MAX_BYTES: usize = 16 * 1024 * 1024;

/// A file of a skill.
#[derive(Debug)]
pub struct SkillFile {
    /// `skill://<skill-path>/<file-path>`.
    pub uri: String,
    /// The path from the skill's directory, such as `SKILL.md`.
    pub path: String,
    pub mime_type: &'static str,
    pub text: String,
    /// `sha256:` and the hex digest of the text's bytes.
    pub digest: String,
}

/// A skill: a directory with a `SKILL.md`.
#[derive(Debug)]
pub struct Skill {
    /// The `SKILL.md`'s URI, which identifies the skill.
    pub uri: String,
    pub name: String,
    pub description: String,
    /// The `SKILL.md`'s YAML frontmatter, as JSON.
    pub frontmatter: Map<String, Value>,
    /// `SKILL.md` first, then the others by path.
    pub files: Vec<SkillFile>,
}

impl Skill {
    /// The skill's entry, as `skills/list` and `skills/get` give it.
    pub fn entry(&self) -> Value {
        let resources: Vec<Value> = self
            .files
            .iter()
            .map(|file| json!({ "uri": file.uri, "digest": file.digest, "size": file.text.len() }))
            .collect();
        json!({ "uri": self.uri, "frontmatter": self.frontmatter, "resources": resources })
    }
}

/// The skills a server serves. Cheap to clone.
#[derive(Clone, Debug, Default)]
pub struct Skills {
    skills: Arc<Vec<Skill>>,
}

impl Skills {
    /// The skills in `dir`, such as one embedded with `include_dir!`: each
    /// folder with a `SKILL.md` is a skill, its path from `dir` the skill's
    /// path. Files whose names start with a dot are left out.
    pub fn from_dir(dir: &Dir<'_>) -> Result<Self> {
        let mut files = Vec::new();
        collect(dir, &mut files)?;
        Self::from_files(files)
    }

    /// The skills among `files`, by their paths from the skills' root.
    pub fn from_files<P: Into<String>>(
        files: impl IntoIterator<Item = (P, Vec<u8>)>,
    ) -> Result<Self> {
        let mut texts = BTreeMap::new();
        for (path, bytes) in files {
            let path: String = path.into();
            let text = String::from_utf8(bytes)
                .map_err(|_| anyhow!("skill file {path} isn't UTF-8 text"))?;
            texts.insert(path, text);
        }
        let mut skills = Vec::new();
        for path in texts.keys() {
            let Some(dir) = path.strip_suffix(SKILL_FILE) else {
                continue;
            };
            if !(dir.is_empty() || dir.ends_with('/')) {
                continue;
            }
            let dir = dir.trim_end_matches('/');
            ensure!(
                !dir.is_empty(),
                "{SKILL_FILE} must be in a skill's folder, not at the root"
            );
            skills.push(skill(dir, &texts).with_context(|| format!("skill {dir}"))?);
        }
        Ok(Self {
            skills: Arc::new(skills),
        })
    }

    pub fn is_empty(&self) -> bool {
        self.skills.is_empty()
    }

    pub fn iter(&self) -> impl Iterator<Item = &Skill> {
        self.skills.iter()
    }

    /// The skill whose `SKILL.md` is at `uri`.
    pub fn get(&self, uri: &str) -> Option<&Skill> {
        self.skills.iter().find(|skill| skill.uri == uri)
    }

    /// The file at `uri`, of any skill.
    pub fn file(&self, uri: &str) -> Option<&SkillFile> {
        self.skills
            .iter()
            .flat_map(|skill| &skill.files)
            .find(|file| file.uri == uri)
    }

    /// Every file, as resources/list describes it: a `SKILL.md` by its skill's
    /// name and description, another file by its path.
    pub fn resources(&self) -> Vec<Resource> {
        let mut resources = Vec::new();
        for skill in self.skills.iter() {
            for file in &skill.files {
                let resource = if file.path == SKILL_FILE {
                    Resource::new(&file.uri, &skill.name).with_description(&skill.description)
                } else {
                    Resource::new(&file.uri, &file.path)
                };
                resources.push(
                    resource
                        .with_mime_type(file.mime_type)
                        .with_size(file.text.len() as u64),
                );
            }
        }
        resources
    }
}

fn collect(dir: &Dir<'_>, files: &mut Vec<(String, Vec<u8>)>) -> Result<()> {
    for entry in dir.entries() {
        let name = entry
            .path()
            .file_name()
            .and_then(|name| name.to_str())
            .unwrap_or_default();
        if name.starts_with('.') {
            continue;
        }
        match entry {
            DirEntry::Dir(child) => collect(child, files)?,
            DirEntry::File(file) => {
                let path = file
                    .path()
                    .to_str()
                    .ok_or_else(|| anyhow!("a skill file's path isn't UTF-8"))?
                    .replace('\\', "/");
                files.push((path, file.contents().to_vec()));
            }
        }
    }
    Ok(())
}

/// The skill in folder `dir`, with every file below it.
fn skill(dir: &str, texts: &BTreeMap<String, String>) -> Result<Skill> {
    let name = dir.rsplit('/').next().unwrap_or(dir);
    for segment in dir.split('/') {
        ensure!(
            valid_name(segment),
            "{segment:?}: a skill path's segments are lowercase letters, digits and single hyphens"
        );
    }
    let instructions = &texts[&format!("{dir}/{SKILL_FILE}")];
    let frontmatter = frontmatter(instructions)?;
    let field = |key: &str| frontmatter.get(key).and_then(Value::as_str);
    ensure!(
        field("name") == Some(name),
        "the frontmatter's name must be {name:?}, the folder's name"
    );
    let description = field("description").unwrap_or_default();
    ensure!(
        (1..=1024).contains(&description.chars().count()),
        "the description must have 1 to 1024 characters"
    );
    if let Some(compatibility) = frontmatter.get("compatibility") {
        let length = compatibility
            .as_str()
            .map_or(0, |text| text.chars().count());
        ensure!(
            (1..=500).contains(&length),
            "compatibility must be text of 1 to 500 characters"
        );
    }
    for key in ["license", "allowed-tools"] {
        ensure!(
            frontmatter.get(key).is_none_or(Value::is_string),
            "{key} must be text"
        );
    }
    if let Some(metadata) = frontmatter.get("metadata") {
        let map = metadata
            .as_object()
            .ok_or_else(|| anyhow!("metadata must be a mapping"))?;
        ensure!(
            map.values().all(Value::is_string),
            "metadata's values must be text"
        );
    }

    let prefix = format!("{dir}/");
    let mut files: Vec<SkillFile> = texts
        .range(prefix.clone()..)
        .take_while(|(path, _)| path.starts_with(&prefix))
        .map(|(path, text)| {
            let relative = &path[prefix.len()..];
            SkillFile {
                uri: format!("{SCHEME}{dir}/{relative}"),
                path: relative.to_owned(),
                mime_type: mime_type(relative),
                text: text.clone(),
                digest: format!("sha256:{}", hex::encode(Sha256::digest(text.as_bytes()))),
            }
        })
        .collect();
    files.sort_by_key(|file| (file.path != SKILL_FILE, file.path.clone()));
    ensure!(
        files.len() <= MAX_FILES,
        "a skill has at most {MAX_FILES} files"
    );
    let bytes: usize = files.iter().map(|file| file.text.len()).sum();
    ensure!(
        bytes <= MAX_BYTES,
        "a skill's files total at most {MAX_BYTES} bytes"
    );
    Ok(Skill {
        uri: format!("{SCHEME}{dir}/{SKILL_FILE}"),
        name: name.to_owned(),
        description: description.to_owned(),
        frontmatter,
        files,
    })
}

/// A skill name: 1 to 64 lowercase letters, digits and hyphens, without a
/// hyphen first, last or twice in a row.
fn valid_name(name: &str) -> bool {
    (1..=64).contains(&name.len())
        && name
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
        && !name.starts_with('-')
        && !name.ends_with('-')
        && !name.contains("--")
}

/// The YAML frontmatter that starts `text`, as a JSON object.
pub fn frontmatter(text: &str) -> Result<Map<String, Value>> {
    let text = text.strip_prefix('\u{feff}').unwrap_or(text);
    let mut lines = text.split_inclusive('\n');
    if lines.next().map(str::trim_end) != Some("---") {
        bail!("{SKILL_FILE} must start with YAML frontmatter, between --- lines");
    }
    let mut yaml = String::new();
    let mut closed = false;
    for line in lines {
        if line.trim_end() == "---" {
            closed = true;
            break;
        }
        yaml.push_str(line);
    }
    ensure!(closed, "{SKILL_FILE}'s frontmatter has no closing --- line");
    match serde_yaml_ng::from_str::<Value>(&yaml).context("the frontmatter isn't valid YAML")? {
        Value::Object(map) => Ok(map),
        _ => bail!("the frontmatter must be a YAML mapping"),
    }
}

fn mime_type(path: &str) -> &'static str {
    match path.rsplit_once('.').map(|(_, extension)| extension) {
        Some("md") => "text/markdown",
        Some("json") => "application/json",
        Some("yaml" | "yml") => "application/yaml",
        Some("py") => "text/x-python",
        Some("sh") => "text/x-shellscript",
        Some("sql") => "application/sql",
        _ => "text/plain",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const SKILL: &str = "---\nname: demo\ndescription: 'Shows how. Use when asked: \"demo\".'\nlicense: MIT\nmetadata:\n  version: \"1\"\n---\n\n# Demo\n\nSee references/notes.md.\n";

    fn files(entries: &[(&str, &str)]) -> Vec<(String, Vec<u8>)> {
        entries
            .iter()
            .map(|(path, text)| (path.to_string(), text.as_bytes().to_vec()))
            .collect()
    }

    #[test]
    fn a_skill_is_its_files_with_digests_and_verbatim_frontmatter() {
        let skills = Skills::from_files(files(&[
            ("demo/references/notes.md", "notes"),
            ("demo/SKILL.md", SKILL),
            ("other.txt", "not part of a skill"),
        ]))
        .unwrap();
        let skill = skills.get("skill://demo/SKILL.md").unwrap();
        assert_eq!(skill.name, "demo");
        assert_eq!(
            Value::Object(skill.frontmatter.clone()),
            json!({ "name": "demo", "description": "Shows how. Use when asked: \"demo\".",
                    "license": "MIT", "metadata": { "version": "1" } })
        );
        let entry = skill.entry();
        assert_eq!(entry["resources"][0]["uri"], "skill://demo/SKILL.md");
        assert_eq!(
            entry["resources"][1]["uri"],
            "skill://demo/references/notes.md"
        );
        assert_eq!(entry["resources"][1]["size"], 5);
        assert_eq!(
            entry["resources"][1]["digest"],
            format!("sha256:{}", hex::encode(Sha256::digest(b"notes")))
        );
        assert_eq!(
            skills
                .file("skill://demo/references/notes.md")
                .unwrap()
                .text,
            "notes"
        );
        assert!(skills.file("skill://other.txt").is_none());
        let resources = skills.resources();
        assert_eq!(resources.len(), 2);
        assert_eq!(resources[0].name, "demo");
        assert_eq!(resources[0].mime_type.as_deref(), Some("text/markdown"));
    }

    #[test]
    fn skills_may_have_a_prefix_whose_last_segment_is_the_name() {
        let skills = Skills::from_files(files(&[("team/demo/SKILL.md", SKILL)])).unwrap();
        assert!(skills.get("skill://team/demo/SKILL.md").is_some());
    }

    #[test]
    fn skills_that_break_the_rules_are_refused() {
        let refused = |entries: &[(&str, &str)]| {
            format!("{:#}", Skills::from_files(files(entries)).unwrap_err())
        };
        assert!(refused(&[("other/SKILL.md", SKILL)]).contains("folder's name"));
        assert!(refused(&[("Demo/SKILL.md", SKILL)]).contains("lowercase"));
        assert!(refused(&[("demo/SKILL.md", "# no frontmatter")]).contains("frontmatter"));
        assert!(refused(&[("demo/SKILL.md", "---\nname: demo\n")]).contains("closing"));
        assert!(
            refused(&[("demo/SKILL.md", "---\nname: demo\ndescription: \"\"\n---\n")])
                .contains("description")
        );
        assert!(
            refused(&[(
                "demo/SKILL.md",
                "---\nname: demo\ndescription: d\nmetadata:\n  n: 1\n---\n"
            )])
            .contains("metadata")
        );
        assert!(
            refused(&[("demo/SKILL.md", "---\nname: demo\ndescription: a: b\n---\n")])
                .contains("YAML")
        );
        assert!(refused(&[("SKILL.md", SKILL)]).contains("folder"));
        let long = format!("---\nname: demo\ndescription: {}\n---\n", "x".repeat(1025));
        assert!(refused(&[("demo/SKILL.md", &long)]).contains("1024"));
        let binary = Skills::from_files(vec![("demo/SKILL.md".to_owned(), vec![0xff, 0xfe])]);
        assert!(format!("{:#}", binary.unwrap_err()).contains("UTF-8"));
    }

    #[test]
    fn names_follow_the_agent_skills_rules() {
        for good in ["review-as-author", "a", "pdf2text"] {
            assert!(valid_name(good), "{good}");
        }
        for bad in ["", "-a", "a-", "a--b", "Review", "a_b", &"a".repeat(65)] {
            assert!(!valid_name(bad), "{bad}");
        }
    }
}

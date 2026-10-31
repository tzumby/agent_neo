//! Skills: how-tos for common jobs on the desktop (installing software, Python
//! and Node projects, git and GitHub). Each is a Markdown file with a name and
//! a one-line description in its front matter. The system prompt lists them;
//! the agent reads one with read_skill when it starts that kind of job.

use std::path::Path;

pub struct Skill {
    pub name: String,
    pub description: String,
    pub body: String,
}

#[derive(Default)]
pub struct Skills(Vec<Skill>);

impl Skills {
    /// Every `*.md` in `dir` (none if it doesn't exist); bad files are skipped.
    pub fn load(dir: &Path) -> Self {
        let Ok(entries) = std::fs::read_dir(dir) else {
            return Self::default();
        };
        let mut skills: Vec<Skill> = entries
            .filter_map(Result::ok)
            .map(|entry| entry.path())
            .filter(|path| path.extension().is_some_and(|ext| ext == "md"))
            .filter_map(|path| {
                let skill = std::fs::read_to_string(&path).ok().and_then(|text| parse(&text));
                if skill.is_none() {
                    tracing::warn!(path = %path.display(), "skipping a skill without name and description");
                }
                skill
            })
            .collect();
        skills.sort_by(|a, b| a.name.cmp(&b.name));
        Self(skills)
    }

    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    /// "- name: description" lines for the system prompt.
    pub fn listing(&self) -> String {
        self.0
            .iter()
            .map(|skill| format!("- {}: {}\n", skill.name, skill.description))
            .collect()
    }

    pub fn read(&self, name: &str) -> Option<&str> {
        self.0
            .iter()
            .find(|skill| skill.name == name.trim())
            .map(|skill| skill.body.as_str())
    }
}

/// "---\nname: x\ndescription: y\n---\nbody".
fn parse(text: &str) -> Option<Skill> {
    let rest = text.strip_prefix("---\n")?;
    let (front, body) = rest.split_once("\n---\n")?;
    let field = |key: &str| {
        front
            .lines()
            .find_map(|line| line.strip_prefix(key)?.strip_prefix(':'))
            .map(|value| value.trim().to_string())
            .filter(|value| !value.is_empty())
    };
    Some(Skill {
        name: field("name")?,
        description: field("description")?,
        body: body.trim().to_string(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_front_matter() {
        let skill = parse(
            "---\nname: install-software\ndescription: How to install tools.\n---\n# Install\nUse Nix.\n",
        )
        .unwrap();
        assert_eq!(skill.name, "install-software");
        assert_eq!(skill.description, "How to install tools.");
        assert_eq!(skill.body, "# Install\nUse Nix.");
        assert!(parse("# no front matter").is_none());
        assert!(parse("---\nname: x\n---\nbody").is_none()); // no description
    }

    #[test]
    fn the_shipped_skills_load() {
        let skills = Skills::load(&Path::new(env!("CARGO_MANIFEST_DIR")).join("skills"));
        assert!(
            skills
                .read("install-software")
                .is_some_and(|body| body.contains("nix profile add"))
        );
        assert!(skills.listing().contains("- install-software: "));
        assert!(Skills::load(Path::new("/nonexistent")).is_empty());
    }
}

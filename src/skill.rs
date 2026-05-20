use std::collections::HashMap;
use std::path::{Path, PathBuf};

#[derive(Debug, Clone)]
pub struct Skill {
    pub name: String,
    pub description: String,
    pub content: String,
    #[allow(dead_code)]
    pub location: PathBuf,
}

pub fn discover_skills(cwd: &Path) -> Vec<Skill> {
    let mut map: HashMap<String, Skill> = HashMap::new();

    let global_dirs = dirs::home_dir().into_iter().flat_map(|home| {
        [
            home.join(".claude").join("skills"),
            home.join(".maki").join("skills"),
            home.join(".opencode").join("skills"),
            home.join(".dirge").join("skills"),
        ]
    });

    let project_dirs = find_project_ancestor_dirs(cwd)
        .into_iter()
        .flat_map(|ancestor| {
            [
                ancestor.join(".claude").join("skills"),
                ancestor.join(".maki").join("skills"),
                ancestor.join(".opencode").join("skills"),
                ancestor.join(".dirge").join("skills"),
            ]
        });

    for dir in global_dirs.chain(project_dirs) {
        if let Ok(entries) = std::fs::read_dir(&dir) {
            for entry in entries.flatten() {
                let path = entry.path();
                if !path.is_dir() {
                    continue;
                }
                let skill_md = path.join("SKILL.md");
                if !skill_md.is_file() {
                    continue;
                }
                // Cap skill content at 1 MB. A skill is meant to be a
                // short markdown instructions file; multi-MB skills
                // would blow up LLM context. If users have legitimate
                // need for larger skills, they should compress and
                // bump this cap deliberately.
                const SKILL_MAX_BYTES: u64 = 1024 * 1024;
                if let Ok(meta) = std::fs::metadata(&skill_md)
                    && meta.len() > SKILL_MAX_BYTES
                {
                    eprintln!(
                        "warning: skipping skill {:?} ({} bytes > 1 MB cap)",
                        skill_md,
                        meta.len(),
                    );
                    continue;
                }
                if let Ok(content) = std::fs::read_to_string(&skill_md) {
                    if let Some(skill) = parse_skill(&content, &path) {
                        map.entry(skill.name.clone()).or_insert(skill);
                    }
                }
            }
        }
    }

    let mut skills: Vec<Skill> = map.into_values().collect();
    skills.sort_by(|a, b| a.name.cmp(&b.name));
    skills
}

pub fn find_project_ancestor_dirs(cwd: &Path) -> Vec<PathBuf> {
    let mut dirs = Vec::new();
    let mut current = cwd.to_path_buf();
    dirs.push(current.clone());
    loop {
        if current.join(".git").is_dir() {
            if !dirs.contains(&current) {
                dirs.push(current.clone());
            }
        }
        if !current.pop() {
            break;
        }
    }
    dirs
}

fn parse_skill(content: &str, dir_path: &Path) -> Option<Skill> {
    let dir_name = dir_path
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("unknown");

    let (frontmatter, body) = split_frontmatter(content);
    let body = body.trim();
    if body.is_empty() {
        return None;
    }

    let (name, description) = if frontmatter.is_empty() {
        (dir_name.to_string(), String::new())
    } else {
        parse_frontmatter(&frontmatter, dir_name)
    };

    Some(Skill {
        name,
        description,
        content: body.to_string(),
        location: dir_path.to_path_buf(),
    })
}

pub(crate) fn split_frontmatter(content: &str) -> (String, String) {
    let content = if let Some(c) = content.strip_prefix("---\n") {
        c
    } else if let Some(c) = content.strip_prefix("---\r\n") {
        c
    } else {
        return (String::new(), content.to_string());
    };

    if let Some(pos) = content.find("\r\n---") {
        let frontmatter = &content[..pos];
        let body = &content[pos + 5..];
        (frontmatter.to_string(), body.to_string())
    } else if let Some(pos) = content.find("\n---") {
        let frontmatter = &content[..pos];
        let body = &content[pos + 4..];
        (frontmatter.to_string(), body.to_string())
    } else {
        (String::new(), content.to_string())
    }
}

pub(crate) fn parse_frontmatter(frontmatter: &str, default_name: &str) -> (String, String) {
    let mut name = default_name.to_string();
    let mut description = String::new();

    for line in frontmatter.lines() {
        let line = line.trim();
        if let Some(value) = line.strip_prefix("name:") {
            name = value.trim().to_string();
        } else if let Some(value) = line.strip_prefix("description:") {
            description = value.trim().to_string();
        }
    }

    (name, description)
}

pub fn build_skill_list_description(skills: &[Skill]) -> String {
    if skills.is_empty() {
        return String::new();
    }
    let mut desc = String::from("\n<available_skills>\n");
    for skill in skills {
        desc.push_str(&format!("- {}: {}\n", skill.name, skill.description));
    }
    desc.push_str("</available_skills>\n");
    desc
}

pub fn find_skill<'a>(name: &str, skills: &'a [Skill]) -> Option<&'a Skill> {
    skills.iter().find(|s| s.name == name)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_split_frontmatter() {
        let (fm, body) = split_frontmatter("---\nname: test\ndescription: desc\n---\nbody here");
        assert_eq!(fm, "name: test\ndescription: desc");
        assert_eq!(body.trim(), "body here");
    }

    #[test]
    fn test_split_frontmatter_no_fm() {
        let (fm, body) = split_frontmatter("just body");
        assert!(fm.is_empty());
        assert_eq!(body, "just body");
    }

    #[test]
    fn test_split_frontmatter_crlf() {
        let (fm, body) = split_frontmatter("---\r\nname: test\r\n---\r\nbody");
        assert_eq!(fm, "name: test");
        assert_eq!(body.trim(), "body");
    }

    #[test]
    fn test_parse_frontmatter() {
        let (name, desc) = parse_frontmatter("name: my-skill\ndescription: Does stuff", "default");
        assert_eq!(name, "my-skill");
        assert_eq!(desc, "Does stuff");
    }

    #[test]
    fn test_parse_frontmatter_falls_back_to_default_name() {
        let (name, desc) = parse_frontmatter("description: Does stuff", "dir-name");
        assert_eq!(name, "dir-name");
        assert_eq!(desc, "Does stuff");
    }

    #[test]
    fn test_parse_skill_rejects_empty_body() {
        let skill = parse_skill("---\nname: test\n---\n   \n", Path::new("/tmp/test-skill"));
        assert!(skill.is_none());
    }

    #[test]
    fn test_build_skill_list_description() {
        let skills = vec![Skill {
            name: "git-release".into(),
            description: "Create releases".into(),
            content: "...".into(),
            location: PathBuf::from("/tmp"),
        }];
        let desc = build_skill_list_description(&skills);
        assert!(desc.contains("git-release"));
        assert!(desc.contains("Create releases"));
    }

    #[test]
    fn test_build_skill_list_description_empty() {
        let desc = build_skill_list_description(&[]);
        assert!(desc.is_empty());
    }

    #[test]
    fn test_find_skill() {
        let skills = vec![Skill {
            name: "test".into(),
            description: "desc".into(),
            content: "body".into(),
            location: PathBuf::from("/tmp"),
        }];
        assert!(find_skill("test", &skills).is_some());
        assert!(find_skill("missing", &skills).is_none());
    }
}

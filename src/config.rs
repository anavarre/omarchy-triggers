use anyhow::{bail, Context, Result};
use serde::Deserialize;
use std::collections::HashSet;
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Snippet {
    pub trigger: String,
    pub snippet: String,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Config {
    #[serde(default)]
    pub snippets: Vec<Snippet>,
}

impl Config {
    pub fn load(path: &Path) -> Result<Self> {
        let text = std::fs::read_to_string(path)
            .with_context(|| format!("reading config {}", path.display()))?;
        if let Some(mode) = others_can_read(path) {
            eprintln!(
                "triggers: warning: {} is readable by other users (mode {mode:o}); \
                 snippets can hold secrets, so consider chmod 600",
                path.display()
            );
        }
        Self::parse(&text).with_context(|| format!("in config {}", path.display()))
    }

    pub fn parse(text: &str) -> Result<Self> {
        let mut config: Config = toml::from_str(text)?;
        let mut seen = HashSet::new();
        for s in &config.snippets {
            if s.trigger.is_empty() {
                bail!("empty trigger (snippet: {:?})", s.snippet);
            }
            if s.trigger.chars().any(char::is_whitespace) {
                bail!("trigger {:?} contains whitespace", s.trigger);
            }
            if s.trigger.chars().count() > crate::matcher::MAX_TRIGGER_LEN {
                bail!(
                    "trigger {:?} is longer than {} chars",
                    s.trigger,
                    crate::matcher::MAX_TRIGGER_LEN
                );
            }
            if !seen.insert(s.trigger.as_str()) {
                bail!("duplicate trigger {:?}", s.trigger);
            }
        }
        // Longest first, so `;sig2` wins over `;sig` when both match.
        config
            .snippets
            .sort_by_key(|s| std::cmp::Reverse(s.trigger.chars().count()));
        Ok(config)
    }
}

/// The file's permission bits if group or others can read it.
fn others_can_read(path: &Path) -> Option<u32> {
    use std::os::unix::fs::PermissionsExt;
    let mode = std::fs::metadata(path).ok()?.permissions().mode() & 0o777;
    (mode & 0o044 != 0).then_some(mode)
}

pub fn default_path() -> PathBuf {
    let base = std::env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .filter(|p| p.is_absolute())
        .unwrap_or_else(|| {
            PathBuf::from(std::env::var_os("HOME").unwrap_or_default()).join(".config")
        });
    base.join("triggers").join("config.toml")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_and_sorts() {
        let c = Config::parse(
            r#"
            [[snippets]]
            trigger = "rg"
            snippet = "ripgrep"

            [[snippets]]
            trigger = ";sig"
            snippet = """
Best,
Me"""
            "#,
        )
        .unwrap();
        assert_eq!(c.snippets[0].trigger, ";sig");
        assert_eq!(c.snippets[0].snippet, "Best,\nMe");
        assert_eq!(c.snippets[1].snippet, "ripgrep");
    }

    #[test]
    fn rejects_bad_triggers() {
        let dup = "[[snippets]]\ntrigger='a'\nsnippet='x'\n[[snippets]]\ntrigger='a'\nsnippet='y'";
        assert!(Config::parse(dup).is_err());
        assert!(Config::parse("[[snippets]]\ntrigger='a b'\nsnippet='x'").is_err());
        assert!(Config::parse("[[snippets]]\ntrigger=''\nsnippet='x'").is_err());
        assert!(Config::parse("[[snippets]]\ntrigger='a'\nreplace='x'").is_err());
    }
}

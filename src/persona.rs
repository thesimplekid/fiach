use std::path::PathBuf;
use std::str::FromStr;

#[derive(Clone, Debug)]
pub enum PersonaSource {
    BuiltinSecurity,
    BuiltinCodeQuality,
    BuiltinPrReview,
    Custom(PathBuf),
}

impl std::fmt::Display for PersonaSource {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::BuiltinSecurity => write!(f, "builtin:security"),
            Self::BuiltinCodeQuality => write!(f, "builtin:code-quality"),
            Self::BuiltinPrReview => write!(f, "builtin:pr-review"),
            Self::Custom(path) => write!(f, "{}", path.display()),
        }
    }
}

impl FromStr for PersonaSource {
    type Err = std::convert::Infallible;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "builtin:security" => Ok(PersonaSource::BuiltinSecurity),
            "builtin:code-quality" => Ok(PersonaSource::BuiltinCodeQuality),
            "builtin:pr-review" | "builtin:general-review" => Ok(PersonaSource::BuiltinPrReview),
            path => Ok(PersonaSource::Custom(PathBuf::from(path))),
        }
    }
}

impl PersonaSource {
    /// Loads the persona content.
    /// Builtins are loaded via `include_str!`, while customs read from the filesystem.
    pub fn load_content(&self) -> anyhow::Result<String> {
        match self {
            PersonaSource::BuiltinSecurity => {
                Ok(include_str!("../personas/security-persona.md").to_string())
            }
            PersonaSource::BuiltinCodeQuality => {
                Ok(include_str!("../personas/code-quality-persona.md").to_string())
            }
            PersonaSource::BuiltinPrReview => {
                Ok(include_str!("../personas/pr-review-persona.md").to_string())
            }
            PersonaSource::Custom(path) => std::fs::read_to_string(path)
                .map_err(|e| anyhow::anyhow!("Failed to read persona file from {:?}: {}", path, e)),
        }
    }

    pub fn review_target(&self) -> &'static str {
        match self {
            PersonaSource::BuiltinSecurity => "security vulnerabilities introduced by this PR",
            PersonaSource::BuiltinCodeQuality => {
                "actionable code quality issues introduced by this PR"
            }
            PersonaSource::BuiltinPrReview | PersonaSource::Custom(_) => {
                "actionable PR issues introduced by this PR"
            }
        }
    }

    pub fn candidate_kind(&self) -> &'static str {
        match self {
            PersonaSource::BuiltinSecurity => "candidate vulnerability",
            PersonaSource::BuiltinCodeQuality | PersonaSource::BuiltinPrReview => "candidate issue",
            PersonaSource::Custom(_) => "candidate finding",
        }
    }

    pub fn methodology_hint(&self) -> &'static str {
        match self {
            PersonaSource::BuiltinSecurity => "follow the CTF methodology in your instructions",
            PersonaSource::BuiltinCodeQuality | PersonaSource::BuiltinPrReview => {
                "follow the review methodology in your instructions"
            }
            PersonaSource::Custom(_) => "follow the methodology in your persona instructions",
        }
    }

    pub fn session_name(&self) -> &'static str {
        match self {
            PersonaSource::BuiltinSecurity => "security-review",
            PersonaSource::BuiltinCodeQuality => "code-quality-review",
            PersonaSource::BuiltinPrReview | PersonaSource::Custom(_) => "pr-review",
        }
    }

    pub fn review_kind(&self) -> String {
        match self {
            PersonaSource::BuiltinSecurity => "security".to_string(),
            PersonaSource::BuiltinCodeQuality => "code-quality".to_string(),
            PersonaSource::BuiltinPrReview => "pr-review".to_string(),
            PersonaSource::Custom(path) => {
                let raw = path.to_string_lossy();
                let slug = slugify(&raw);
                if slug.is_empty() {
                    "custom".to_string()
                } else {
                    format!("custom-{slug}")
                }
            }
        }
    }

    pub fn is_security(&self) -> bool {
        matches!(self, Self::BuiltinSecurity)
    }
}

fn slugify(value: &str) -> String {
    let mut slug = String::new();
    let mut last_was_separator = false;

    for ch in value.chars() {
        if ch.is_ascii_alphanumeric() {
            slug.push(ch.to_ascii_lowercase());
            last_was_separator = false;
        } else if !last_was_separator {
            slug.push('-');
            last_was_separator = true;
        }
    }

    slug.trim_matches('-').to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_pr_review_builtin_and_alias() {
        let persona = PersonaSource::from_str("builtin:pr-review").unwrap();
        assert_eq!(persona.to_string(), "builtin:pr-review");
        assert_eq!(persona.review_kind(), "pr-review");
        assert_eq!(
            persona.review_target(),
            "actionable PR issues introduced by this PR"
        );

        let alias = PersonaSource::from_str("builtin:general-review").unwrap();
        assert_eq!(alias.to_string(), "builtin:pr-review");
    }

    #[test]
    fn custom_persona_review_kind_is_path_based_slug() {
        let persona = PersonaSource::from_str("/tmp/My Persona.md").unwrap();
        assert_eq!(persona.review_kind(), "custom-tmp-my-persona-md");
    }
}

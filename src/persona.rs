use std::path::PathBuf;
use std::str::FromStr;

#[derive(Clone, Debug)]
pub enum PersonaSource {
    BuiltinSecurity,
    BuiltinCodeQuality,
    Custom(PathBuf),
}

impl std::fmt::Display for PersonaSource {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::BuiltinSecurity => write!(f, "builtin:security"),
            Self::BuiltinCodeQuality => write!(f, "builtin:code-quality"),
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
            PersonaSource::Custom(path) => std::fs::read_to_string(path)
                .map_err(|e| anyhow::anyhow!("Failed to read persona file from {:?}: {}", path, e)),
        }
    }
}

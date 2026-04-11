use serde::{Deserialize, Serialize};
use std::collections::HashMap;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Rule {
    pub action: String,
    #[serde(default)]
    pub os: Option<OsRule>,
    #[serde(default)]
    pub features: Option<HashMap<String, bool>>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct OsRule {
    #[serde(default)]
    pub name: String,
    #[serde(default)]
    pub arch: String,
    #[serde(default)]
    pub version: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Environment {
    pub os_name: String,
    pub os_arch: String,
    pub os_version: String,
    pub features: HashMap<String, bool>,
}

pub fn default_environment() -> Environment {
    let mut features = HashMap::new();
    features.insert("is_demo_user".to_string(), false);
    features.insert("has_custom_resolution".to_string(), false);
    features.insert("has_quick_plays_support".to_string(), true);
    features.insert("is_quick_play_singleplayer".to_string(), false);
    features.insert("is_quick_play_multiplayer".to_string(), false);
    features.insert("is_quick_play_realms".to_string(), false);

    Environment {
        os_name: current_os_name().to_string(),
        os_arch: current_os_arch().to_string(),
        os_version: String::new(),
        features,
    }
}

pub fn evaluate_rules(rules: &[Rule], env: &Environment) -> bool {
    if rules.is_empty() {
        return true;
    }

    let mut action = "disallow";
    for rule in rules {
        if rule_matches(rule, env) {
            action = rule.action.as_str();
        }
    }

    action == "allow"
}

pub fn current_os_name() -> &'static str {
    if cfg!(target_os = "windows") {
        "windows"
    } else if cfg!(target_os = "macos") {
        "osx"
    } else {
        "linux"
    }
}

pub fn current_os_arch() -> &'static str {
    if cfg!(target_arch = "x86_64") {
        "x86_64"
    } else if cfg!(target_arch = "x86") {
        "x86"
    } else if cfg!(target_arch = "aarch64") {
        "arm64"
    } else {
        std::env::consts::ARCH
    }
}

pub fn native_classifier_key() -> String {
    let name = if current_os_name() == "osx" {
        "macos"
    } else {
        current_os_name()
    };
    format!("natives-{name}")
}

pub fn is_native_library(name: &str) -> bool {
    name.to_ascii_lowercase().contains("natives-")
}

fn rule_matches(rule: &Rule, env: &Environment) -> bool {
    if let Some(os) = &rule.os {
        if !os.name.is_empty() && os.name != env.os_name {
            return false;
        }
        if !os.arch.is_empty() && os.arch != env.os_arch {
            return false;
        }
        if !os.version.is_empty() && !env.os_version.is_empty() {
            let Ok(regex) = regex::Regex::new(&os.version) else {
                return false;
            };
            if !regex.is_match(&env.os_version) {
                return false;
            }
        }
    }

    if let Some(features) = &rule.features {
        for (feature, required) in features {
            match env.features.get(feature) {
                Some(actual) if actual == required => {}
                Some(_) => return false,
                None if *required => return false,
                None => {}
            }
        }
    }

    true
}

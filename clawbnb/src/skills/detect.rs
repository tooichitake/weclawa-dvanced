//! Read-only inspection of the operator's `~/.claude/` install.
//!
//! Produces the "upper bound" of the skill universe: what the operator has
//! actually installed via `/plugin install`. The global allow-list (in
//! `~/.weclawbot/config.json`) and per-user allow-list cannot exceed this.

use std::fs;
use std::path::PathBuf;

use serde::{Deserialize, Serialize};
use serde_json::Value;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SkillInfo {
    /// Plugin name from `marketplace.json` / `plugin.json` (e.g. "document-skills").
    pub plugin: String,
    /// Marketplace name (e.g. "anthropic-agent-skills").
    pub marketplace: String,
    /// Skill directory name inside `skills/<name>/SKILL.md`.
    pub skill: String,
    /// Frontmatter `description` (truncated for display).
    pub description: String,
    /// Absolute path to SKILL.md on disk.
    pub path: PathBuf,
}

/// Operator's installed plugins, parsed from `~/.claude/settings.json`.
#[derive(Debug, Clone)]
pub struct InstalledPlugin {
    pub plugin: String,
    pub marketplace: String,
    /// Best-effort filesystem location (`plugins/cache/<id>`).
    pub cache_dir: Option<PathBuf>,
}

fn operator_claude_dir() -> Option<PathBuf> {
    dirs::home_dir().map(|h| h.join(".claude"))
}

pub fn read_operator_settings() -> Option<Value> {
    let path = operator_claude_dir()?.join("settings.json");
    let raw = fs::read_to_string(path).ok()?;
    serde_json::from_str(&raw).ok()
}

/// Parse `enabledPlugins` from operator settings. Best-effort: accepts both
/// `["name@marketplace", ...]` and `[{ "plugin": "...", "scope": "..." }]`
/// shapes, since the on-disk schema isn't fully nailed down.
pub fn enabled_plugins() -> Vec<InstalledPlugin> {
    let Some(settings) = read_operator_settings() else {
        return Vec::new();
    };
    let Some(arr) = settings.get("enabledPlugins").and_then(|v| v.as_array()) else {
        return Vec::new();
    };
    let cache_root = operator_claude_dir().map(|d| d.join("plugins").join("cache"));

    arr.iter()
        .filter_map(|entry| {
            let spec = match entry {
                Value::String(s) => s.clone(),
                Value::Object(o) => o
                    .get("plugin")
                    .and_then(|x| x.as_str())
                    .map(|s| s.to_string())?,
                _ => return None,
            };
            let (plugin, marketplace) = parse_spec(&spec);
            let cache_dir = cache_root.as_ref().and_then(|root| {
                find_cache_dir(root, &plugin, &marketplace)
            });
            Some(InstalledPlugin {
                plugin,
                marketplace,
                cache_dir,
            })
        })
        .collect()
}

fn parse_spec(spec: &str) -> (String, String) {
    if let Some((p, m)) = spec.split_once('@') {
        (p.to_string(), m.to_string())
    } else {
        (spec.to_string(), String::new())
    }
}

/// Search `plugins/cache/` for a directory matching this plugin. The exact
/// naming scheme isn't part of any spec we control, so we try a few likely
/// patterns. Returns the first match.
fn find_cache_dir(root: &std::path::Path, plugin: &str, marketplace: &str) -> Option<PathBuf> {
    let candidates: Vec<String> = [
        format!("{plugin}-{marketplace}"),
        format!("{marketplace}-{plugin}"),
        plugin.to_string(),
    ]
    .into_iter()
    .collect();
    for name in &candidates {
        let p = root.join(name);
        if p.is_dir() {
            return Some(p);
        }
    }
    // Fall back to scanning: any directory containing `skills/<plugin>/SKILL.md`
    // is a reasonable match.
    let read = fs::read_dir(root).ok()?;
    for entry in read.flatten() {
        let p = entry.path();
        if p.is_dir() && p.join("skills").is_dir() {
            // Ambiguous, but if the plugin name matches any skill subdir,
            // accept it.
            let skills_dir = p.join("skills");
            if skills_dir.join(plugin).is_dir() {
                return Some(p);
            }
        }
    }
    None
}

/// Enumerate every skill available under the operator's installed plugins.
pub fn discover_skills() -> Vec<SkillInfo> {
    let mut out: Vec<SkillInfo> = Vec::new();
    for plugin in enabled_plugins() {
        let Some(cache) = plugin.cache_dir.clone() else { continue };
        let skills_dir = cache.join("skills");
        let read = match fs::read_dir(&skills_dir) {
            Ok(r) => r,
            Err(_) => continue,
        };
        for entry in read.flatten() {
            let path = entry.path();
            let skill_md = path.join("SKILL.md");
            if !skill_md.is_file() {
                continue;
            }
            let raw = match fs::read_to_string(&skill_md) {
                Ok(s) => s,
                Err(_) => continue,
            };
            let (name, desc) = parse_frontmatter(&raw);
            let display_name = name.unwrap_or_else(|| {
                path.file_name()
                    .and_then(|s| s.to_str())
                    .unwrap_or("?")
                    .to_string()
            });
            out.push(SkillInfo {
                plugin: plugin.plugin.clone(),
                marketplace: plugin.marketplace.clone(),
                skill: display_name,
                description: desc.unwrap_or_default(),
                path: skill_md,
            });
        }
    }
    out
}

/// Parse YAML frontmatter from a SKILL.md. We only need `name` and
/// `description`. The frontmatter block is `--- ... ---` at the very top.
fn parse_frontmatter(raw: &str) -> (Option<String>, Option<String>) {
    let s = raw.trim_start();
    let body = match s.strip_prefix("---") {
        Some(b) => b,
        None => return (None, None),
    };
    let end = match body.find("\n---") {
        Some(i) => i,
        None => return (None, None),
    };
    let block = &body[..end];

    let mut name: Option<String> = None;
    let mut description: Option<String> = None;
    let mut in_description = false;
    let mut desc_buf: Vec<String> = Vec::new();

    for line in block.lines() {
        let trimmed = line.trim_end();
        if let Some(rest) = trimmed.strip_prefix("name:") {
            name = Some(strip_quotes(rest.trim()).to_string());
            in_description = false;
            continue;
        }
        if let Some(rest) = trimmed.strip_prefix("description:") {
            let v = strip_quotes(rest.trim());
            if v.is_empty() {
                in_description = true;
            } else {
                description = Some(v.to_string());
                in_description = false;
            }
            continue;
        }
        if in_description {
            // continuation lines of a multi-line YAML value (folded scalar)
            if trimmed.starts_with("  ") || trimmed.is_empty() {
                desc_buf.push(trimmed.trim().to_string());
            } else {
                in_description = false;
            }
        }
    }
    if description.is_none() && !desc_buf.is_empty() {
        description = Some(desc_buf.join(" ").trim().to_string());
    }
    if let Some(d) = description.as_mut() {
        if d.len() > 320 {
            d.truncate(317);
            d.push_str("...");
        }
    }
    (name, description)
}

fn strip_quotes(s: &str) -> &str {
    let s = s.trim();
    if (s.starts_with('"') && s.ends_with('"') && s.len() >= 2)
        || (s.starts_with('\'') && s.ends_with('\'') && s.len() >= 2)
    {
        &s[1..s.len() - 1]
    } else {
        s
    }
}

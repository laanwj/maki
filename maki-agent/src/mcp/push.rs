//! The brain's config push to the executor, carried in the initialize
//! handshake: everything the executor needs that it must not read from disk
//! itself. The executor's whole configuration arrives this way.

use std::collections::HashMap;
use std::path::Path;

use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

use super::config::{McpConfig, RawTransport};

#[derive(Serialize, Deserialize, Default, Clone)]
pub struct ExecutorPush {
    /// The enabled executor-role builtins as the brain's config names them.
    /// `None` means an older brain: load the default executor set.
    #[serde(default)]
    pub builtin_plugins: Option<Vec<String>>,
    /// `plugins.<name>` option tables for the executor-role builtins.
    #[serde(default)]
    pub plugin_opts: HashMap<String, Map<String, Value>>,
    #[serde(default)]
    pub deny_rules: Vec<DenyRule>,
    /// The brain's folder-trust verdict for the workspace.
    #[serde(default)]
    pub trusted: bool,
    /// stdio MCP servers move to the executor in split mode.
    #[serde(default)]
    pub mcp_servers: HashMap<String, super::config::RawServerConfig>,
    /// Global skills (`~/.config/maki/skills/*/SKILL.md`), name + content.
    #[serde(default)]
    pub skills: Vec<SkillPush>,
    /// User plugins declaring `role = "executor"`: name + plugin.toml + a
    /// single init.lua (pushed plugins must be one file).
    #[serde(default)]
    pub plugin_sources: Vec<PluginSourcePush>,
}

/// A deny rule in wire form: the tool key as a string (the executor rebuilds
/// it through ToolKey::parse, which validates).
#[derive(Serialize, Deserialize, Clone)]
pub struct DenyRule {
    pub tool: String,
    pub scope: Option<String>,
}

#[derive(Serialize, Deserialize, Clone)]
pub struct SkillPush {
    pub name: String,
    pub content: String,
}

#[derive(Serialize, Deserialize, Clone)]
pub struct PluginSourcePush {
    pub name: String,
    pub source: String,
}

pub fn build(config: &maki_config::Config, probe: &McpConfig, trusted: bool) -> ExecutorPush {
    ExecutorPush {
        plugin_opts: config.plugins.opts.clone(),
        deny_rules: config
            .permissions
            .rules
            .iter()
            .filter(|rule| rule.effect == maki_config::Effect::Deny)
            .map(|rule| DenyRule {
                tool: rule.tool.to_string(),
                scope: rule.scope.clone(),
            })
            .collect(),
        builtin_plugins: Some(
            config
                .plugins
                .names
                .iter()
                .filter(|name| !maki_config::is_brain_role_builtin(name))
                .cloned()
                .collect(),
        ),
        trusted,
        mcp_servers: probe
            .mcp
            .iter()
            .filter(|(_, entry)| matches!(entry.transport, RawTransport::Stdio(_)))
            .map(|(name, entry)| (name.clone(), entry.clone()))
            .collect(),
        skills: collect_skills(),
        plugin_sources: collect_plugin_sources(),
    }
}

fn collect_skills() -> Vec<SkillPush> {
    let Ok(config_dir) = maki_storage::paths::config_dir() else {
        return Vec::new();
    };
    let Ok(entries) = std::fs::read_dir(config_dir.join("skills")) else {
        return Vec::new();
    };
    entries
        .filter_map(|entry| {
            let entry = entry.ok()?;
            let content = std::fs::read_to_string(entry.path().join("SKILL.md")).ok()?;
            Some(SkillPush {
                name: entry.file_name().to_string_lossy().into_owned(),
                content,
            })
        })
        .collect()
}

fn collect_plugin_sources() -> Vec<PluginSourcePush> {
    let Ok(config_dir) = maki_storage::paths::config_dir() else {
        return Vec::new();
    };
    collect_plugin_sources_in(&config_dir)
}

/// Executor-role plugins are `autoload/executor/*.lua` files: placement by
/// directory, shipped wholesale.
fn collect_plugin_sources_in(config_dir: &Path) -> Vec<PluginSourcePush> {
    let mut sources = Vec::new();
    let autoload = config_dir.join("autoload").join("executor");
    if let Ok(entries) = std::fs::read_dir(autoload) {
        let mut files: Vec<_> = entries
            .flatten()
            .map(|entry| entry.path())
            .filter(|path| path.extension().and_then(|e| e.to_str()) == Some("lua"))
            .collect();
        files.sort();
        for path in files {
            let Ok(source) = std::fs::read_to_string(&path) else {
                continue;
            };
            let name = path
                .file_stem()
                .unwrap_or_default()
                .to_string_lossy()
                .into_owned();
            sources.push(PluginSourcePush { name, source });
        }
    }
    sources
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn push_roundtrips_through_json() {
        let mut push = ExecutorPush {
            trusted: true,
            ..ExecutorPush::default()
        };
        push.skills.push(SkillPush {
            name: "demo".into(),
            content: "---\nname: demo\n---\nbody".into(),
        });
        push.plugin_sources.push(PluginSourcePush {
            name: "python".into(),
            source: "return {}".into(),
        });
        let value = serde_json::to_value(&push).unwrap();
        let back: ExecutorPush = serde_json::from_value(value).unwrap();
        assert!(back.trusted);
        assert_eq!(back.skills[0].name, "demo");
        assert_eq!(back.plugin_sources[0].name, "python");
    }

    #[test]
    fn build_carries_the_enabled_executor_builtins() {
        let mut raw = maki_config::RawConfig::default();
        raw.plugins.insert(
            "bash".into(),
            maki_config::PluginFileConfig {
                enabled: Some(false),
                ..Default::default()
            },
        );
        let config = raw.into_config(&[]).unwrap();
        let push = build(&config, &McpConfig::default(), false);
        let names = push.builtin_plugins.unwrap();
        assert!(names.iter().any(|n| n == "read"));
        assert!(!names.iter().any(|n| n == "task"));
        assert!(!names.iter().any(|n| n == "bash"));
    }

    #[test]
    fn collects_autoload_executor_wholesale() {
        let dir = tempfile::tempdir().unwrap();
        let autoload = dir.path().join("autoload/executor");
        std::fs::create_dir_all(&autoload).unwrap();
        // No annotations at all: the directory places them.
        std::fs::write(autoload.join("tool_a.lua"), "return {}\n").unwrap();
        std::fs::write(autoload.join("notes.txt"), "not lua\n").unwrap();
        let brain = dir.path().join("autoload/brain");
        std::fs::create_dir_all(&brain).unwrap();
        std::fs::write(brain.join("brainy.lua"), "return {}\n").unwrap();
        // lua/ is a pure module root: nothing in it ships.
        let lua = dir.path().join("lua");
        std::fs::create_dir_all(&lua).unwrap();
        std::fs::write(lua.join("tool_legacy.lua"), "return {}\n").unwrap();

        let sources = collect_plugin_sources_in(dir.path());
        let mut names: Vec<&str> = sources.iter().map(|s| s.name.as_str()).collect();
        names.sort_unstable();
        assert_eq!(names, ["tool_a"]);
    }
}

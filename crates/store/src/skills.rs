//! The xssh skill for agent CLIs (Claude Code, Codex, Gemini CLI, ...): where each one reads
//! user-level skills, and installing / updating / removing `<skills dir>/xssh/SKILL.md`.

use std::path::{Path, PathBuf};
use xssh_core::error::{Error, Result};
use xssh_core::guide::SKILL_NAME;

pub struct Agent {
    /// Stable id for the CLI (`--agent codex`).
    pub id: &'static str,
    pub name: &'static str,
    /// The agent's config folder, relative to the user's home; its presence means "installed".
    root: &'static str,
    /// User-level skills folder, relative to the user's home.
    skills: &'static str,
}

/// User-scope skill folders, as documented by each tool (cross-checked with vercel-labs/skills).
pub const AGENTS: &[Agent] = &[
    Agent {
        id: "claude-code",
        name: "Claude Code",
        root: ".claude",
        skills: ".claude/skills",
    },
    Agent {
        id: "codex",
        name: "Codex",
        root: ".codex",
        skills: ".codex/skills",
    },
    Agent {
        id: "gemini-cli",
        name: "Gemini CLI",
        root: ".gemini",
        skills: ".gemini/skills",
    },
    Agent {
        id: "github-copilot",
        name: "GitHub Copilot",
        root: ".copilot",
        skills: ".copilot/skills",
    },
    Agent {
        id: "cursor",
        name: "Cursor",
        root: ".cursor",
        skills: ".cursor/skills",
    },
    Agent {
        id: "opencode",
        name: "OpenCode",
        root: ".config/opencode",
        skills: ".config/opencode/skills",
    },
    Agent {
        id: "windsurf",
        name: "Windsurf",
        root: ".codeium/windsurf",
        skills: ".codeium/windsurf/skills",
    },
    Agent {
        id: "amp",
        name: "Amp",
        root: ".config/amp",
        skills: ".config/agents/skills",
    },
    Agent {
        id: "qwen-code",
        name: "Qwen Code",
        root: ".qwen",
        skills: ".qwen/skills",
    },
    Agent {
        id: "kiro",
        name: "Kiro CLI",
        root: ".kiro",
        skills: ".kiro/skills",
    },
    Agent {
        id: "trae",
        name: "Trae",
        root: ".trae",
        skills: ".trae/skills",
    },
    Agent {
        id: "roo",
        name: "Roo Code",
        root: ".roo",
        skills: ".roo/skills",
    },
    // The shared folder read by Cline and other tools that follow the agent-skills layout.
    Agent {
        id: "agents",
        name: "Cline / ~/.agents",
        root: ".agents",
        skills: ".agents/skills",
    },
];

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SkillState {
    Missing,
    /// Installed with exactly the content this build would write.
    Current,
    /// Installed, but from another build or for another executable path.
    Outdated,
}

pub fn home_dir() -> Result<PathBuf> {
    std::env::home_dir().ok_or_else(|| Error::io("cannot determine the user's home directory"))
}

pub fn find(id: &str) -> Option<&'static Agent> {
    AGENTS.iter().find(|a| a.id == id)
}

fn join(home: &Path, rel: &str) -> PathBuf {
    rel.split('/').fold(home.to_path_buf(), |p, c| p.join(c))
}

impl Agent {
    pub fn detected(&self, home: &Path) -> bool {
        join(home, self.root).is_dir()
    }

    pub fn skill_dir(&self, home: &Path) -> PathBuf {
        join(home, self.skills).join(SKILL_NAME)
    }

    pub fn skill_file(&self, home: &Path) -> PathBuf {
        self.skill_dir(home).join("SKILL.md")
    }

    pub fn state(&self, home: &Path, content: &str) -> SkillState {
        match std::fs::read_to_string(self.skill_file(home)) {
            Ok(s) if s == content => SkillState::Current,
            Ok(_) => SkillState::Outdated,
            Err(_) => SkillState::Missing,
        }
    }

    pub fn install(&self, home: &Path, content: &str) -> Result<PathBuf> {
        let dir = self.skill_dir(home);
        std::fs::create_dir_all(&dir).map_err(|e| Error::io(format!("create {}: {e}", dir.display())))?;
        let f = dir.join("SKILL.md");
        std::fs::write(&f, content).map_err(|e| Error::io(format!("write {}: {e}", f.display())))?;
        Ok(f)
    }

    /// Remove `SKILL.md`, and the skill folder if nothing else is in it. Returns whether a file
    /// was removed.
    pub fn uninstall(&self, home: &Path) -> Result<bool> {
        let f = self.skill_file(home);
        match std::fs::remove_file(&f) {
            Ok(()) => {
                let _ = std::fs::remove_dir(self.skill_dir(home));
                Ok(true)
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(false),
            Err(e) => Err(Error::io(format!("remove {}: {e}", f.display()))),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn install_update_uninstall() {
        let home = std::env::temp_dir().join(format!("xssh-skills-test-{}", std::process::id()));
        let a = find("codex").unwrap();
        assert!(!a.detected(&home));
        assert_eq!(a.state(&home, "v1"), SkillState::Missing);
        let f = a.install(&home, "v1").unwrap();
        assert!(f.ends_with(Path::new(".codex").join("skills").join("xssh").join("SKILL.md")));
        assert!(a.detected(&home));
        assert_eq!(a.state(&home, "v1"), SkillState::Current);
        assert_eq!(a.state(&home, "v2"), SkillState::Outdated);
        // A file the user added next to SKILL.md survives uninstall.
        std::fs::write(a.skill_dir(&home).join("notes.md"), "mine").unwrap();
        assert!(a.uninstall(&home).unwrap());
        assert!(a.skill_dir(&home).join("notes.md").exists());
        assert!(!a.uninstall(&home).unwrap());
        let _ = std::fs::remove_dir_all(&home);
    }

    #[test]
    fn ids_are_unique() {
        for (i, a) in AGENTS.iter().enumerate() {
            assert!(AGENTS[i + 1..].iter().all(|b| b.id != a.id), "{}", a.id);
        }
    }
}

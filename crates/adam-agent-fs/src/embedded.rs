//! The embedded form of a package: what `build.rs` generates.
//!
//! The types here are `'static` and built from literals, so a generated file can hold a whole
//! agent directory in a `static`: prompts, skill catalogs and resources are read straight from
//! the binary, with no parsing at startup. The two things that are *not* pre-digested are the
//! frontmatter (kept as JSON and read by the same serde schema as the directory path, so there is
//! no second schema) and `mcp.json` (kept as the file's text and read by [`parse_mcp`]).
//!
//! [`EmbeddedPackage`] is a [`ManifestSource`]: `load()` gives the same [`Package`] a [`Dir`] over
//! the same files gives, which is the property the tests rely on. The fields are public because
//! generated code builds the values; nothing else should.

use std::borrow::Cow;
use std::collections::BTreeMap;
use std::path::PathBuf;

use crate::digest::{Digest, digest_with};
use crate::load::parse_mcp;
use crate::manifest::{
    AgentManifest, InstructionPart, Instructions, Layout, Package, RemoteAgent, RemoteAuth, Report,
    Schedule, Skill, SkillLayout, Subagent,
};
use crate::schema::{AgentFrontmatter, McpConfig};
use crate::source::ManifestSource;
use crate::{Diagnostic, Error};

/// A package embedded in the binary: the layout and the agents.
#[derive(Debug, Clone, Copy)]
pub struct EmbeddedPackage {
    /// One agent (`agent/`) or several (`agents/<name>/`); [`Layout::Absent`] when the build was
    /// told the directory is optional and there is none.
    pub layout: Layout,
    /// The agents, sorted by name.
    pub agents: &'static [EmbeddedAgent],
}

impl EmbeddedPackage {
    /// The owned [`Package`] these agents make.
    ///
    /// # Errors
    ///
    /// [`Error::Codec`] or [`Error::Invalid`] when the embedded text no longer reads with this
    /// crate's schema, which means the generated code came from another version of it.
    pub fn to_package(self) -> Result<Package, Error> {
        let agents = self
            .agents
            .iter()
            .copied()
            .map(EmbeddedAgent::to_manifest)
            .collect::<Result<Vec<_>, _>>()?;
        Ok(Package {
            layout: self.layout,
            agents,
        })
    }
}

impl ManifestSource for EmbeddedPackage {
    /// The embedded agents and no diagnostics: the build refused anything that had an error.
    fn load(&self) -> Result<Report, Error> {
        Ok(Report {
            package: self.to_package()?,
            diagnostics: Vec::new(),
        })
    }

    fn read_resource(&self, skill: &Skill, name: &str) -> Result<Cow<'static, [u8]>, Error> {
        let path = skill.path.to_string_lossy().replace('\\', "/");
        self.agents
            .iter()
            .find_map(|agent| agent.resource(&path, name))
            .map(Cow::Borrowed)
            .ok_or_else(|| {
                Error::io(
                    format!("{path}/{name}"),
                    std::io::Error::new(std::io::ErrorKind::NotFound, "not embedded"),
                )
            })
    }
}

/// One agent with everything that belongs to it.
#[derive(Debug, Clone, Copy)]
pub struct EmbeddedAgent {
    /// The agent's name.
    pub name: &'static str,
    /// The [`Digest`] of the normalised manifest at build time (`sha256:...`).
    pub digest: &'static str,
    /// The instructions file (or subagent file), relative to the source root.
    pub path: &'static str,
    /// The frontmatter as JSON, read back into an [`AgentFrontmatter`] by
    /// [`frontmatter`](Self::frontmatter).
    pub frontmatter_json: &'static str,
    /// The prompt.
    pub instructions: EmbeddedInstructions,
    /// The agent's skills, sorted by name.
    pub skills: &'static [EmbeddedSkill],
    /// The agent's subagents, sorted by name.
    pub subagents: &'static [EmbeddedSubagent],
    /// The agent's `mcp.json`, if it has one.
    pub mcp: Option<EmbeddedMcp>,
    /// The agent's schedules, sorted by name.
    pub schedules: &'static [EmbeddedSchedule],
}

impl EmbeddedAgent {
    /// The frontmatter, read from its embedded JSON.
    ///
    /// # Errors
    ///
    /// [`Error::Codec`] when the JSON does not read as an [`AgentFrontmatter`].
    pub fn frontmatter(&self) -> Result<AgentFrontmatter, Error> {
        serde_json::from_str(self.frontmatter_json)
            .map_err(|e| Error::codec("decode", format!("{} frontmatter", self.name), e))
    }

    /// The owned [`AgentManifest`], the type a [`Dir`](crate::Dir) produces.
    ///
    /// # Errors
    ///
    /// See [`EmbeddedPackage::to_package`].
    pub fn to_manifest(self) -> Result<AgentManifest, Error> {
        let mcp = match &self.mcp {
            Some(mcp) => Some(mcp.to_config()?),
            None => None,
        };
        Ok(AgentManifest {
            name: self.name.to_owned(),
            path: PathBuf::from(self.path),
            frontmatter: self.frontmatter()?,
            instructions: Instructions {
                body: self.instructions.body.to_owned(),
                parts: self
                    .instructions
                    .parts
                    .iter()
                    .map(|p| InstructionPart {
                        file: p.file.to_owned(),
                        body: p.body.to_owned(),
                    })
                    .collect(),
            },
            skills: self
                .skills
                .iter()
                .copied()
                .map(EmbeddedSkill::to_skill)
                .collect(),
            subagents: self
                .subagents
                .iter()
                .copied()
                .map(EmbeddedSubagent::to_subagent)
                .collect::<Result<_, _>>()?,
            mcp,
            schedules: self
                .schedules
                .iter()
                .copied()
                .map(EmbeddedSchedule::to_schedule)
                .collect(),
        })
    }

    /// The digest, computed again from the embedded content.
    ///
    /// # Errors
    ///
    /// See [`to_manifest`](Self::to_manifest).
    pub fn recompute_digest(&self) -> Result<Digest, Error> {
        let manifest = self.to_manifest()?;
        digest_with(&manifest, |skill, name| {
            self.find_resource(skill, name).map(Cow::Borrowed)
        })
    }

    /// Whether the embedded content still has the digest recorded when it was generated.
    /// Nothing but a mismatched generator or a hand edit of the generated file makes it `false`.
    ///
    /// # Errors
    ///
    /// See [`to_manifest`](Self::to_manifest).
    pub fn verify(&self) -> Result<bool, Error> {
        Ok(self.recompute_digest()? == self.digest)
    }

    /// The bytes of a resource of one of this agent's skills (at any depth of subagents).
    fn find_resource(&self, skill: &Skill, name: &str) -> Result<&'static [u8], Error> {
        self.resource(&skill.path.to_string_lossy().replace('\\', "/"), name)
            .ok_or_else(|| {
                Error::io(
                    format!("{}/{name}", skill.path.display()),
                    std::io::Error::new(std::io::ErrorKind::NotFound, "not embedded"),
                )
            })
    }

    /// The bytes of a resource of a skill of this agent or of one of its subagents, found by the
    /// skill's file (`agent/skills/release-notes/SKILL.md`) and the resource's path
    /// (`references/style.md`).
    pub fn resource(&self, skill_path: &str, name: &str) -> Option<&'static [u8]> {
        let own = self
            .skills
            .iter()
            .filter(|s| s.path == skill_path)
            .flat_map(|s| s.files.iter())
            .find(|f| f.path == name)
            .map(|f| f.bytes);
        own.or_else(|| {
            self.subagents.iter().find_map(|sub| match sub {
                EmbeddedSubagent::Local(local) => local.resource(skill_path, name),
                EmbeddedSubagent::Remote(_) => None,
            })
        })
    }
}

/// The system prompt of an agent, in the pieces it is written in.
#[derive(Debug, Clone, Copy)]
pub struct EmbeddedInstructions {
    /// The body of the instructions file: LF line endings, trimmed.
    pub body: &'static str,
    /// `instructions/*.md` in filename order.
    pub parts: &'static [EmbeddedPart],
}

/// One extra file of `instructions/`.
#[derive(Debug, Clone, Copy)]
pub struct EmbeddedPart {
    /// The file name.
    pub file: &'static str,
    /// Its content: LF line endings, trimmed.
    pub body: &'static str,
}

/// A skill: a catalog entry, a body loaded on demand and its resources.
#[derive(Debug, Clone, Copy)]
pub struct EmbeddedSkill {
    /// The name the catalog uses.
    pub name: &'static str,
    /// What the skill does and when to use it.
    pub description: &'static str,
    /// The `license` field.
    pub license: Option<&'static str>,
    /// The `compatibility` field.
    pub compatibility: Option<&'static str>,
    /// The `metadata` map, sorted by key.
    pub metadata: &'static [(&'static str, &'static str)],
    /// The `allowed-tools` entries.
    pub allowed_tools: &'static [&'static str],
    /// The Markdown after the frontmatter: LF line endings, trimmed.
    pub body: &'static str,
    /// Directory or flat.
    pub layout: SkillLayout,
    /// The `SKILL.md` (or flat) file, relative to the source root.
    pub path: &'static str,
    /// The other files of a directory skill, sorted by path, each `include_bytes!`d.
    pub files: &'static [EmbeddedFile],
}

impl EmbeddedSkill {
    fn to_skill(self) -> Skill {
        Skill {
            name: self.name.to_owned(),
            description: self.description.to_owned(),
            license: self.license.map(str::to_owned),
            compatibility: self.compatibility.map(str::to_owned),
            metadata: self
                .metadata
                .iter()
                .map(|(k, v)| ((*k).to_owned(), (*v).to_owned()))
                .collect::<BTreeMap<_, _>>(),
            allowed_tools: self.allowed_tools.iter().map(|s| (*s).to_owned()).collect(),
            body: self.body.to_owned(),
            layout: self.layout,
            path: PathBuf::from(self.path),
            resources: self.files.iter().map(|f| f.path.to_owned()).collect(),
        }
    }
}

/// One resource file of a skill.
#[derive(Debug, Clone, Copy)]
pub struct EmbeddedFile {
    /// Relative to the skill directory, with `/` separators.
    pub path: &'static str,
    /// The content.
    pub bytes: &'static [u8],
}

/// A subagent: a local agent of its own, or a remote A2A agent.
#[derive(Debug, Clone, Copy)]
pub enum EmbeddedSubagent {
    /// Hosted in this process.
    Local(EmbeddedAgent),
    /// An A2A agent (`a2a:` in the frontmatter).
    Remote(EmbeddedRemote),
}

impl EmbeddedSubagent {
    /// The subagent's name.
    pub fn name(&self) -> &'static str {
        match self {
            Self::Local(a) => a.name,
            Self::Remote(r) => r.name,
        }
    }

    fn to_subagent(self) -> Result<Subagent, Error> {
        Ok(match self {
            Self::Local(a) => Subagent::Local(Box::new(a.to_manifest()?)),
            Self::Remote(r) => Subagent::Remote(r.to_remote()),
        })
    }
}

/// How to authenticate to a remote subagent.
#[derive(Debug, Clone, Copy)]
pub enum EmbeddedAuth {
    /// `bearer:VAR`: the token is in the environment variable `VAR`.
    Bearer {
        /// The variable's name (never its value).
        env: &'static str,
    },
}

/// A remote subagent.
#[derive(Debug, Clone, Copy)]
pub struct EmbeddedRemote {
    /// The tool name the parent sees.
    pub name: &'static str,
    /// The tool description the parent reads.
    pub description: &'static str,
    /// The agent-card URL.
    pub url: &'static str,
    /// Credentials, if the agent needs them.
    pub auth: Option<EmbeddedAuth>,
    /// The body of the file, which extends the tool description.
    pub note: &'static str,
    /// The file, relative to the source root.
    pub path: &'static str,
}

impl EmbeddedRemote {
    fn to_remote(self) -> RemoteAgent {
        RemoteAgent {
            name: self.name.to_owned(),
            description: self.description.to_owned(),
            url: self.url.to_owned(),
            auth: self
                .auth
                .map(|EmbeddedAuth::Bearer { env }| RemoteAuth::Bearer {
                    env: env.to_owned(),
                }),
            note: self.note.to_owned(),
            path: PathBuf::from(self.path),
        }
    }
}

/// An agent's `mcp.json`, as the text of the file. `${VAR}` references are unexpanded.
#[derive(Debug, Clone, Copy)]
pub struct EmbeddedMcp {
    /// The file, relative to the source root.
    pub path: &'static str,
    /// The file's content.
    pub json: &'static str,
}

impl EmbeddedMcp {
    /// The parsed configuration.
    ///
    /// # Errors
    ///
    /// [`Error::Invalid`] when the text has an error (it had none when it was embedded).
    pub fn to_config(self) -> Result<McpConfig, Error> {
        let mut diagnostics: Vec<Diagnostic> = Vec::new();
        let config = parse_mcp(std::path::Path::new(self.path), self.json, &mut diagnostics);
        match config {
            Some(config) if !diagnostics.iter().any(Diagnostic::is_error) => Ok(config),
            _ => Err(Error::Invalid { diagnostics }),
        }
    }
}

/// A scheduled prompt.
#[derive(Debug, Clone, Copy)]
pub struct EmbeddedSchedule {
    /// From the path: `schedules/a/b.md` is `a/b`.
    pub name: &'static str,
    /// A five-field cron expression.
    pub cron: &'static str,
    /// An IANA time zone name.
    pub timezone: &'static str,
    /// The agent it runs.
    pub agent: &'static str,
    /// The body of the file.
    pub prompt: &'static str,
    /// The file, relative to the source root.
    pub path: &'static str,
}

impl EmbeddedSchedule {
    fn to_schedule(self) -> Schedule {
        Schedule {
            name: self.name.to_owned(),
            cron: self.cron.to_owned(),
            timezone: self.timezone.to_owned(),
            agent: self.agent.to_owned(),
            prompt: self.prompt.to_owned(),
            path: PathBuf::from(self.path),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Severity;

    const BARE: EmbeddedAgent = EmbeddedAgent {
        name: "a",
        digest: "sha256:none",
        path: "agent/instructions.md",
        frontmatter_json: "{}",
        instructions: EmbeddedInstructions {
            body: "You help.",
            parts: &[],
        },
        skills: &[],
        subagents: &[],
        mcp: None,
        schedules: &[],
    };

    static WITH_SKILL: EmbeddedAgent = EmbeddedAgent {
        skills: &[EmbeddedSkill {
            name: "s",
            description: "d",
            license: Some("MIT"),
            compatibility: None,
            metadata: &[("k", "v")],
            allowed_tools: &["Read"],
            body: "b",
            layout: SkillLayout::Directory,
            path: "agent/skills/s/SKILL.md",
            files: &[EmbeddedFile {
                path: "scripts/run.sh",
                bytes: b"echo hi",
            }],
        }],
        subagents: &[EmbeddedSubagent::Local(EmbeddedAgent {
            name: "kid",
            skills: &[EmbeddedSkill {
                name: "k",
                description: "d",
                license: None,
                compatibility: None,
                metadata: &[],
                allowed_tools: &[],
                body: "b",
                layout: SkillLayout::Directory,
                path: "agent/subagents/kid/skills/k/SKILL.md",
                files: &[EmbeddedFile {
                    path: "assets/x.bin",
                    bytes: &[0, 1, 2],
                }],
            }],
            ..BARE
        })],
        ..BARE
    };

    #[test]
    fn an_embedded_agent_becomes_the_owned_manifest() {
        let m = WITH_SKILL.to_manifest().unwrap();
        assert_eq!(m.name, "a");
        assert_eq!(m.skills[0].resources, ["scripts/run.sh"]);
        assert_eq!(m.skills[0].metadata["k"], "v");
        assert_eq!(m.skills[0].license.as_deref(), Some("MIT"));
        assert_eq!(m.subagents[0].name(), "kid");
        assert_eq!(WITH_SKILL.subagents[0].name(), "kid");
    }

    #[test]
    fn the_digest_is_recomputed_over_resources_at_every_depth() {
        let d = WITH_SKILL.recompute_digest().unwrap();
        assert!(d.as_str().starts_with("sha256:"));
        // The recorded digest here is a placeholder, so it does not verify.
        assert!(!WITH_SKILL.verify().unwrap());
        let recorded: &'static str = Box::leak(d.to_string().into_boxed_str());
        let honest = EmbeddedAgent {
            digest: recorded,
            ..WITH_SKILL
        };
        assert!(honest.verify().unwrap());
    }

    #[test]
    fn a_package_is_a_manifest_source() {
        static AGENTS: [EmbeddedAgent; 1] = [BARE];
        let package = EmbeddedPackage {
            layout: Layout::Single,
            agents: &AGENTS,
        };
        let report = package.load().unwrap();
        assert!(report.diagnostics.is_empty());
        assert_eq!(report.package.layout, Layout::Single);
        assert_eq!(report.package.agents[0].name, "a");
    }

    #[test]
    fn a_package_serves_the_resources_of_any_depth_and_nothing_else() {
        static AGENTS: [EmbeddedAgent; 1] = [WITH_SKILL];
        let package = EmbeddedPackage {
            layout: Layout::Single,
            agents: &AGENTS,
        };
        let manifest = package.load().unwrap().package.agents.remove(0);
        let own = &manifest.skills[0];
        assert_eq!(
            &*package.read_resource(own, "scripts/run.sh").unwrap(),
            b"echo hi"
        );
        let Subagent::Local(kid) = &manifest.subagents[0] else {
            panic!("a local subagent");
        };
        assert_eq!(
            &*package
                .read_resource(&kid.skills[0], "assets/x.bin")
                .unwrap(),
            &[0, 1, 2]
        );
        // Not a resource of that skill, even if another skill has it.
        let err = package.read_resource(own, "assets/x.bin").unwrap_err();
        assert!(matches!(err, Error::Io { .. }), "{err}");
        assert!(
            err.to_string()
                .contains("agent/skills/s/SKILL.md/assets/x.bin"),
            "{err}"
        );
    }

    #[test]
    fn frontmatter_that_does_not_read_is_a_codec_error() {
        let broken = EmbeddedAgent {
            frontmatter_json: "{\"tools\": 3",
            ..BARE
        };
        let err = broken.to_manifest().unwrap_err();
        assert!(
            matches!(
                err,
                Error::Codec {
                    action: "decode",
                    ..
                }
            ),
            "{err}"
        );
        assert_eq!(
            err.to_string(),
            "cannot decode the manifest of `a frontmatter`"
        );
        assert!(std::error::Error::source(&err).is_some());
        assert!(broken.recompute_digest().is_err());
    }

    #[test]
    fn mcp_text_is_parsed_and_an_error_in_it_is_reported() {
        let ok = EmbeddedMcp {
            path: "agent/mcp.json",
            json: r#"{"mcpServers":{"fs":{"command":"mcp-fs"}}}"#,
        };
        assert_eq!(ok.to_config().unwrap().servers.len(), 1);
        let bad = EmbeddedMcp {
            path: "agent/mcp.json",
            json: "{",
        };
        let Err(Error::Invalid { diagnostics }) = bad.to_config() else {
            panic!("expected Invalid");
        };
        assert_eq!(diagnostics[0].severity, Severity::Error);
        assert_eq!(diagnostics[0].path, PathBuf::from("agent/mcp.json"));
    }

    #[test]
    fn remote_subagents_and_schedules_convert() {
        static AGENTS: [EmbeddedAgent; 1] = [EmbeddedAgent {
            subagents: &[EmbeddedSubagent::Remote(EmbeddedRemote {
                name: "billing",
                description: "Bills.",
                url: "https://billing.example.com/card.json",
                auth: Some(EmbeddedAuth::Bearer { env: "TOKEN" }),
                note: "",
                path: "agent/subagents/billing.md",
            })],
            schedules: &[EmbeddedSchedule {
                name: "daily",
                cron: "0 9 * * *",
                timezone: "UTC",
                agent: "a",
                prompt: "Digest.",
                path: "agent/schedules/daily.md",
            }],
            ..BARE
        }];
        let package = EmbeddedPackage {
            layout: Layout::Single,
            agents: &AGENTS,
        }
        .to_package()
        .unwrap();
        let agent = &package.agents[0];
        let Subagent::Remote(r) = &agent.subagents[0] else {
            panic!("expected a remote subagent");
        };
        assert_eq!(
            r.auth,
            Some(RemoteAuth::Bearer {
                env: "TOKEN".into()
            })
        );
        assert_eq!(agent.schedules[0].name, "daily");
    }
}

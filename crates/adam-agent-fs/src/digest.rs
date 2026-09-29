//! The digest of a normalised manifest: what identifies "these exact agent files".
//!
//! The digest covers the JSON of the [`AgentManifest`] (every field, in declaration order, with
//! `/` in paths) and the bytes of every skill resource, so a changed prompt, frontmatter key or
//! script changes it, and the way the files reached the process (read from a directory, embedded
//! by `build.rs`) does not. It is meant for observability (store it with a run) and for tests
//! that compare an embedded manifest with the directory it came from.

use std::borrow::Cow;
use std::fmt::{self, Write as _};
use std::fs;

use sha2::{Digest as _, Sha256};

use crate::Error;
use crate::manifest::{AgentManifest, Skill, Subagent};
use crate::source::Dir;

/// The prefix that names the algorithm and the recipe: bump the number when the recipe changes.
const DOMAIN: &[u8] = b"adam-agent-manifest/1\n";

/// `sha256:` and 64 lower-case hexadecimal digits.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct Digest(String);

impl Digest {
    /// The text form, `sha256:...`.
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for Digest {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl PartialEq<str> for Digest {
    fn eq(&self, other: &str) -> bool {
        self.0 == other
    }
}

impl PartialEq<&str> for Digest {
    fn eq(&self, other: &&str) -> bool {
        self.0 == *other
    }
}

/// The digest of `agent`, with the bytes of each skill resource supplied by `resource` (given the
/// skill and the resource path from [`Skill::resources`]).
///
/// Every source of the same files gives the same digest: that is what makes an embedded manifest
/// comparable with the directory it came from.
///
/// # Errors
///
/// [`Error::Codec`] when the manifest cannot be encoded (it cannot, for these types), or the
/// error `resource` returns.
pub fn digest_with<'a, F>(agent: &AgentManifest, mut resource: F) -> Result<Digest, Error>
where
    F: FnMut(&Skill, &str) -> Result<Cow<'a, [u8]>, Error>,
{
    let json =
        serde_json::to_vec(agent).map_err(|e| Error::codec("encode", agent.name.clone(), e))?;
    let mut hasher = Sha256::new();
    hasher.update(DOMAIN);
    feed(&mut hasher, &json);
    feed_resources(&mut hasher, agent, &mut resource)?;
    let mut text = String::from("sha256:");
    for byte in hasher.finalize() {
        // Writing to a `String` cannot fail.
        let _ = write!(text, "{byte:02x}");
    }
    Ok(Digest(text))
}

/// A length, then the bytes: so `("ab", "c")` and `("a", "bc")` differ.
fn feed(hasher: &mut Sha256, bytes: &[u8]) {
    hasher.update((bytes.len() as u64).to_le_bytes());
    hasher.update(bytes);
}

fn feed_resources<'a, F>(
    hasher: &mut Sha256,
    agent: &AgentManifest,
    resource: &mut F,
) -> Result<(), Error>
where
    F: FnMut(&Skill, &str) -> Result<Cow<'a, [u8]>, Error>,
{
    for skill in &agent.skills {
        for name in &skill.resources {
            let bytes = resource(skill, name)?;
            feed(
                hasher,
                skill.path.to_string_lossy().replace('\\', "/").as_bytes(),
            );
            feed(hasher, name.as_bytes());
            feed(hasher, &bytes);
        }
    }
    for sub in &agent.subagents {
        if let Subagent::Local(local) = sub {
            feed_resources(hasher, local, resource)?;
        }
    }
    Ok(())
}

impl Dir {
    /// The [`Digest`] of an agent this directory produced, reading its skill resources from disk.
    ///
    /// # Errors
    ///
    /// [`Error::Io`] when a resource cannot be read.
    pub fn digest(&self, agent: &AgentManifest) -> Result<Digest, Error> {
        digest_with(agent, |skill, name| {
            let dir = skill.path.parent().unwrap_or(std::path::Path::new(""));
            let path = self.root().join(dir).join(name);
            fs::read(&path)
                .map(Cow::Owned)
                .map_err(|e| Error::io(path, e))
        })
    }
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use super::*;
    use crate::manifest::{Instructions, SkillLayout};
    use crate::schema::AgentFrontmatter;

    fn agent(body: &str) -> AgentManifest {
        AgentManifest {
            name: "a".into(),
            path: PathBuf::from("agent/instructions.md"),
            frontmatter: AgentFrontmatter::default(),
            instructions: Instructions {
                body: body.into(),
                parts: Vec::new(),
            },
            skills: Vec::new(),
            subagents: Vec::new(),
            mcp: None,
            schedules: Vec::new(),
        }
    }

    fn with_skill(mut a: AgentManifest, resources: &[&str]) -> AgentManifest {
        a.skills.push(Skill {
            name: "s".into(),
            description: "d".into(),
            license: None,
            compatibility: None,
            metadata: Default::default(),
            allowed_tools: Vec::new(),
            body: "b".into(),
            layout: SkillLayout::Directory,
            path: PathBuf::from("agent/skills/s/SKILL.md"),
            resources: resources.iter().map(|r| (*r).to_owned()).collect(),
        });
        a
    }

    fn no_resources(_: &Skill, _: &str) -> Result<Cow<'static, [u8]>, Error> {
        Ok(Cow::Borrowed(b""))
    }

    #[test]
    fn a_digest_is_sha256_hex_and_stable() {
        let d = digest_with(&agent("hi"), no_resources).unwrap();
        assert!(d.as_str().starts_with("sha256:"));
        assert_eq!(d.as_str().len(), "sha256:".len() + 64);
        assert!(
            d.as_str()["sha256:".len()..]
                .bytes()
                .all(|b| b.is_ascii_hexdigit())
        );
        assert_eq!(d, digest_with(&agent("hi"), no_resources).unwrap());
        assert_eq!(d.to_string(), d.as_str());
        assert!(d == d.as_str());
        assert!(d == *d.as_str());
    }

    #[test]
    fn any_change_of_the_manifest_changes_the_digest() {
        let base = digest_with(&agent("hi"), no_resources).unwrap();
        assert_ne!(base, digest_with(&agent("hello"), no_resources).unwrap());
        let mut renamed = agent("hi");
        renamed.name = "b".into();
        assert_ne!(base, digest_with(&renamed, no_resources).unwrap());
    }

    #[test]
    fn resources_count_by_path_and_by_content() {
        let a = with_skill(agent("hi"), &["scripts/run.sh"]);
        let one = digest_with(&a, |_, _| Ok(Cow::Borrowed(&b"echo 1"[..]))).unwrap();
        let two = digest_with(&a, |_, _| Ok(Cow::Borrowed(&b"echo 2"[..]))).unwrap();
        assert_ne!(one, two);
        let renamed = with_skill(agent("hi"), &["scripts/other.sh"]);
        let three = digest_with(&renamed, |_, _| Ok(Cow::Borrowed(&b"echo 1"[..]))).unwrap();
        assert_ne!(one, three);
    }

    #[test]
    fn neighbouring_fields_do_not_run_together() {
        // ("a", "bc") and ("ab", "c") would hash alike without the length prefixes.
        let ab = with_skill(agent("hi"), &["a", "bc"]);
        let ba = with_skill(agent("hi"), &["ab", "c"]);
        let same = |_: &Skill, _: &str| Ok(Cow::Borrowed(&b"x"[..]));
        assert_ne!(
            digest_with(&ab, same).unwrap(),
            digest_with(&ba, same).unwrap()
        );
    }

    #[test]
    fn a_resource_that_cannot_be_read_fails_the_digest() {
        let a = with_skill(agent("hi"), &["x"]);
        let err = digest_with(&a, |_, _| -> Result<Cow<'static, [u8]>, Error> {
            Err(Error::io("x", std::io::Error::other("gone")))
        })
        .unwrap_err();
        assert!(matches!(err, Error::Io { .. }));
    }

    #[test]
    fn a_dir_reads_the_resources_from_disk() {
        let root = tempfile::tempdir().unwrap();
        let skill_dir = root.path().join("agent/skills/s/scripts");
        std::fs::create_dir_all(&skill_dir).unwrap();
        std::fs::write(skill_dir.join("run.sh"), "echo hi").unwrap();
        let a = with_skill(agent("hi"), &["scripts/run.sh"]);
        let dir = Dir::new(root.path());
        let on_disk = dir.digest(&a).unwrap();
        let by_hand = digest_with(&a, |_, _| Ok(Cow::Borrowed(&b"echo hi"[..]))).unwrap();
        assert_eq!(on_disk, by_hand);
        std::fs::remove_file(skill_dir.join("run.sh")).unwrap();
        assert!(matches!(dir.digest(&a), Err(Error::Io { .. })));
    }
}

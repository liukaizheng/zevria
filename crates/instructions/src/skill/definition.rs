//! Validated snapshot content and runtime-only discovery locations.

use schemars::JsonSchema;
use serde::{Deserialize, Deserializer, Serialize, Serializer, de::Error as _};
use sha2::{Digest as _, Sha256};
use std::{
    borrow::Borrow,
    fmt,
    path::{Path, PathBuf},
    str::FromStr,
};

use super::MAX_SKILL_BYTES;

const MAX_NAME_CHARS: usize = 64;
pub const MAX_SKILL_METADATA_BYTES: usize = 128 * 1024;
const SNAPSHOT_DOMAIN: &[u8] = b"zevria.skill.snapshot.v1";

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, JsonSchema)]
#[schemars(transparent)]
pub struct SkillName(String);
impl SkillName {
    pub fn parse(value: impl AsRef<str>) -> Result<Self, SkillNameError> {
        value.as_ref().parse()
    }
    pub fn as_str(&self) -> &str {
        &self.0
    }
}
impl FromStr for SkillName {
    type Err = SkillNameError;
    fn from_str(value: &str) -> Result<Self, Self::Err> {
        if value.is_empty() || value.chars().count() > MAX_NAME_CHARS {
            return Err(SkillNameError(format!(
                "the skill name {:?} must be 1-{MAX_NAME_CHARS} characters",
                value.chars().take(MAX_NAME_CHARS + 1).collect::<String>()
            )));
        }
        if !value
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || matches!(c, '-' | '_'))
        {
            return Err(SkillNameError(format!(
                "the skill name {value:?} may only contain lowercase ASCII letters, digits, `-`, and `_`"
            )));
        }
        Ok(Self(value.into()))
    }
}
impl fmt::Display for SkillName {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}
impl AsRef<str> for SkillName {
    fn as_ref(&self) -> &str {
        self.as_str()
    }
}
impl Borrow<str> for SkillName {
    fn borrow(&self) -> &str {
        self.as_str()
    }
}
impl Serialize for SkillName {
    fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        s.serialize_str(self.as_str())
    }
}
impl<'de> Deserialize<'de> for SkillName {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        String::deserialize(d)?.parse().map_err(D::Error::custom)
    }
}
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SkillNameError(String);
impl fmt::Display for SkillNameError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}
impl std::error::Error for SkillNameError {}

/// SHA-256 identity of the complete canonical snapshot, including metadata and provenance.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct SkillDigest(pub(super) [u8; 32]);
impl SkillDigest {
    pub fn as_bytes(&self) -> &[u8; 32] {
        &self.0
    }
    pub fn to_hex(self) -> String {
        use fmt::Write as _;
        let mut output = String::with_capacity(64);
        for byte in self.0 {
            let _ = write!(output, "{byte:02x}");
        }
        output
    }
    fn for_snapshot(
        name: &SkillName,
        body: &str,
        metadata: &SkillMetadata,
        provenance: Option<&SkillProvenance>,
    ) -> anyhow::Result<Self> {
        let mut hasher = Sha256::new();
        hasher.update(SNAPSHOT_DOMAIN);
        // Typed structs, not unordered maps, define deterministic field order.
        update_length_delimited(&mut hasher, name.as_str().as_bytes());
        update_length_delimited(&mut hasher, body.as_bytes());
        update_length_delimited(&mut hasher, &serde_json::to_vec(metadata)?);
        update_length_delimited(&mut hasher, &serde_json::to_vec(&provenance)?);
        Ok(Self(hasher.finalize().into()))
    }
}
pub(crate) fn update_length_delimited(hasher: &mut Sha256, value: &[u8]) {
    hasher.update((value.len() as u64).to_be_bytes());
    hasher.update(value);
}
impl fmt::Display for SkillDigest {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.to_hex())
    }
}
impl FromStr for SkillDigest {
    type Err = SkillDigestError;
    fn from_str(value: &str) -> Result<Self, Self::Err> {
        if value.len() != 64
            || !value
                .bytes()
                .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
        {
            return Err(SkillDigestError(
                "a skill digest must contain exactly 64 lowercase hexadecimal characters".into(),
            ));
        }
        let mut bytes = [0; 32];
        for (i, pair) in value.as_bytes().as_chunks::<2>().0.iter().enumerate() {
            bytes[i] = u8::from_str_radix(std::str::from_utf8(pair).expect("hex is UTF-8"), 16)
                .map_err(|_| SkillDigestError("invalid hexadecimal skill digest".into()))?;
        }
        Ok(Self(bytes))
    }
}
impl Serialize for SkillDigest {
    fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        s.serialize_str(&self.to_hex())
    }
}
impl<'de> Deserialize<'de> for SkillDigest {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        String::deserialize(d)?.parse().map_err(D::Error::custom)
    }
}
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SkillDigestError(String);
impl fmt::Display for SkillDigestError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}
impl std::error::Error for SkillDigestError {}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FixedSkillScope {
    Global,
    Project,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SkillLayout {
    Flat,
    Package,
}

/// Private containment binding, never a public source selector or path authority.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(transparent)]
pub(crate) struct SourceBinding(SkillDigest);
impl SourceBinding {
    pub(crate) fn for_source(
        scope: FixedSkillScope,
        root: &Path,
        manifest: &SkillRelativePath,
    ) -> anyhow::Result<Self> {
        Self::for_opened_source(
            scope,
            root,
            manifest,
            &zevria_foundation::contained_read::OpenedRoot::open(root)?,
        )
    }
    pub(crate) fn for_opened_source(
        scope: FixedSkillScope,
        root: &Path,
        manifest: &SkillRelativePath,
        opened: &zevria_foundation::contained_read::OpenedRoot,
    ) -> anyhow::Result<Self> {
        let identity = opened.identity()?;
        let mut hasher = Sha256::new();
        hasher.update(b"zevria.skill.source-binding.v2");
        update_length_delimited(&mut hasher, identity.as_bytes());
        hasher.update([match scope {
            FixedSkillScope::Global => 0,
            FixedSkillScope::Project => 1,
        }]);
        update_length_delimited(&mut hasher, root.as_os_str().as_encoded_bytes());
        update_length_delimited(&mut hasher, manifest.as_str().as_bytes());
        Ok(Self(SkillDigest(hasher.finalize().into())))
    }
}

/// Portable relative locator; rejects platform-specific escaping syntax everywhere.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Serialize)]
#[serde(transparent)]
pub struct SkillRelativePath(String);
impl SkillRelativePath {
    pub fn new(value: impl Into<String>) -> anyhow::Result<Self> {
        let value = value.into();
        anyhow::ensure!(
            !value.is_empty() && value.len() <= 1024,
            "a skill locator must be 1-1024 bytes"
        );
        anyhow::ensure!(
            !value
                .chars()
                .any(|c| c.is_control() || matches!(c, '\\' | ':')),
            "a skill locator cannot contain controls or platform prefixes"
        );
        anyhow::ensure!(
            value
                .split('/')
                .all(|part| !matches!(part, "" | "." | "..")),
            "a skill locator must be relative without empty, dot, or parent components"
        );
        Ok(Self(value))
    }
    pub fn as_str(&self) -> &str {
        &self.0
    }
    pub fn to_path(&self) -> PathBuf {
        self.0.split('/').collect()
    }
}
impl<'de> Deserialize<'de> for SkillRelativePath {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        Self::new(String::deserialize(d)?).map_err(D::Error::custom)
    }
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SkillProvenance {
    pub scope: FixedSkillScope,
    pub layout: SkillLayout,
    pub manifest: SkillRelativePath,
    pub(crate) source_binding: SourceBinding,
}
impl SkillProvenance {
    pub fn validate(&self) -> anyhow::Result<()> {
        let path = self.manifest.to_path();
        match self.layout {
            SkillLayout::Flat => anyhow::ensure!(
                path.components().count() == 1 && path.extension().is_some_and(|e| e == "md"),
                "flat skill provenance must identify a direct-child .md file"
            ),
            SkillLayout::Package => anyhow::ensure!(
                path.components().count() >= 2 && path.file_name().is_some_and(|n| n == "SKILL.md"),
                "package skill provenance must identify a package SKILL.md"
            ),
        }
        Ok(())
    }
}
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SkillOrigin {
    pub provenance: SkillProvenance,
    pub advertised_document: PathBuf,
    pub canonical_document: PathBuf,
    pub canonical_package: Option<PathBuf>,
    pub canonical_root: PathBuf,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SkillInvocationPolicy {
    ModelAllowed,
    ExplicitOnly,
}
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SkillInterface {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub display_name: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub default_prompt: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub brand_color: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub icon_small: Option<SkillRelativePath>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub icon_large: Option<SkillRelativePath>,
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SkillDependency {
    pub kind: String,
    pub value: String,
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SkillMetadata {
    pub description: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub short_description: Option<String>,
    pub interface: SkillInterface,
    pub invocation_policy: SkillInvocationPolicy,
    #[serde(default)]
    pub dependencies: Vec<SkillDependency>,
}
impl SkillMetadata {
    pub fn new(description: impl Into<String>) -> Self {
        Self {
            description: description.into(),
            short_description: None,
            interface: SkillInterface::default(),
            invocation_policy: SkillInvocationPolicy::ModelAllowed,
            dependencies: Vec::new(),
        }
    }
    pub fn validate(&self) -> anyhow::Result<()> {
        anyhow::ensure!(
            !self.description.trim().is_empty(),
            "skill metadata requires a nonempty description"
        );
        anyhow::ensure!(
            self.dependencies.len() <= 64,
            "too many skill dependency declarations"
        );
        anyhow::ensure!(
            serde_json::to_vec(self)?.len() <= MAX_SKILL_METADATA_BYTES,
            "skill metadata exceeds its 128 KiB snapshot cap"
        );
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct SkillSnapshot {
    name: SkillName,
    body: String,
    metadata: SkillMetadata,
    provenance: Option<SkillProvenance>,
    digest: SkillDigest,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct SnapshotFields {
    name: SkillName,
    body: String,
    metadata: SkillMetadata,
    provenance: Option<SkillProvenance>,
    digest: SkillDigest,
}
impl<'de> Deserialize<'de> for SkillSnapshot {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        let fields = SnapshotFields::deserialize(d)?;
        let snapshot = Self {
            name: fields.name,
            body: fields.body,
            metadata: fields.metadata,
            provenance: fields.provenance,
            digest: fields.digest,
        };
        snapshot.validate().map_err(D::Error::custom)?;
        Ok(snapshot)
    }
}
impl SkillSnapshot {
    pub fn new(
        name: SkillName,
        description: impl Into<String>,
        body: impl Into<String>,
    ) -> anyhow::Result<Self> {
        Self::from_content(
            name,
            body.into().trim().into(),
            SkillMetadata::new(description),
            None,
        )
    }
    fn from_content(
        name: SkillName,
        body: String,
        metadata: SkillMetadata,
        provenance: Option<SkillProvenance>,
    ) -> anyhow::Result<Self> {
        let digest = SkillDigest::for_snapshot(&name, &body, &metadata, provenance.as_ref())?;
        let snapshot = Self {
            name,
            body,
            metadata,
            provenance,
            digest,
        };
        snapshot.validate()?;
        Ok(snapshot)
    }
    pub fn validate(&self) -> anyhow::Result<()> {
        anyhow::ensure!(
            !self.body.is_empty()
                && self.body == self.body.trim()
                && self.body.len() as u64 <= MAX_SKILL_BYTES,
            "skill snapshot body must be nonempty, normalized and within the {MAX_SKILL_BYTES}-byte cap"
        );
        self.metadata.validate()?;
        if let Some(p) = &self.provenance {
            p.validate()?;
        }
        anyhow::ensure!(
            SkillDigest::for_snapshot(
                &self.name,
                &self.body,
                &self.metadata,
                self.provenance.as_ref()
            )? == self.digest,
            "skill snapshot digest mismatch for {}",
            self.name
        );
        Ok(())
    }
    pub fn name(&self) -> &SkillName {
        &self.name
    }
    pub fn body(&self) -> &str {
        &self.body
    }
    pub fn description(&self) -> &str {
        &self.metadata.description
    }
    pub fn metadata(&self) -> &SkillMetadata {
        &self.metadata
    }
    pub fn provenance(&self) -> Option<&SkillProvenance> {
        self.provenance.as_ref()
    }
    pub fn digest(&self) -> SkillDigest {
        self.digest
    }
}
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SkillSource {
    File(PathBuf),
    PersistedSession,
    Programmatic(String),
}
impl fmt::Display for SkillSource {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::File(p) => p.display().fmt(f),
            Self::PersistedSession => f.write_str("persisted session snapshot"),
            Self::Programmatic(s) => f.write_str(s),
        }
    }
}
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SkillDefinition {
    snapshot: SkillSnapshot,
    source: SkillSource,
    origin: Option<SkillOrigin>,
}
impl SkillDefinition {
    pub fn new(
        name: SkillName,
        description: impl Into<String>,
        body: impl Into<String>,
        source: SkillSource,
    ) -> anyhow::Result<Self> {
        Ok(Self {
            snapshot: SkillSnapshot::new(name, description, body)?,
            source,
            origin: None,
        })
    }
    pub fn from_snapshot(snapshot: &SkillSnapshot) -> Self {
        Self {
            snapshot: snapshot.clone(),
            source: SkillSource::PersistedSession,
            origin: None,
        }
    }
    pub fn name(&self) -> &SkillName {
        self.snapshot.name()
    }
    pub fn description(&self) -> &str {
        self.snapshot.description()
    }
    pub fn body(&self) -> &str {
        self.snapshot.body()
    }
    pub fn digest(&self) -> SkillDigest {
        self.snapshot.digest()
    }
    pub fn source(&self) -> &SkillSource {
        &self.source
    }
    pub fn origin(&self) -> Option<&SkillOrigin> {
        self.origin.as_ref()
    }
    pub fn metadata(&self) -> &SkillMetadata {
        self.snapshot.metadata()
    }
    pub fn snapshot(&self) -> SkillSnapshot {
        self.snapshot.clone()
    }
    pub fn with_metadata(
        mut self,
        metadata: SkillMetadata,
        origin: Option<SkillOrigin>,
    ) -> anyhow::Result<Self> {
        self.snapshot = SkillSnapshot::from_content(
            self.snapshot.name,
            self.snapshot.body,
            metadata,
            origin.as_ref().map(|o| o.provenance.clone()),
        )?;
        self.origin = origin;
        Ok(self)
    }
}
/// Authoritative explicit-resolution completion metadata, never candidate selectors.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SkillMeta {
    pub name: SkillName,
    pub description: String,
}

//! Live package resources. Main instructions remain pinned engine directives.
//! Unix and Windows reads walk from one opened root with handle-relative
//! no-follow semantics. Other platforms fail closed.

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use std::path::Path;
use zevria_foundation::contained_read::{OpenedRoot, RelativePath, file_identity, read_regular};

use super::*;

const MAX_RESOURCE_BYTES: usize = 1024 * 1024;
const MAX_PAGE_BYTES: usize = 32 * 1024;

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct SkillReadRequest {
    pub skill: SkillName,
    pub resource: String,
    pub cursor: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct SkillResourcePage {
    pub skill: SkillName,
    pub resource: String,
    pub content_digest: String,
    pub contents: String,
    pub next_cursor: Option<String>,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct ResourceCursor {
    skill: SkillDigest,
    resource: String,
    identity: String,
    digest: String,
    offset: usize,
}

/// Unix alias policy is unchanged. Windows never resolves aliases before the
/// protected open: retain the lexical path and reject every reparse component.
pub(super) fn source_path(path: &Path) -> std::io::Result<std::path::PathBuf> {
    #[cfg(windows)]
    {
        let path = zevria_foundation::windows_io::normalize_disk_path(path)?;
        if std::fs::symlink_metadata(&path)?.is_dir() {
            zevria_foundation::windows_io::open_directory(&path)?;
        } else {
            let parent = zevria_foundation::windows_io::open_directory(
                path.parent()
                    .ok_or_else(|| std::io::Error::other("source has no parent"))?,
            )?;
            zevria_foundation::windows_io::open_relative(
                &parent,
                path.file_name()
                    .ok_or_else(|| std::io::Error::other("source has no filename"))?,
                false,
                false,
            )?;
        }
        Ok(path)
    }
    #[cfg(not(windows))]
    {
        std::fs::canonicalize(path)
    }
}

/// Bounded no-follow regular-file reader used for catalog source snapshots as
/// well as resource access. It never executes files or resolves package grants.
pub(crate) fn read_contained(
    root: &Path,
    relative: &SkillRelativePath,
    cap: usize,
) -> anyhow::Result<Vec<u8>> {
    let root = OpenedRoot::open(root)?;
    let file = root.open_file(&RelativePath::new(&relative.to_path())?)?;
    Ok(read_regular(file, cap)?)
}

impl SkillContext {
    /// Validate historical provenance against its exact recorded fixed scope.
    /// No same-name fallback or inferred package authority is permitted.
    pub fn read_resource(&self, request: &SkillReadRequest) -> anyhow::Result<SkillResourcePage> {
        self.resolve(&request.skill, SkillInvocationOrigin::Model)?;
        let snapshot = self
            .pins
            .get(&request.skill)
            .ok_or_else(|| anyhow::anyhow!("skill_read requires an active skill"))?;
        let provenance = snapshot.provenance().ok_or_else(|| {
            anyhow::anyhow!("this historical skill is body-only; package provenance is unavailable")
        })?;
        provenance.validate()?;
        anyhow::ensure!(
            provenance.layout == SkillLayout::Package,
            "flat skills are body-only and cannot read neighboring skills"
        );
        let resource = SkillRelativePath::new(request.resource.clone())?;
        let roots = self.catalog.roots.as_ref().ok_or_else(|| {
            anyhow::anyhow!("no fixed filesystem roots are bound to this skill catalog")
        })?;
        let fixed = roots
            .directory(provenance.scope)
            .ok_or_else(|| anyhow::anyhow!("recorded fixed skill scope is unavailable"))?;
        let canonical_root = source_path(fixed)
            .map_err(|error| anyhow::anyhow!("recorded skill source is unavailable: {error}"))?;
        let opened = OpenedRoot::open(&canonical_root)?;
        anyhow::ensure!(
            SourceBinding::for_opened_source(
                provenance.scope,
                &canonical_root,
                &provenance.manifest,
                &opened
            )? == provenance.source_binding,
            "recorded skill root identity changed; resources are unavailable"
        );
        let main = opened.open_file(&RelativePath::new(&provenance.manifest.to_path())?)?;
        let main_identity = file_identity(&main)?;
        let main_source = String::from_utf8(read_regular(main, MAX_SKILL_BYTES as usize)?)?;
        let manifest = provenance.manifest.to_path();
        let package = manifest
            .parent()
            .ok_or_else(|| anyhow::anyhow!("package location unavailable"))?;
        let derived = package
            .file_name()
            .and_then(|name| name.to_str())
            .ok_or_else(|| anyhow::anyhow!("package basename is not UTF-8"))?;
        let parsed = parser::parse_document(&main_source, derived, SkillLayout::Package)?;
        anyhow::ensure!(
            parsed.name == *snapshot.name() && parsed.body == snapshot.body(),
            "main instructions changed; historical body remains pinned but live resources are unavailable"
        );
        let relative = SkillRelativePath::new(format!(
            "{}/{}",
            provenance
                .manifest
                .as_str()
                .rsplit_once('/')
                .ok_or_else(|| anyhow::anyhow!("package locator has no parent"))?
                .0,
            resource.as_str()
        ))?;
        let file = opened.open_file(&RelativePath::new(&relative.to_path())?)?;
        let identity = file_identity(&file)?;
        anyhow::ensure!(
            identity != main_identity,
            "skill_read cannot read the main SKILL.md or an alias to it; instructions are engine-owned"
        );
        let bytes = read_regular(file, MAX_RESOURCE_BYTES)?;
        let digest = catalog::hex_digest(&bytes);
        let contents = String::from_utf8(bytes).map_err(|_| anyhow::anyhow!("binary/non-UTF-8 resources are unsupported by skill_read; no base64 or execution is provided"))?;
        anyhow::ensure!(
            !contents.contains('\0'),
            "binary resources containing NUL bytes are unsupported by skill_read"
        );
        let offset = if let Some(cursor) = &request.cursor {
            anyhow::ensure!(cursor.len() <= 4096, "skill_read cursor is too long");
            let cursor: ResourceCursor = serde_json::from_str(cursor)
                .map_err(|_| anyhow::anyhow!("invalid skill_read cursor"))?;
            anyhow::ensure!(
                cursor.skill == snapshot.digest()
                    && cursor.resource == request.resource
                    && cursor.identity == identity
                    && cursor.digest == digest,
                "resource changed or cursor is stale; restart pagination"
            );
            cursor.offset
        } else {
            0
        };
        anyhow::ensure!(
            offset <= contents.len() && contents.is_char_boundary(offset),
            "invalid skill_read cursor offset"
        );
        let mut end = contents.len().min(offset.saturating_add(24 * 1024));
        loop {
            while !contents.is_char_boundary(end) {
                end -= 1;
            }
            let next_cursor = (end < contents.len()).then(|| {
                serde_json::to_string(&ResourceCursor {
                    skill: snapshot.digest(),
                    resource: request.resource.clone(),
                    identity: identity.clone(),
                    digest: digest.clone(),
                    offset: end,
                })
                .expect("resource cursor serializes")
            });
            let page = SkillResourcePage {
                skill: request.skill.clone(),
                resource: request.resource.clone(),
                content_digest: digest.clone(),
                contents: contents[offset..end].to_string(),
                next_cursor,
            };
            if serde_json::to_vec(&page)?.len() <= MAX_PAGE_BYTES {
                return Ok(page);
            }
            anyhow::ensure!(
                end > offset,
                "resource response metadata exceeds the page cap"
            );
            end = offset + (end - offset) / 2;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    fn fixture() -> (tempfile::TempDir, SkillContext, std::path::PathBuf) {
        let directory = tempfile::tempdir().expect("directory");
        let roots = FixedSkillRoots::fixture(None, directory.path());
        let package = roots.project().join("review");
        std::fs::create_dir_all(package.join("references")).expect("package");
        std::fs::write(
            package.join("SKILL.md"),
            "---\ndescription: Review\n---\nPinned body",
        )
        .expect("manifest");
        std::fs::write(
            package.join("references/guide.txt"),
            "Guide é🦀\n".repeat(5000),
        )
        .expect("reference");
        let catalog = Arc::new(
            SkillCatalog::from_discovery(discover_skills(&roots), Default::default())
                .expect("catalog"),
        );
        let snapshot = catalog.get("review").expect("definition").snapshot();
        let pins = ActiveSkills::from_snapshots([snapshot]).expect("active");
        (
            directory,
            SkillContext {
                catalog,
                pins,
                mode_enabled: true,
            },
            package,
        )
    }

    fn request(resource: &str) -> SkillReadRequest {
        SkillReadRequest {
            skill: SkillName::parse("review").expect("name"),
            resource: resource.into(),
            cursor: None,
        }
    }

    #[test]
    fn skill_resources_are_live_digest_bound_and_main_body_stays_pinned() {
        let (_directory, context, package) = fixture();
        let first = context
            .read_resource(&request("references/guide.txt"))
            .expect("first page");
        assert!(first.next_cursor.is_some());
        let mut next = request("references/guide.txt");
        next.cursor = first.next_cursor;
        context
            .read_resource(&next)
            .expect("same content second page");
        std::fs::write(package.join("references/guide.txt"), "new live reference")
            .expect("edit auxiliary");
        assert!(context.read_resource(&next).is_err());
        let changed = context
            .read_resource(&request("references/guide.txt"))
            .expect("new read");
        assert_eq!(changed.contents, "new live reference");
        assert_ne!(first.content_digest, changed.content_digest);
        std::fs::write(
            package.join("SKILL.md"),
            "---\ndescription: Review\n---\nChanged body",
        )
        .expect("edit main");
        assert!(
            context
                .read_resource(&request("references/guide.txt"))
                .is_err()
        );
        assert_eq!(
            context.pins.snapshots().next().expect("snapshot").body(),
            "Pinned body"
        );
    }

    #[cfg(windows)]
    #[test]
    fn native_validation_accepts_both_drive_prefix_representations() {
        let (_directory, context, package) = fixture();
        let roots = context.catalog.roots.as_ref().unwrap();
        for source in [&package, &package.join("SKILL.md")] {
            // Change only the drive prefix. canonicalize() can expand a short
            // alias (e.g. RUNNER~1) while the fixed root keeps its lexical name.
            let verbatim = zevria_foundation::windows_io::normalize_disk_path(source).unwrap();
            let ordinary = Path::new(
                verbatim
                    .to_str()
                    .unwrap()
                    .strip_prefix(r"\\?\")
                    .expect("verbatim drive prefix"),
            );
            for path in [ordinary, verbatim.as_path()] {
                let (definition, diagnostics) =
                    validate_skill_path(roots, path).unwrap_or_else(|error| {
                        panic!(
                            "validation of {path:?} under {:?}: {error:#}",
                            roots.project()
                        )
                    });
                assert_eq!(definition.body(), "Pinned body");
                assert_eq!(
                    &definition.snapshot(),
                    context.pins.snapshots().next().unwrap(),
                    "drive-prefix spelling must preserve the pinned source: {path:?}"
                );
                assert!(diagnostics.is_empty(), "{path:?}: {diagnostics:?}");
            }
        }
    }

    #[test]
    fn replacing_the_root_at_the_same_path_does_not_rebind_resources() {
        let (directory, context, package) = fixture();
        let root = context
            .catalog
            .roots
            .as_ref()
            .unwrap()
            .project()
            .to_path_buf();
        let manifest = std::fs::read(package.join("SKILL.md")).unwrap();
        std::fs::rename(&root, directory.path().join("old-root")).unwrap();
        std::fs::create_dir_all(package.join("references")).unwrap();
        std::fs::write(package.join("SKILL.md"), manifest).unwrap();
        std::fs::write(package.join("references/guide.txt"), "replacement secret").unwrap();
        assert!(
            context
                .read_resource(&request("references/guide.txt"))
                .unwrap_err()
                .to_string()
                .contains("root identity changed")
        );
    }

    #[test]
    fn skill_resources_allow_metadata_only_edits_without_replacing_the_pin() {
        let (_directory, mut context, package) = fixture();
        let original = context.pins.snapshots().next().unwrap().clone();
        std::fs::write(package.join("SKILL.md"),
            "---\ndescription: Changed description\nmetadata:\n  short-description: Changed metadata\n---\nPinned body").unwrap();
        context.catalog = Arc::new(
            SkillCatalog::from_discovery(
                discover_skills(context.catalog.roots.as_ref().unwrap()),
                SkillsConfig::default(),
            )
            .unwrap(),
        );
        let installed = context.catalog.get("review").unwrap();
        assert_ne!(installed.digest(), original.digest());
        assert_eq!(installed.body(), original.body());
        assert!(
            context
                .read_resource(&request("references/guide.txt"))
                .unwrap()
                .contents
                .starts_with("Guide")
        );
        assert_eq!(context.pins.snapshots().next(), Some(&original));
    }

    #[cfg(unix)]
    #[test]
    fn skill_resources_reject_canonical_root_retarget_even_after_reload() {
        use std::os::unix::fs::symlink;
        let (directory, mut context, package) = fixture();
        let original = context.pins.snapshots().next().unwrap().clone();
        let roots = context.catalog.roots.as_ref().unwrap().clone();
        let revision = context.catalog.revision().to_owned();
        let replacement = directory.path().join("replacement");
        std::fs::create_dir_all(replacement.join("review/references")).unwrap();
        std::fs::copy(
            package.join("SKILL.md"),
            replacement.join("review/SKILL.md"),
        )
        .unwrap();
        std::fs::write(
            replacement.join("review/references/guide.txt"),
            "DIFFERENT ROOT DATA",
        )
        .unwrap();
        std::fs::rename(roots.project(), directory.path().join("original")).unwrap();
        symlink(&replacement, roots.project()).unwrap();
        let read = request("references/guide.txt");
        assert!(
            context
                .read_resource(&read)
                .unwrap_err()
                .to_string()
                .contains("root identity changed")
        );
        context.catalog = Arc::new(
            SkillCatalog::from_discovery(discover_skills(&roots), SkillsConfig::default()).unwrap(),
        );
        assert_ne!(context.catalog.revision(), revision);
        assert!(
            context.read_resource(&read).is_err(),
            "reload cannot rebind historical authority"
        );
        assert_eq!(context.pins.snapshots().next(), Some(&original));
        let installed = context.catalog.get("review").unwrap().snapshot();
        assert_eq!(installed.body(), original.body());
        assert_ne!(installed.digest(), original.digest());
        let mut tampered = serde_json::to_value(&installed).unwrap();
        tampered["provenance"]["scope"] = serde_json::json!("global");
        assert!(serde_json::from_value::<SkillSnapshot>(tampered).is_err());
        context.pins = ActiveSkills::from_snapshots([installed]).unwrap();
        assert_eq!(
            context.read_resource(&read).unwrap().contents,
            "DIFFERENT ROOT DATA"
        );
    }

    #[cfg(unix)]
    #[test]
    fn skill_resource_directory_symlink_races_never_read_outside_text() {
        use std::os::unix::fs::symlink;
        let (directory, context, package) = fixture();
        let outside = directory.path().join("outside-package");
        std::fs::create_dir(&outside).expect("outside fixture directory");
        std::fs::write(outside.join("guide"), "OUTSIDE SECRET").expect("outside fixture");
        let swapping = package.join("swapping");
        let parked = package.join("parked");
        std::fs::create_dir(&swapping).expect("inside directory");
        std::fs::write(swapping.join("guide"), "inside").expect("inside text");
        let writer = std::thread::spawn(move || {
            for _ in 0..200 {
                std::fs::rename(&swapping, &parked).expect("park inside directory");
                symlink(&outside, &swapping).expect("install racing outside alias");
                std::thread::yield_now();
                std::fs::remove_file(&swapping).expect("remove racing alias");
                std::fs::rename(&parked, &swapping).expect("restore inside directory");
            }
        });
        for _ in 0..200 {
            if let Ok(page) = context.read_resource(&request("swapping/guide")) {
                assert_eq!(page.contents, "inside");
            }
        }
        writer.join().expect("race writer");
        assert_eq!(
            context
                .read_resource(&request("swapping/guide"))
                .expect("restored read")
                .contents,
            "inside"
        );
    }

    #[test]
    fn skill_resources_require_provenance_and_respect_disable_and_source_loss() {
        let (_directory, context, package) = fixture();
        let mut no_provenance = context.clone();
        no_provenance.pins = ActiveSkills::from_snapshots([SkillSnapshot::new(
            SkillName::parse("review").expect("name"),
            "Review",
            "Pinned body",
        )
        .expect("current snapshot without provenance")])
        .expect("active ledger");
        assert!(
            no_provenance
                .read_resource(&request("references/guide.txt"))
                .expect_err("no inferred source")
                .to_string()
                .contains("provenance")
        );
        let mut disabled = context.clone();
        let mut config = SkillsConfig::default();
        config.rules.push(SkillEnableRule {
            name: SkillName::parse("review").expect("name"),
            enabled: false,
        });
        let registry = context
            .catalog
            .as_ref()
            .clone()
            .with_config(config)
            .expect("disabled config");
        disabled.catalog = Arc::new(registry);
        assert!(
            disabled
                .read_resource(&request("references/guide.txt"))
                .expect_err("disabled precedes source checks")
                .to_string()
                .contains("disabled")
        );
        std::fs::remove_dir_all(package).expect("remove source");
        assert!(
            context
                .read_resource(&request("references/guide.txt"))
                .is_err()
        );
        assert_eq!(
            context.pins.len(),
            1,
            "source loss does not remove the historical activation"
        );
    }

    #[test]
    fn skill_resources_reject_traversal_aliases_special_files_and_bad_text() {
        let (_directory, context, package) = fixture();
        #[cfg(unix)]
        {
            use std::os::unix::fs::symlink;
            symlink("guide.txt", package.join("references/alias")).expect("leaf alias");
            symlink(_directory.path(), package.join("escape")).expect("directory alias");
        }
        std::fs::hard_link(package.join("SKILL.md"), package.join("main-alias"))
            .expect("hard main alias");
        std::fs::write(package.join("binary"), [0xff, 0xfe]).expect("binary");
        std::fs::write(package.join("nul-binary"), [0, 0, 0]).expect("NUL binary");
        std::fs::write(package.join("huge"), vec![b'a'; MAX_RESOURCE_BYTES + 1]).expect("oversize");
        #[cfg(unix)]
        {
            let fifo = std::ffi::CString::new(package.join("fifo").to_str().expect("path"))
                .expect("c path");
            // SAFETY: a valid NUL-terminated temporary fixture path and mode.
            assert_eq!(unsafe { libc::mkfifo(fifo.as_ptr(), 0o600) }, 0);
        }
        for path in [
            "../outside",
            "/etc/passwd",
            "a/./b",
            "C:/outside",
            "SKILL.md",
            "main-alias",
            "references",
            "references/alias",
            "escape/file",
            "binary",
            "nul-binary",
            "huge",
            "fifo",
        ] {
            assert!(context.read_resource(&request(path)).is_err(), "{path}");
        }
        let mut inactive = context.clone();
        inactive.pins = ActiveSkills::default();
        assert!(
            inactive
                .read_resource(&request("references/guide.txt"))
                .is_err()
        );
    }
}

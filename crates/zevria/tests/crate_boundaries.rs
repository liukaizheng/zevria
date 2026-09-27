//! Architectural production edges, inspected without invoking Cargo recursively.
use std::{
    collections::{BTreeMap, BTreeSet},
    path::{Path, PathBuf},
};
use toml::Value;

type Graph = BTreeMap<String, BTreeSet<String>>;

fn manifest(path: &Path) -> Value {
    let contents =
        std::fs::read_to_string(path).unwrap_or_else(|error| panic!("{}: {error}", path.display()));
    toml::from_str(&contents).unwrap_or_else(|error| panic!("{}: {error}", path.display()))
}

fn dependency_name(
    alias: &str,
    value: &Value,
    workspace: &Value,
    root: &Path,
    base: &Path,
) -> String {
    let (value, base) = if value.get("workspace").and_then(Value::as_bool) == Some(true) {
        (&workspace["dependencies"][alias], root)
    } else {
        (value, base)
    };
    if let Some(package) = value.get("package").and_then(Value::as_str) {
        return package.to_owned();
    }
    if let Some(path) = value.get("path").and_then(Value::as_str) {
        return manifest(&base.join(path).join("Cargo.toml"))["package"]["name"]
            .as_str()
            .expect("path dependency has a package name")
            .to_owned();
    }
    alias.to_owned()
}

fn production_dependencies(
    document: &Value,
    workspace: &Value,
    root: &Path,
    base: &Path,
) -> BTreeSet<String> {
    let mut result = BTreeSet::new();
    let mut add = |table: &Value| {
        for section in ["dependencies", "build-dependencies"] {
            if let Some(dependencies) = table.get(section).and_then(Value::as_table) {
                for (alias, value) in dependencies {
                    // Optional and target-specific edges count conservatively:
                    // enabling a feature/platform may not bypass architecture.
                    result.insert(dependency_name(alias, value, workspace, root, base));
                }
            }
        }
    };
    add(document);
    if let Some(targets) = document.get("target").and_then(Value::as_table) {
        for target in targets.values() {
            add(target);
        }
    }
    result
}

fn reaches(graph: &Graph, start: &str, destination: &str) -> bool {
    let mut visited = BTreeSet::new();
    let mut pending = vec![start];
    while let Some(node) = pending.pop() {
        if visited.insert(node) {
            for next in &graph[node] {
                if next == destination {
                    return true;
                }
                pending.push(next);
            }
        }
    }
    false
}

#[test]
fn production_domain_graph_has_only_approved_edges() {
    let root = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../..");
    let workspace = manifest(&root.join("Cargo.toml"));
    let workspace = &workspace["workspace"];
    let mut manifests = BTreeMap::new();
    for entry in std::fs::read_dir(root.join("crates")).unwrap() {
        let base = entry.unwrap().path();
        let path = base.join("Cargo.toml");
        if path.is_file() {
            let document = manifest(&path);
            let name = document["package"]["name"].as_str().unwrap().to_owned();
            assert!(manifests.insert(name, (base, document)).is_none());
        }
    }
    let lower = ["foundation", "content", "instructions", "model", "workflow"];
    let allowed: [(&str, Vec<&str>); 19] = [
        ("foundation", vec![]),
        ("content", vec!["foundation"]),
        ("instructions", vec!["foundation", "content"]),
        ("model", vec!["foundation", "content", "instructions"]),
        ("workflow", vec!["foundation", "content"]),
        ("transcript", lower.to_vec()),
        ("session-api", lower.to_vec()),
        (
            "core",
            [lower.as_slice(), &["session-api", "transcript"]].concat(),
        ),
        (
            "responses",
            vec!["foundation", "content", "instructions", "model"],
        ),
        (
            "provider",
            [lower.as_slice(), &["responses", "session-api"]].concat(),
        ),
        ("tools", [lower.as_slice(), &["session-api"]].concat()),
        (
            "acp",
            [lower.as_slice(), &["session-api", "transcript"]].concat(),
        ),
        ("theme", vec![]),
        ("tui-widgets", [lower.as_slice(), &["theme"]].concat()),
        (
            "tui-input",
            [lower.as_slice(), &["theme", "tui-widgets"]].concat(),
        ),
        (
            "tui",
            [
                lower.as_slice(),
                &[
                    "theme",
                    "tui-widgets",
                    "tui-input",
                    "session-api",
                    "transcript",
                ],
            ]
            .concat(),
        ),
        (
            "ensemble",
            [lower.as_slice(), &["session-api", "transcript", "acp"]].concat(),
        ),
        (
            "app",
            [
                lower.as_slice(),
                &[
                    "core",
                    "provider",
                    "tools",
                    "acp",
                    "ensemble",
                    "theme",
                    "session-api",
                    "transcript",
                ],
            ]
            .concat(),
        ),
        (
            "zevria",
            [
                lower.as_slice(),
                &[
                    "app",
                    "acp",
                    "tui",
                    "tui-widgets",
                    "tui-input",
                    "theme",
                    "session-api",
                    "transcript",
                ],
            ]
            .concat(),
        ),
    ];
    let package = |name: &str| {
        if name == "zevria" {
            name.to_owned()
        } else {
            format!("zevria-{name}")
        }
    };
    assert_eq!(
        manifests.keys().cloned().collect::<BTreeSet<_>>(),
        allowed.iter().map(|(name, _)| package(name)).collect()
    );
    let graph: Graph = manifests
        .iter()
        .map(|(name, (base, document))| {
            let dependencies = production_dependencies(document, workspace, &root, base)
                .into_iter()
                .filter(|dependency| manifests.contains_key(dependency))
                .collect();
            (name.clone(), dependencies)
        })
        .collect();
    for (name, owners) in allowed {
        let name = package(name);
        let allowed = owners.into_iter().map(package).collect::<BTreeSet<_>>();
        assert!(
            graph[&name].is_subset(&allowed),
            "forbidden production edges from {name}: {:?}",
            graph[&name].difference(&allowed).collect::<Vec<_>>()
        );
        assert!(
            !reaches(&graph, &name, &name),
            "production cycle through {name}"
        );
        if name != "zevria-app" {
            assert!(
                !graph[&name].contains("zevria-core"),
                "only app may directly own an engine"
            );
        }
    }
    for adapter in ["provider", "tools", "acp", "tui", "ensemble"] {
        assert!(!reaches(&graph, &package(adapter), "zevria-core"));
    }
    assert!(!reaches(&graph, "zevria-session-api", "zevria-transcript"));
    for forbidden in ["core", "session-api", "transcript", "provider", "app"] {
        assert!(!reaches(&graph, "zevria-responses", &package(forbidden)));
    }
    for frontend in ["tui", "tui-input", "tui-widgets"] {
        assert!(!reaches(&graph, "zevria-app", &package(frontend)));
    }
}

#[test]
fn boundary_reader_resolves_aliases_targets_and_build_edges_but_ignores_dev_edges() {
    let workspace: Value = toml::from_str(
        r#"
[dependencies]
renamed = { package = "zevria-core", version = "0.1" }
"#,
    )
    .unwrap();
    let document: Value = toml::from_str(
        r#"
[dependencies]
renamed = { workspace = true, optional = true }
[build-dependencies]
codec = { package = "zevria-responses", version = "0.1" }
[dev-dependencies]
ignored = { package = "zevria-tui", version = "0.1" }
[target.'cfg(unix)'.dependencies]
store = { package = "zevria-transcript", version = "0.1" }
[target.'cfg(windows)'.build-dependencies]
api = { package = "zevria-session-api", version = "0.1" }
[target.'cfg(unix)'.dev-dependencies]
also_ignored = { package = "zevria-app", version = "0.1" }
"#,
    )
    .unwrap();
    assert_eq!(
        production_dependencies(&document, &workspace, Path::new("."), Path::new(".")),
        [
            "zevria-core",
            "zevria-responses",
            "zevria-transcript",
            "zevria-session-api"
        ]
        .into_iter()
        .map(str::to_owned)
        .collect()
    );
}

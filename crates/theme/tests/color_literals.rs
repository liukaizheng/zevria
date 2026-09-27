//! Prevent new component-local colors, while permitting deliberate test fixtures.
use std::{fs, path::Path};
use syn::visit::Visit;

#[derive(Default)]
struct ColorPaths {
    found: bool,
}
impl<'ast> Visit<'ast> for ColorPaths {
    fn visit_item(&mut self, item: &'ast syn::Item) {
        use syn::Item::*;
        let attrs = match item {
            Const(item) => &item.attrs,
            Enum(item) => &item.attrs,
            ExternCrate(item) => &item.attrs,
            Fn(item) => &item.attrs,
            ForeignMod(item) => &item.attrs,
            Impl(item) => &item.attrs,
            Macro(item) => &item.attrs,
            Mod(item) => &item.attrs,
            Static(item) => &item.attrs,
            Struct(item) => &item.attrs,
            Trait(item) => &item.attrs,
            TraitAlias(item) => &item.attrs,
            Type(item) => &item.attrs,
            Union(item) => &item.attrs,
            Use(item) => &item.attrs,
            _ => return syn::visit::visit_item(self, item),
        };
        if attrs.iter().any(|attr| {
            attr.path().is_ident("cfg")
                && attr
                    .parse_args::<syn::Path>()
                    .is_ok_and(|path| path.is_ident("test"))
        }) {
            return;
        }
        syn::visit::visit_item(self, item);
    }
    fn visit_path(&mut self, path: &'ast syn::Path) {
        self.found |= path
            .segments
            .iter()
            .take(path.segments.len().saturating_sub(1))
            .any(|segment| segment.ident == "Color");
        syn::visit::visit_path(self, path);
    }
    fn visit_macro(&mut self, mac: &'ast syn::Macro) {
        // Macro bodies aren't parsed into paths, but can construct colors too.
        self.found |= mac.tokens.to_string().replace(' ', "").contains("Color::");
    }
}

fn inspect(path: &Path, violations: &mut Vec<String>) {
    for entry in fs::read_dir(path).expect("read source tree") {
        let path = entry.expect("source entry").path();
        if path.is_dir() {
            if !matches!(
                path.file_name().and_then(|name| name.to_str()),
                Some("theme" | "tests" | "target")
            ) {
                inspect(&path, violations);
            }
        } else if path.extension().is_some_and(|extension| extension == "rs") {
            let name = path.file_stem().unwrap().to_string_lossy();
            if name == "tests" || name.ends_with("_tests") {
                continue;
            }
            let source = fs::read_to_string(&path).expect("UTF-8 Rust source");
            let file = syn::parse_file(&source)
                .unwrap_or_else(|error| panic!("{}: {error}", path.display()));
            let mut visitor = ColorPaths::default();
            visitor.visit_file(&file);
            if visitor.found {
                violations.push(path.display().to_string());
            }
        }
    }
}

#[test]
fn components_use_theme_roles_not_color_literals() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).parent().unwrap();
    let mut violations = Vec::new();
    inspect(root, &mut violations);
    assert!(
        violations.is_empty(),
        "Color constructors/patterns belong in crates/theme, not:\n{}",
        violations.join("\n")
    );
}

#[test]
fn guard_does_not_skip_production_after_a_test_module() {
    for (source, expected) in [
        (
            "#[cfg(test)] mod tests { const C: Color = Color::Red; } fn f() { let _ = Color::Blue; }",
            true,
        ),
        ("#[cfg(test)] fn fixture() { let _ = Color::Blue; }", false),
        ("fn f() { colors!(Color::Rgb(1, 2, 3)); }", true),
        ("fn f() { let _ = theme().text.primary; }", false),
    ] {
        let mut visitor = ColorPaths::default();
        visitor.visit_file(&syn::parse_file(source).unwrap());
        assert_eq!(visitor.found, expected, "{source}");
    }
}

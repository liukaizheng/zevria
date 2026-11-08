//! Bounded, window-independent TeX parsing and em-based layout.
use ratex_layout::{LayoutOptions, layout, to_display_list, validate_layout};
use ratex_types::{color::Color as TexColor, display_item::DisplayItem, math_style::MathStyle};
use std::{
    collections::{VecDeque, hash_map::DefaultHasher},
    hash::{Hash, Hasher},
    panic::catch_unwind,
    sync::{Arc, LazyLock, Mutex},
};

pub(super) const MAX_TEX_BYTES: usize = 16 * 1024;
pub(super) const MAX_WIDTH_EM: f64 = 200.;
pub(super) const MAX_HEIGHT_EM: f64 = 100.;
pub(super) const MAX_TYPESET_ENTRIES: usize = 1024;
// Black is the base ink, with a private alpha sentinel for inheritance. Keep
// explicit opaque black distinguishable: \\color{black}/\\colorbox{black} must
// not turn into the surrounding (often light) foreground. Resolve before paint.
pub(super) const INHERITED_INK: TexColor = TexColor::new(0., 0., 0., -1.);

#[derive(Debug)]
pub(super) struct FallbackGlyph {
    pub character: char,
    pub x: f64,
    pub y: f64,
    pub scale: f64,
    pub color: TexColor,
}

#[derive(Debug)]
pub(super) struct Formula {
    pub tex: Arc<str>,
    pub display: bool,
    pub fingerprint: u64,
    pub width: f64,
    pub height: f64,
    pub depth: f64,
    pub items: Vec<DisplayItem>,
    pub fallback: Vec<FallbackGlyph>,
}

struct MemoEntry {
    tex: Arc<str>,
    display: bool,
    formula: Option<Arc<Formula>>,
}
#[derive(Default)]
struct Memo(VecDeque<MemoEntry>);
impl Memo {
    fn get(&mut self, tex: &str, display: bool) -> Option<Option<Arc<Formula>>> {
        let index = self
            .0
            .iter()
            .position(|e| e.display == display && &*e.tex == tex)?;
        let entry = self.0.remove(index)?;
        let result = entry.formula.clone();
        self.0.push_back(entry);
        Some(result)
    }
    fn insert(&mut self, tex: &str, display: bool, formula: Option<Arc<Formula>>) {
        if self.0.len() == MAX_TYPESET_ENTRIES {
            self.0.pop_front();
        }
        self.0.push_back(MemoEntry {
            tex: tex.into(),
            display,
            formula,
        });
    }
}
static MEMO: LazyLock<Mutex<Memo>> = LazyLock::new(|| Mutex::new(Memo::default()));

/// The memo includes failures: plugins probe inline nodes twice, and streaming
/// reparses the last block. Neither path should repeatedly typeset bad input.
pub(super) fn typeset(tex: &str, display: bool) -> Option<Arc<Formula>> {
    if tex.len() > MAX_TEX_BYTES || tex.trim().is_empty() {
        return None;
    }
    if let Some(cached) = MEMO
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .get(tex, display)
    {
        return cached;
    }
    // Do not hold the memo lock across library code, including unwind. RaTeX
    // also limits macro expansions, nesting depth, and layout output items.
    let formula = catch_unwind(|| build(tex, display))
        .ok()
        .flatten()
        .map(Arc::new);
    let mut memo = MEMO.lock().unwrap_or_else(|e| e.into_inner());
    if let Some(cached) = memo.get(tex, display) {
        return cached; // Another background parser may have won the race.
    }
    memo.insert(tex, display, formula.clone());
    formula
}

fn build(tex: &str, display: bool) -> Option<Formula> {
    let nodes = ratex_parser::parse(tex).ok()?;
    let root = layout(
        &nodes,
        &LayoutOptions {
            style: if display {
                MathStyle::Display
            } else {
                MathStyle::Text
            },
            color: INHERITED_INK,
            ..LayoutOptions::default()
        },
    );
    validate_layout(&root).ok()?;
    let list = to_display_list(&root);
    if !valid_dimensions(list.width, list.height, list.depth) {
        return None;
    }
    let mut items = Vec::with_capacity(list.items.len());
    let mut fallback = Vec::new();
    for item in list.items {
        if let DisplayItem::GlyphPath {
            x,
            y,
            scale,
            ref font,
            char_code,
            color,
        } = item
            && super::raster::glyph(font, char_code).is_none()
        {
            fallback.push(FallbackGlyph {
                character: char::from_u32(char_code)?,
                x,
                y,
                scale,
                color,
            });
        } else {
            items.push(item);
        }
    }
    if items.is_empty() && fallback.is_empty() {
        return None;
    }
    let mut hash = DefaultHasher::new();
    tex.hash(&mut hash);
    display.hash(&mut hash);
    Some(Formula {
        tex: tex.into(),
        display,
        fingerprint: hash.finish(),
        width: list.width,
        height: list.height,
        depth: list.depth,
        items,
        fallback,
    })
}

fn valid_dimensions(width: f64, height: f64, depth: f64) -> bool {
    [width, height, depth].iter().all(|x| x.is_finite())
        && width > 0.
        && width <= MAX_WIDTH_EM
        && height >= 0.
        && depth >= 0.
        && height + depth > 0.
        && height + depth <= MAX_HEIGHT_EM
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fixture_typesets_in_both_modes() {
        for (tex, display, _) in super::super::tests::fixture_formulas() {
            let formula = typeset(&tex, display).expect("fixture formula typesets");
            assert!(valid_dimensions(
                formula.width,
                formula.height,
                formula.depth
            ));
        }
        let inline = typeset(r"\sum_{i=0}^n i", false).unwrap();
        let display = typeset(r"\sum_{i=0}^n i", true).unwrap();
        assert!(display.height + display.depth > inline.height + inline.depth);
    }
    #[test]
    fn rejects_invalid_unbounded_and_empty_input() {
        for tex in [
            r"\undefinedcommand",
            "{x",
            r"\def\a{\a\a}\a",
            "",
            "  ",
            r"\rule{201em}{1em}",
            r"\rule{1em}{101em}",
        ] {
            assert!(typeset(tex, true).is_none(), "rejected input");
        }
        assert!(typeset(&"x".repeat(MAX_TEX_BYTES + 1), false).is_none());
        for dimensions in [
            (f64::NAN, 1., 0.),
            (1., f64::INFINITY, 0.),
            (201., 1., 0.),
            (1., 50., 51.),
            (0., 1., 0.),
        ] {
            assert!(!valid_dimensions(dimensions.0, dimensions.1, dimensions.2));
        }
    }
    #[test]
    fn limits_are_inclusive_and_library_work_guards_are_respected() {
        let tex = format!("{}x", " ".repeat(MAX_TEX_BYTES - 1));
        assert!(typeset(&tex, false).is_some());
        assert!(typeset(r"\rule{200em}{1em}", true).is_some());
        assert!(typeset(r"\rule{1em}{100em}", true).is_some());
        let nested = format!("{}x{}", "{".repeat(64), "}".repeat(64));
        assert!(typeset(&nested, false).is_none());
        let shadows = format!("{}x{}", r"\pmb{".repeat(17), "}".repeat(17));
        assert!(typeset(&shadows, false).is_none());
    }
    #[test]
    fn memo_reuses_arcs_and_evicts_oldest() {
        assert!(Arc::ptr_eq(
            &typeset("x+1", false).unwrap(),
            &typeset("x+1", false).unwrap()
        ));
        let mut memo = Memo::default();
        for i in 0..MAX_TYPESET_ENTRIES + 1 {
            memo.insert(&i.to_string(), false, None);
        }
        assert_eq!(memo.0.len(), MAX_TYPESET_ENTRIES);
        assert!(memo.get("0", false).is_none());
        assert_eq!(memo.get("1", false).map(|x| x.is_none()), Some(true));
        memo.insert("last", false, None);
        assert!(memo.get("2", false).is_none());
        assert!(memo.get("1", false).is_some());
    }
    #[test]
    fn unicode_is_left_to_native_font_fallback() {
        let formula = typeset(r"\text{中文😊}", false).unwrap();
        assert_eq!(
            formula
                .fallback
                .iter()
                .map(|g| g.character)
                .collect::<String>(),
            "中文😊"
        );
    }
}

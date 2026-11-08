use super::{MathRasters, closed_display, extensions, formula_element, typeset};
use gpui_kit::{
    AvailableSpace, Bounds, Context, Entity, IntoElement, Render, TestAppContext, Window,
    WindowBounds, WindowHandle, WindowOptions, canvas, div, prelude::*, px, size,
};
use gpui_kit::{
    base::{
        input::{Copy, SelectAll},
        text::{SelectionFormat, TextView, TextViewState},
    },
    test::TestWindowExt as _,
};

pub(super) const PROOF: &str = include_str!("../../../tests/fixtures/holder-proof.md");

/// Fixture-only extraction, not a second Markdown parser. The parser's own
/// contract tests separately pin all 26 nodes, spans, and streaming boundaries.
pub(super) fn fixture_formulas() -> Vec<(String, bool, String)> {
    let bytes = PROOF.as_bytes();
    let mut cursor = 0;
    let mut found = Vec::new();
    while cursor + 1 < bytes.len() {
        if bytes[cursor] != b'\\' || !matches!(bytes[cursor + 1], b'(' | b'[') {
            cursor += 1;
            continue;
        }
        let start = cursor;
        let display = bytes[cursor + 1] == b'[';
        let close = if display { b']' } else { b')' };
        cursor += 2;
        let content = cursor;
        while cursor + 1 < bytes.len() {
            if bytes[cursor] == b'\\' {
                if bytes[cursor + 1] == close {
                    break;
                }
                cursor += 2;
            } else {
                cursor += 1;
            }
        }
        let tex = PROOF[content..cursor].trim().to_owned();
        cursor += 2;
        found.push((tex, display, PROOF[start..cursor].to_owned()));
    }
    assert_eq!(found.iter().filter(|f| f.1).count(), 11);
    assert_eq!(found.len(), 26);
    found
}
struct MathView {
    state: Entity<TextViewState>,
    format: SelectionFormat,
}
impl Render for MathView {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        div()
            .size_full()
            .p(px(20.))
            .text_size(px(16.))
            .line_height(px(27.))
            .child(
                TextView::new(&self.state)
                    .size_full()
                    .markdown_extensions(extensions())
                    .selection_format(self.format)
                    .style(crate::ui::style::markdown(cx, false)),
            )
    }
}
type TestWindow = WindowHandle<gpui_kit::base::Root>;
fn setup(
    source: &str,
    format: SelectionFormat,
    cx: &mut TestAppContext,
) -> (TestWindow, Entity<TextViewState>) {
    cx.update(|cx| {
        gpui_kit::init(cx);
        crate::ui::theme::apply(zevria_theme::ZEVRIA_GRAPHITE, cx);
        let state = cx.new(|cx| TextViewState::markdown(source, cx));
        let (window, _) = gpui_kit::open_window(
            WindowOptions {
                window_bounds: Some(WindowBounds::Windowed(Bounds {
                    origin: Default::default(),
                    size: size(px(900.), px(3200.)),
                })),
                ..Default::default()
            },
            cx,
            |_, cx| {
                cx.new(|_| MathView {
                    state: state.clone(),
                    format,
                })
            },
        )
        .unwrap();
        (window.downcast().unwrap(), state)
    })
}
fn frame(window: TestWindow, cx: &mut TestAppContext) {
    for _ in 0..3 {
        cx.run_until_parked();
        cx.update_window(window.into(), |_, window, cx| window.render_frame(cx))
            .unwrap();
    }
}
fn copy(window: TestWindow, state: &Entity<TextViewState>, cx: &mut TestAppContext) -> String {
    cx.update_window(window.into(), |_, window, cx| {
        let focus = state.read(cx).focus_handle().clone();
        focus.focus(window, cx);
        focus.dispatch_action(&SelectAll, window, cx);
        focus.dispatch_action(&Copy, window, cx);
        cx.read_from_clipboard()
            .and_then(|i| i.text())
            .expect("native copy")
    })
    .unwrap()
}

#[gpui_kit::test]
fn fixture_plain_copy_keeps_every_delimiter_and_paint_populates_cache(cx: &mut TestAppContext) {
    let (window, state) = setup(PROOF, SelectionFormat::Plain, cx);
    frame(window, cx);
    let copied = copy(window, &state, cx);
    for (_, _, source) in fixture_formulas() {
        assert!(copied.contains(&source), "raw formula source preserved");
    }
    // This viewport holds the entire fixture. In particular, zero-depth AB
    // and square must paint, not spuriously fail GPUI's rounded baseline check.
    cx.update(|cx| assert_eq!(cx.global::<MathRasters>().len(), 26));
}

#[gpui_kit::test]
fn measurement_does_not_rasterize(cx: &mut TestAppContext) {
    struct Measure(std::sync::Arc<std::sync::atomic::AtomicBool>);
    impl Render for Measure {
        fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
            let checked = self.0.clone();
            canvas(
                move |_, window, cx| {
                    assert!(!cx.has_global::<MathRasters>());
                    let formula = typeset(r"\frac{x}{y}", false).unwrap();
                    let mut element = formula_element(
                        formula,
                        20.,
                        window.text_style(),
                        r"\(\frac{x}{y}\)".into(),
                    )
                    .into_any_element();
                    let measured = element.layout_as_root(
                        size(AvailableSpace::MaxContent, AvailableSpace::MaxContent),
                        window,
                        cx,
                    );
                    assert!(measured.width > px(0.) && measured.height > px(0.));
                    assert!(!cx.has_global::<MathRasters>());
                    checked.store(true, std::sync::atomic::Ordering::Relaxed);
                },
                |_, _, _, _| (),
            )
            .w(px(500.))
            .h(px(500.))
        }
    }
    let checked = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    let window = cx.update(|cx| {
        gpui_kit::init(cx);
        gpui_kit::open_window(WindowOptions::default(), cx, |_, cx| {
            cx.new(|_| Measure(checked.clone()))
        })
        .unwrap()
        .0
    });
    cx.update_window(window, |_, window, cx| window.render_frame(cx))
        .unwrap();
    assert!(checked.load(std::sync::atomic::Ordering::Relaxed));
}

#[gpui_kit::test]
fn currency_and_invalid_math_have_native_copy_fallbacks(cx: &mut TestAppContext) {
    let source = "Costs $5-$10 and $5 and $10. Bad \\(\\undefinedcommand\\).\n\n\\[\n\\undefinedcommand\n\\]\n\n\\[\nunclosed";
    let (window, state) = setup(source, SelectionFormat::Plain, cx);
    frame(window, cx);
    let copied = copy(window, &state, cx);
    assert!(copied.contains("Costs $5-$10 and $5 and $10."));
    assert!(copied.contains(r"\(\undefinedcommand\)"));
    assert!(copied.contains("unclosed"));
    assert_eq!(copied.matches(r"\undefinedcommand").count(), 2);
    // Only the invalid inline is claimed; neither code fallback allocates images.
    cx.update(|cx| assert!(!cx.has_global::<MathRasters>()));
}

#[gpui_kit::test]
fn streaming_copy_matches_full_parse(cx: &mut TestAppContext) {
    let (window, state) = setup("", SelectionFormat::Source, cx);
    frame(window, cx);
    let chars: Vec<_> = PROOF.chars().collect();
    for chunk in chars.chunks(7) {
        state.update(cx, |state, cx| {
            state.push_str(&chunk.iter().collect::<String>(), cx)
        });
        cx.run_until_parked();
    }
    frame(window, cx);
    assert_eq!(copy(window, &state, cx), PROOF.trim());
}

#[test]
fn displays_require_a_real_closer() {
    for source in [r"\[x\]", "\\[\nx\\]", "$$x$$", "$$\nx\n$$", "$$$\nx\n$$$"] {
        assert!(closed_display(source));
    }
    for source in ["\\[\nx", "$$\nx", "$$", "$$\nx$$", "\\[\nx\\\\]"] {
        assert!(!closed_display(source));
    }
}

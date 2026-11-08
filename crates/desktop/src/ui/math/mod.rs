//! Native, source-preserving Markdown math. Everything here is desktop-only.
mod cache;
mod raster;
mod typeset;

use cache::MathRasters;
use gpui_kit::{
    base::text::{
        InlineElement, InlineRenderContext, MarkdownExtensions, MarkdownNode, MarkdownParseContext,
        MarkdownPlugin, markdown_ast,
    },
    prelude::*,
    *,
};
use std::sync::{Arc, LazyLock};
use typeset::{Formula, typeset};

const MATH_EM_RATIO: f32 = 1.21;
const MAX_INLINE_WIDTH: f32 = 480.;
const INLINE_PLUGIN: &str = "zevria-math-inline";
const DISPLAY_PLUGIN: &str = "zevria-math-display";

/// Constructed once: stable registration/revision avoids reparsing every frame.
pub fn extensions() -> MarkdownExtensions {
    static EXTENSIONS: LazyLock<MarkdownExtensions> = LazyLock::new(|| {
        MarkdownExtensions::default()
            .plugin(InlineMath)
            .plugin(DisplayMath)
    });
    EXTENSIONS.clone()
}
struct MathData(Option<Arc<Formula>>);
struct InlineMath;
impl MarkdownPlugin for InlineMath {
    fn name(&self) -> &str {
        INLINE_PLUGIN
    }
    fn parse(
        &self,
        node: &markdown_ast::Node,
        cx: &MarkdownParseContext<'_>,
    ) -> Option<MarkdownNode> {
        let markdown_ast::Node::InlineMath(math) = node else {
            return None;
        };
        let source = cx.node_source(node)?.to_owned();
        Some(
            MarkdownNode::new(INLINE_PLUGIN, MathData(typeset(&math.value, false)))
                .text(source.clone())
                .markdown(source)
                .accessibility_label(math.value.clone()),
        )
    }
    fn render_inline(
        &self,
        node: &MarkdownNode,
        context: &InlineRenderContext,
        window: &mut Window,
        _cx: &mut App,
    ) -> Option<InlineElement> {
        // None selects GPUI's atomic, proportionally fitted source-text fallback.
        let formula = node.data::<MathData>()?.0.as_ref()?.clone();
        let mut em = f32::from(context.font_size()) * MATH_EM_RATIO;
        em *= (MAX_INLINE_WIDTH / (formula.width as f32 * em)).min(1.);
        if !raster::fits_layout(&formula, em, window.scale_factor()) {
            return None;
        }
        let baseline = px(formula.height as f32 * em);
        Some(
            InlineElement::new(formula_element(
                formula,
                em,
                context.text_style().clone(),
                node.as_text().to_owned(),
            ))
            .with_baseline(baseline),
        )
    }
}
struct DisplayMath;
impl MarkdownPlugin for DisplayMath {
    fn is_block(&self) -> bool {
        true
    }
    fn name(&self) -> &str {
        DISPLAY_PLUGIN
    }
    fn parse(
        &self,
        node: &markdown_ast::Node,
        cx: &MarkdownParseContext<'_>,
    ) -> Option<MarkdownNode> {
        let markdown_ast::Node::Math(math) = node else {
            return None;
        };
        let source = cx.node_source(node)?;
        if !closed_display(source) {
            return None;
        }
        let formula = typeset(&math.value, true)?;
        Some(
            MarkdownNode::new(DISPLAY_PLUGIN, MathData(Some(formula)))
                .text(source.to_owned())
                .markdown(source.to_owned())
                .accessibility_label(math.value.clone()),
        )
    }
    fn render(&self, node: &MarkdownNode, window: &mut Window, _cx: &mut App) -> impl IntoElement {
        let formula = node
            .data::<MathData>()
            .and_then(|d| d.0.as_ref())
            .expect("claimed display is typeset")
            .clone();
        let style = window.text_style();
        let em = f32::from(style.font_size.to_pixels(window.rem_size())) * MATH_EM_RATIO;
        let width = px(formula.width as f32 * em);
        if !raster::fits_layout(&formula, em, window.scale_factor()) {
            return div()
                .font_family(super::style::MONO_FONT)
                .child(node.as_text().to_owned())
                .into_any_element();
        }
        div()
            .id((
                DISPLAY_PLUGIN,
                node.source_range()
                    .map_or(formula.fingerprint, |r| r.start as u64),
            ))
            .w_full()
            .py(px(6.))
            .overflow_x_scroll()
            .restrict_scroll_to_axis()
            .child(
                div()
                    .min_w_full()
                    .w(width)
                    .flex()
                    .items_center()
                    .justify_center()
                    .child(formula_element(
                        formula,
                        em,
                        style,
                        node.as_text().to_owned(),
                    )),
            )
            .into_any_element()
    }
}

fn closed_display(source: &str) -> bool {
    let source = source.trim_end();
    if source.starts_with(r"\[") && source.ends_with(r"\]") && source.len() >= 4 {
        // The slash before `]` must be unpaired, not e.g. the last slash in `\\]`.
        return source[..source.len() - 1]
            .bytes()
            .rev()
            .take_while(|b| *b == b'\\')
            .count()
            % 2
            == 1;
    }
    let open = source.bytes().take_while(|b| *b == b'$').count();
    if open < 2 {
        return false;
    }
    if let Some((_, last)) = source.rsplit_once('\n') {
        let last = last.trim();
        last.len() >= open && last.bytes().all(|b| b == b'$')
    } else {
        source.len() >= open + 2 && source.ends_with("$$")
    }
}

/// A fixed em-metric box. Creating and measuring it never allocates a raster.
fn formula_element(
    formula: Arc<Formula>,
    em: f32,
    style: TextStyle,
    source: String,
) -> impl IntoElement {
    let width = px(formula.width as f32 * em);
    // GPUI rounds native element layouts to pixels. Round the box outward so
    // zero-depth formulas (AB, \\square) cannot acquire a measured height below
    // their explicit alphabetic baseline and spuriously become source text.
    let height = px(((formula.height + formula.depth) as f32 * em).ceil());
    canvas(
        |_, _, _| (),
        move |bounds, (), window, cx| {
            let scale = window.scale_factor();
            let color: Rgba = style.color.into();
            let rgba = [color.r, color.g, color.b, color.a];
            let origin = point(
                px((f32::from(bounds.origin.x) * scale).round() / scale),
                px((f32::from(bounds.origin.y) * scale).round() / scale),
            );
            if let Some(raster) = MathRasters::paint_raster(&formula, em, scale, rgba, window, cx) {
                let image_bounds = Bounds {
                    origin: origin + point(px(raster.offset[0]), px(raster.offset[1])),
                    size: size(px(raster.width), px(raster.height)),
                };
                let _ = window.paint_image(
                    image_bounds,
                    image_bounds,
                    Default::default(),
                    raster.image,
                    0,
                    false,
                );
            } else {
                // Extreme device scales/ink overhangs may exceed the raster budget.
                // Never leave a successful layout blank in that case.
                // Preserve delimiters even on this last-resort ink-budget fallback.
                // As with GPUI's inline source fallback, fit rather than overflow.
                let text: SharedString = source.replace(['\r', '\n'], " ").into();
                let line = window.text_system().shape_line(
                    text.clone(),
                    px(em),
                    &[style.to_run(text.len())],
                    None,
                );
                let fit = (width / line.width.max(px(1.))).min(1.);
                paint_native_text(
                    &text,
                    origin + point(px(0.), px(formula.height as f32 * em)),
                    em * fit,
                    &style,
                    window,
                    cx,
                );
                return;
            }
            for glyph in &formula.fallback {
                let mut style = style.clone();
                let [r, g, b, a] = raster::effective_color(glyph.color, rgba);
                style.color = Rgba { r, g, b, a }.into();
                let baseline = origin + point(px(glyph.x as f32 * em), px(glyph.y as f32 * em));
                paint_native_text(
                    &glyph.character.to_string(),
                    baseline,
                    em * glyph.scale as f32,
                    &style,
                    window,
                    cx,
                );
            }
        },
    )
    .w(width)
    .h(height)
    .flex_shrink_0()
}

fn paint_native_text(
    text: &str,
    baseline: Point<Pixels>,
    em: f32,
    style: &TextStyle,
    window: &mut Window,
    cx: &mut App,
) {
    let text: SharedString = text.replace(['\r', '\n'], " ").into();
    let line =
        window
            .text_system()
            .shape_line(text.clone(), px(em), &[style.to_run(text.len())], None);
    let line_height = line.ascent + line.descent;
    let _ = line.paint(
        baseline - point(px(0.), line.ascent),
        line_height,
        TextAlign::Left,
        None,
        window,
        cx,
    );
}

#[cfg(test)]
mod tests;

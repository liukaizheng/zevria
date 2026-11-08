//! Direct KaTeX outline rasterization. No SVG atlas, PNG codec, or font service.
use super::typeset::{Formula, INHERITED_INK};
use ratex_font::{FontId, katex_ttf_glyph_char};
use ratex_types::{color::Color as TexColor, display_item::DisplayItem, path_command::PathCommand};
use std::{borrow::Cow, collections::HashMap, sync::LazyLock};
use tiny_skia::{
    Color as RasterColor, FillRule, Paint, Path, PathBuilder, Pixmap, Rect, Stroke, Transform,
};
use ttf_parser::{Face, GlyphId, OutlineBuilder};

pub(super) const MAX_RASTER_SIDE: u32 = 4096;
pub(super) const MAX_RASTER_PIXELS: u64 = 4_000_000;

static FONT_BYTES: LazyLock<HashMap<FontId, Cow<'static, [u8]>>> = LazyLock::new(|| {
    use FontId::*;
    [
        AmsRegular,
        CaligraphicRegular,
        FrakturRegular,
        FrakturBold,
        MainBold,
        MainBoldItalic,
        MainItalic,
        MainRegular,
        MathBoldItalic,
        MathItalic,
        SansSerifBold,
        SansSerifItalic,
        SansSerifRegular,
        ScriptRegular,
        Size1Regular,
        Size2Regular,
        Size3Regular,
        Size4Regular,
        TypewriterRegular,
    ]
    .into_iter()
    .filter_map(|font| {
        Some((
            font,
            ratex_katex_fonts::ttf_bytes(&format!("KaTeX_{}.ttf", font.as_str()))?,
        ))
    })
    .collect()
});
static FONTS: LazyLock<HashMap<FontId, Face<'static>>> = LazyLock::new(|| {
    FONT_BYTES
        .iter()
        .filter_map(|(font, bytes)| Some((*font, Face::parse(bytes, 0).ok()?)))
        .collect()
});

/// CJK/emoji faces deliberately are not loaded. GPUI owns system font fallback.
pub(super) fn glyph(font: &str, code: u32) -> Option<(&'static Face<'static>, GlyphId)> {
    let font = FontId::parse(font)?;
    let face = FONTS.get(&font)?;
    Some((face, face.glyph_index(katex_ttf_glyph_char(font, code))?))
}

pub(super) struct Raster {
    pub width: u32,
    pub height: u32,
    /// Straight BGRA: GPUI's decoded bitmap format, not tiny-skia's premultiplied RGBA.
    pub bgra: Vec<u8>,
    /// Logical-pixel ink origin relative to the formula's layout box.
    pub offset: [f32; 2],
}
struct Outline(PathBuilder);
impl OutlineBuilder for Outline {
    fn move_to(&mut self, x: f32, y: f32) {
        self.0.move_to(x, y);
    }
    fn line_to(&mut self, x: f32, y: f32) {
        self.0.line_to(x, y);
    }
    fn quad_to(&mut self, x1: f32, y1: f32, x: f32, y: f32) {
        self.0.quad_to(x1, y1, x, y);
    }
    fn curve_to(&mut self, x1: f32, y1: f32, x2: f32, y2: f32, x: f32, y: f32) {
        self.0.cubic_to(x1, y1, x2, y2, x, y);
    }
    fn close(&mut self) {
        self.0.close();
    }
}
struct Ink {
    path: Path,
    color: TexColor,
    fill: FillRule,
    stroke: bool,
}

pub(super) fn effective_color(color: TexColor, inherited: [f32; 4]) -> [f32; 4] {
    if color == INHERITED_INK {
        inherited
    } else {
        [color.r, color.g, color.b, color.a]
    }
}

/// A cheap, allocation-free lower bound for rejecting impossible device sizes.
/// Actual ink overhangs are checked again by the rasterizer before allocation.
pub(super) fn fits_layout(formula: &Formula, em: f32, scale: f32) -> bool {
    let width = (formula.width * f64::from(em) * f64::from(scale)).ceil() + 2.;
    let height = ((formula.height + formula.depth) * f64::from(em) * f64::from(scale)).ceil() + 2.;
    width.is_finite()
        && height.is_finite()
        && width > 0.
        && height > 0.
        && width <= f64::from(MAX_RASTER_SIDE)
        && height <= f64::from(MAX_RASTER_SIDE)
        && width * height <= MAX_RASTER_PIXELS as f64
}

/// Rasterization is paint-only. Measurements use Formula's em metrics directly.
pub(super) fn rasterize(
    formula: &Formula,
    em_px: f32,
    scale: f32,
    rgba: [f32; 4],
) -> Option<Raster> {
    if !em_px.is_finite()
        || !scale.is_finite()
        || em_px <= 0.
        || scale <= 0.
        || !fits_layout(formula, em_px, scale)
        || !rgba.iter().all(|x| x.is_finite())
    {
        return None;
    }
    let em = em_px * scale;
    let mut ink = Vec::new();
    for item in &formula.items {
        match item {
            DisplayItem::GlyphPath {
                x,
                y,
                scale,
                font,
                char_code,
                color,
            } => {
                let (face, glyph) = glyph(font, *char_code)?;
                let mut outline = Outline(PathBuilder::new());
                // Spaces have a cmap entry but no outline.
                if face.outline_glyph(glyph, &mut outline).is_some() {
                    let k = em * *scale as f32 / face.units_per_em() as f32;
                    let path = outline.0.finish()?.transform(Transform::from_row(
                        k,
                        0.,
                        0.,
                        -k,
                        *x as f32 * em,
                        *y as f32 * em,
                    ))?;
                    ink.push(Ink {
                        path,
                        color: *color,
                        fill: FillRule::Winding,
                        stroke: false,
                    });
                }
            }
            DisplayItem::Line {
                x,
                y,
                width,
                thickness,
                color,
                dashed,
            } => {
                let thickness = (*thickness as f32 * em).max(1.);
                let x = *x as f32 * em;
                let y = *y as f32 * em - thickness / 2.;
                let width = *width as f32 * em;
                if *dashed {
                    let dash = thickness * 3.;
                    let count = (width / (dash * 2.)).ceil() as usize;
                    if count > ratex_layout::MAX_LAYOUT_OUTPUT_ITEMS {
                        return None;
                    }
                    for i in 0..count {
                        let start = i as f32 * dash * 2.;
                        rectangle(
                            &mut ink,
                            x + start,
                            y,
                            dash.min(width - start),
                            thickness,
                            *color,
                        )?;
                    }
                } else {
                    rectangle(&mut ink, x, y, width, thickness, *color)?;
                }
            }
            DisplayItem::Rect {
                x,
                y,
                width,
                height,
                color,
            } => {
                rectangle(
                    &mut ink,
                    *x as f32 * em,
                    *y as f32 * em,
                    *width as f32 * em,
                    *height as f32 * em,
                    *color,
                )?;
            }
            DisplayItem::Path {
                x,
                y,
                commands,
                fill,
                color,
            } => {
                let mut builder = PathBuilder::new();
                // KaTeX filled paths are independent subpaths (overlapping
                // delimiter parts must not cut holes in each other).
                let transform = Transform::from_row(em, 0., 0., em, *x as f32 * em, *y as f32 * em);
                for command in commands {
                    if matches!(command, PathCommand::MoveTo { .. }) && !builder.is_empty() {
                        finish_path(&mut ink, builder, transform, *color, *fill)?;
                        builder = PathBuilder::new();
                    }
                    match *command {
                        PathCommand::MoveTo { x, y } => builder.move_to(x as f32, y as f32),
                        PathCommand::LineTo { x, y } => builder.line_to(x as f32, y as f32),
                        PathCommand::QuadTo { x1, y1, x, y } => {
                            builder.quad_to(x1 as f32, y1 as f32, x as f32, y as f32)
                        }
                        PathCommand::CubicTo {
                            x1,
                            y1,
                            x2,
                            y2,
                            x,
                            y,
                        } => builder.cubic_to(
                            x1 as f32, y1 as f32, x2 as f32, y2 as f32, x as f32, y as f32,
                        ),
                        PathCommand::Close => builder.close(),
                    }
                }
                if !builder.is_empty() {
                    finish_path(&mut ink, builder, transform, *color, *fill)?;
                }
            }
        }
    }
    let mut bounds = [
        0f32,
        0f32,
        formula.width as f32 * em,
        (formula.height + formula.depth) as f32 * em,
    ];
    for item in &ink {
        let b = item.path.bounds();
        let margin = if item.stroke { 0.75 } else { 0. };
        bounds[0] = bounds[0].min(b.left() - margin);
        bounds[1] = bounds[1].min(b.top() - margin);
        bounds[2] = bounds[2].max(b.right() + margin);
        bounds[3] = bounds[3].max(b.bottom() + margin);
    }
    if !bounds.iter().all(|x| x.is_finite()) {
        return None;
    }
    let left = bounds[0].floor() - 1.;
    let top = bounds[1].floor() - 1.;
    let width = (bounds[2].ceil() + 1. - left) as u32;
    let height = (bounds[3].ceil() + 1. - top) as u32;
    if width > MAX_RASTER_SIDE
        || height > MAX_RASTER_SIDE
        || u64::from(width) * u64::from(height) > MAX_RASTER_PIXELS
    {
        return None;
    }
    let mut pixmap = Pixmap::new(width, height)?;
    let transform = Transform::from_translate(-left, -top);
    for item in ink {
        let [r, g, b, a] = effective_color(item.color, rgba);
        let mut paint = Paint::default();
        paint.set_color(RasterColor::from_rgba(r, g, b, a)?);
        paint.anti_alias = true;
        if item.stroke {
            pixmap.stroke_path(
                &item.path,
                &paint,
                &Stroke {
                    width: 1.5,
                    ..Stroke::default()
                },
                transform,
                None,
            );
        } else {
            pixmap.fill_path(&item.path, &paint, item.fill, transform, None);
        }
    }
    let mut bgra = Vec::with_capacity(pixmap.data().len());
    for pixel in pixmap.pixels() {
        let color = pixel.demultiply();
        bgra.extend_from_slice(&[color.blue(), color.green(), color.red(), color.alpha()]);
    }
    Some(Raster {
        width,
        height,
        bgra,
        offset: [left / scale, top / scale],
    })
}
fn rectangle(
    ink: &mut Vec<Ink>,
    x: f32,
    y: f32,
    width: f32,
    height: f32,
    color: TexColor,
) -> Option<()> {
    if width == 0. || height == 0. {
        return Some(());
    }
    ink.push(Ink {
        path: PathBuilder::from_rect(Rect::from_xywh(x, y, width, height)?),
        color,
        fill: FillRule::Winding,
        stroke: false,
    });
    Some(())
}
fn finish_path(
    ink: &mut Vec<Ink>,
    builder: PathBuilder,
    transform: Transform,
    color: TexColor,
    fill: bool,
) -> Option<()> {
    ink.push(Ink {
        path: builder.finish()?.transform(transform)?,
        color,
        fill: FillRule::EvenOdd,
        stroke: !fill,
    });
    Some(())
}

#[cfg(test)]
mod tests {
    use super::super::typeset::typeset;
    use super::*;

    #[test]
    fn fixture_rasters_have_transparent_margins_and_finite_offsets() {
        for (tex, display, _) in super::super::tests::fixture_formulas() {
            let f = typeset(&tex, display).unwrap();
            let r = rasterize(&f, 19.4, 2., [1.; 4]).unwrap();
            assert!(r.width as f64 >= f.width * 38.8);
            assert!(r.height as f64 >= (f.height + f.depth) * 38.8);
            assert!(r.offset.iter().all(|x| x.is_finite()));
            assert!(r.bgra.as_chunks::<4>().0.iter().any(|p| p[3] == 0));
            assert!(r.bgra.as_chunks::<4>().0.iter().any(|p| p[3] > 0));
        }
    }
    #[test]
    fn inherited_tint_and_explicit_color_are_straight_bgra() {
        let f = typeset(r"x+\color{red}{y}", false).unwrap();
        let r = rasterize(&f, 20., 2., [0., 0., 1., 0.5]).unwrap();
        assert!(
            r.bgra
                .as_chunks::<4>()
                .0
                .iter()
                .any(|p| p[0] == 255 && p[2] == 0 && p[3] > 0 && p[3] <= 128)
        );
        assert!(
            r.bgra
                .as_chunks::<4>()
                .0
                .iter()
                .any(|p| p[0] == 0 && p[2] == 255 && p[3] > 0)
        );
    }
    #[test]
    fn explicit_black_and_colorbox_do_not_inherit_the_foreground() {
        let f = typeset(
            r"x+\color{black}{y}+\colorbox{black}{\color{red}{z}}",
            false,
        )
        .unwrap();
        let r = rasterize(&f, 20., 2., [1.; 4]).unwrap();
        assert!(
            r.bgra
                .as_chunks::<4>()
                .0
                .iter()
                .any(|p| p[0] == 0 && p[1] == 0 && p[2] == 0 && p[3] > 0)
        );
        assert!(
            r.bgra
                .as_chunks::<4>()
                .0
                .iter()
                .any(|p| p[0] == 255 && p[1] == 255 && p[2] == 255 && p[3] > 0)
        );
        assert!(
            r.bgra
                .as_chunks::<4>()
                .0
                .iter()
                .any(|p| p[0] == 0 && p[1] == 0 && p[2] == 255 && p[3] > 0)
        );
    }
    #[test]
    fn raster_limits_and_invalid_scales() {
        let f = typeset("x", false).unwrap();
        for em in [0., -1., f32::NAN, f32::INFINITY, 10_000.] {
            assert!(rasterize(&f, em, 2., [1.; 4]).is_none());
        }
        let f = typeset(r"\rule{100em}{100em}", true).unwrap();
        assert!(rasterize(&f, 30., 1., [1.; 4]).is_none()); // area, not side limit
    }
    #[test]
    fn raster_budget_boundaries_are_inclusive() {
        let thin = typeset(r"\rule{1em}{0.001em}", true).unwrap();
        assert_eq!(
            rasterize(&thin, 4094., 1., [1.; 4]).unwrap().width,
            MAX_RASTER_SIDE
        );
        assert!(rasterize(&thin, 4095., 1., [1.; 4]).is_none());
        let square = typeset(r"\rule{1em}{1em}", true).unwrap();
        let r = rasterize(&square, 1998., 1., [1.; 4]).unwrap();
        assert_eq!(u64::from(r.width) * u64::from(r.height), MAX_RASTER_PIXELS);
        assert!(rasterize(&square, 1999., 1., [1.; 4]).is_none());
    }
    #[test]
    fn checked_in_ink_signatures() {
        // Exact-resolution coverage/ink bounds pin baseline positioning, rules,
        // superscripts, boxes, large delimiters and multi-part path rendering.
        let mut actual = Vec::new();
        for tex in [
            r"x^2",
            r"\frac{a}{b}",
            r"\boxed{\sum_{i=1}^n i}",
            r"\left(\sqrt{\frac{x}{y}}\right)",
        ] {
            let f = typeset(tex, true).unwrap();
            let r = rasterize(&f, 20., 2., [1.; 4]).unwrap();
            let mut sum = 0u64;
            let mut ink = [u32::MAX, u32::MAX, 0, 0];
            for (i, pixel) in r.bgra.as_chunks::<4>().0.iter().enumerate() {
                sum += u64::from(pixel[3]);
                if pixel[3] > 0 {
                    let x = i as u32 % r.width;
                    let y = i as u32 / r.width;
                    ink = [ink[0].min(x), ink[1].min(y), ink[2].max(x), ink[3].max(y)];
                }
            }
            actual.push((r.width, r.height, sum, ink));
        }
        assert_eq!(
            actual,
            vec![
                (41, 39, 55_232, [2, 1, 36, 36]),
                (33, 76, 85_085, [5, 1, 26, 74]),
                (108, 153, 444_504, [1, 1, 106, 151]),
                (138, 123, 409_051, [10, 1, 127, 120]),
            ]
        );
    }
    #[test]
    fn overhang_is_painted_without_changing_advance() {
        let f = typeset(r"a+\mathllap{xxxx}b", false).unwrap();
        let r = rasterize(&f, 20., 2., [1.; 4]).unwrap();
        assert!(r.offset[0] < -0.5);
        assert!(r.width as f64 > f.width * 40.);
    }
}

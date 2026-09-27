//! Deterministic OKLCH search, version 1.
//!
//! Ordering is part of generator provenance: light foreground then dark;
//! surface spacings .035/.018/.008; role hue offsets 0/-12/+12; lightness
//! .10..=.98 in .02 steps; chroma .07/.11/.15. A stable 24-wide beam selects
//! four accents with hard pairwise CVD gates. Equal scores keep the earlier
//! candidate. No random seed, clock, generator-on-load, or relaxed fallback.
use palette::{LinSrgb, Oklab, Oklch, Srgb, convert::FromColorUnclamped};

use super::{
    definition::{HexRgb, ReferencePalette, SCHEMA_VERSION, ThemeDefinition},
    validation::{contrast, distance, rgb, simulations, validate_theme},
};

pub const GENERATOR_VERSION: &str = "oklch-search-1";
const BEAM_WIDTH: usize = 24;
const SPACINGS: [f64; 3] = [0.035, 0.018, 0.008];

/// Chroma-only bisection into sRGB, holding L/H. Quantization happens only after
/// a fully in-gamut linear RGB value has been found (28 fixed iterations).
fn mapped_linear(l: f64, c: f64, h: f64) -> LinSrgb<f64> {
    let convert = |chroma| LinSrgb::from_color_unclamped(Oklch::new(l, chroma, h));
    let in_gamut = |rgb: LinSrgb<f64>| {
        [rgb.red, rgb.green, rgb.blue]
            .into_iter()
            .all(|v| (0.0..=1.0).contains(&v))
    };
    let full = convert(c);
    if in_gamut(full) {
        return full;
    }
    let (mut low, mut high) = (0.0, c);
    let mut result = convert(0.0);
    for _ in 0..28 {
        let mid = (low + high) / 2.0;
        let candidate = convert(mid);
        if in_gamut(candidate) {
            low = mid;
            result = candidate;
        } else {
            high = mid;
        }
    }
    result
}

fn mapped(l: f64, c: f64, h: f64) -> HexRgb {
    let encoded: Srgb<u8> = Srgb::<f64>::from_linear(mapped_linear(l, c, h)).into_format();
    HexRgb(encoded.red, encoded.green, encoded.blue)
}

fn passes(color: HexRgb, surfaces: &[HexRgb], minimum: f64) -> bool {
    surfaces
        .iter()
        .all(|surface| contrast(color.color(), surface.color()) >= minimum)
}

#[derive(Clone)]
struct Candidate {
    color: HexRgb,
    simulated: [Oklab<f64>; 3],
    score: f64,
}

fn candidates(h: f64, target: f64, light: bool, surfaces: &[HexRgb]) -> Vec<Candidate> {
    let mut candidates = Vec::new();
    for hue_offset in [0.0, -12.0, 12.0] {
        for step in 5..=49 {
            let l = step as f64 * 0.02;
            if (light && l < 0.58) || (!light && l > 0.56) {
                continue;
            }
            for c in [0.07, 0.11, 0.15] {
                let color = mapped(l, c, h + hue_offset);
                if !passes(color, surfaces, 4.5) {
                    continue;
                }
                let actual = Oklch::from_color_unclamped(rgb(color));
                candidates.push(Candidate {
                    color,
                    simulated: simulations(color),
                    score: -(l - target).abs() * 2.0 - hue_offset.abs() * 0.0005
                        + actual.chroma * 0.25,
                });
            }
        }
    }
    candidates
}

fn group(
    names: [&str; 4],
    hues: [f64; 4],
    targets: [f64; 4],
    light: bool,
    surfaces: &[HexRgb],
    floor: f64,
) -> Result<([HexRgb; 4], f64), String> {
    let mut beam: Vec<(Vec<Candidate>, f64)> = vec![(Vec::new(), 0.0)];
    for (index, (hue, target)) in hues.into_iter().zip(targets).enumerate() {
        let choices = candidates(hue, target, light, surfaces);
        if choices.is_empty() {
            return Err(format!(
                "{} on canvas/panel/overlay/diff backgrounds: no candidate meets 4.5:1 contrast",
                names[index]
            ));
        }
        let mut next = Vec::new();
        let mut closest_rejection = (0.0_f64, "");
        for (selected, score) in &beam {
            for candidate in &choices {
                let (separation, paired) = selected
                    .iter()
                    .enumerate()
                    .map(|(i, other)| {
                        (
                            candidate
                                .simulated
                                .into_iter()
                                .zip(other.simulated)
                                .map(|(a, b)| distance(a, b))
                                .fold(1.0_f64, f64::min),
                            names[i],
                        )
                    })
                    .min_by(|a, b| a.0.total_cmp(&b.0))
                    .unwrap_or((1.0, ""));
                if separation < floor {
                    if separation > closest_rejection.0 {
                        closest_rejection = (separation, paired);
                    }
                    continue;
                }
                let mut selected = selected.clone();
                selected.push(candidate.clone());
                next.push((
                    selected,
                    score + candidate.score + separation.min(0.16) * 0.3,
                ));
            }
        }
        // Stable sort supplies the documented traversal-order tie break.
        next.sort_by(|a, b| b.1.total_cmp(&a.1));
        next.truncate(BEAM_WIDTH);
        beam = next;
        if beam.is_empty() {
            return Err(format!(
                "{}/{} full-severity CVD separation: closest beam extension {:.4} < {floor:.3} OKLab distance",
                names[index], closest_rejection.1, closest_rejection.0
            ));
        }
    }
    let (selected, score) = beam
        .into_iter()
        .next()
        .expect("four nonempty beam extensions");
    Ok((std::array::from_fn(|i| selected[i].color), score))
}

fn tone(h: f64, c: f64, target: f64, surfaces: &[HexRgb], minimum: f64) -> Option<HexRgb> {
    // 999 fixed lightness samples, score by closeness to the requested tier.
    (1..1000)
        .map(|i| {
            let l = i as f64 / 1000.0;
            (mapped(l, c, h), (l - target).abs())
        })
        .filter(|(color, _)| passes(*color, surfaces, minimum))
        .min_by(|a, b| a.1.total_cmp(&b.1))
        .map(|(color, _)| color)
}

pub fn generate_theme(background: HexRgb) -> anyhow::Result<ThemeDefinition> {
    let seed = Oklch::from_color_unclamped(rgb(background));
    // Near-achromatic hue is numerically unstable; use a documented blue seed.
    let hue = if seed.chroma < 0.01 {
        255.0
    } else {
        seed.hue.into_positive_degrees()
    };
    let related_chroma = seed.chroma.min(0.02);
    let mut best: Option<(ThemeDefinition, f64)> = None;
    let mut last_failure =
        String::from("workflow/feedback contrast and full-severity CVD separation");
    for light in [true, false] {
        let extreme = if light {
            HexRgb(255, 255, 255)
        } else {
            HexRgb(0, 0, 0)
        };
        if !passes(extreme, &[background], 4.5) {
            continue;
        }
        for spacing in SPACINGS {
            let direction = if light { 1.0 } else { -1.0 };
            let surface =
                |delta: f64, c, h| mapped((seed.l + direction * delta).clamp(0.001, 0.999), c, h);
            let panel = surface(spacing, related_chroma, hue);
            let overlay = surface(spacing * 1.5, related_chroma, hue);
            let feedback_shift = (hue - 255.0).to_radians().sin() * 7.0;
            let success_hue = 140.0 + feedback_shift;
            let error_hue = 25.0 + feedback_shift;
            let addition = surface(spacing, 0.025, success_hue);
            let deletion = surface(spacing, 0.025, error_hue);
            let surfaces = [background, panel, overlay, addition, deletion];
            let (workflow, workflow_score) = match group(
                [
                    "workflow.build",
                    "workflow.plan",
                    "workflow.review",
                    "workflow.explore",
                ],
                [hue - 80.0, hue - 5.0, hue + 175.0, hue + 45.0],
                if light {
                    [0.80, 0.70, 0.85, 0.94]
                } else {
                    [0.36, 0.49, 0.26, 0.14]
                },
                light,
                &surfaces,
                0.08,
            ) {
                Ok(group) => group,
                Err(failure) => {
                    last_failure = failure;
                    continue;
                }
            };
            let (feedback, feedback_score) = match group(
                [
                    "feedback.success",
                    "feedback.warning",
                    "feedback.error",
                    "feedback.info",
                ],
                [
                    success_hue,
                    75.0 + feedback_shift,
                    error_hue,
                    245.0 + feedback_shift,
                ],
                if light {
                    [0.81, 0.95, 0.70, 0.78]
                } else {
                    [0.35, 0.17, 0.49, 0.42]
                },
                light,
                &surfaces,
                0.075,
            ) {
                Ok(group) => group,
                Err(failure) => {
                    last_failure = failure;
                    continue;
                }
            };
            let target = |l| if light { l } else { 1.0 - l };
            let accent = |h, l| tone(h, 0.085, target(l), &surfaces, 4.5);
            // Build the entire graph together; no unchecked per-component colors.
            let Some(palette) = (|| {
                Some(ReferencePalette {
                    canvas: background,
                    panel,
                    overlay,
                    border: surface(spacing + 0.12, 0.02, hue),
                    border_strong: tone(hue, 0.025, target(0.67), &surfaces[..3], 3.0)?,
                    selection: tone(hue - 5.0, 0.075, target(0.72), &surfaces[..3], 4.5)?,
                    text_primary: tone(hue, 0.008, target(0.985), &surfaces, 4.5)?,
                    text_secondary: tone(hue, 0.015, target(0.85), &surfaces, 4.5)?,
                    text_muted: tone(hue, 0.02, target(0.74), &surfaces, 4.5)?,
                    build: workflow[0],
                    plan: workflow[1],
                    review: workflow[2],
                    explore: workflow[3],
                    you: accent(hue - 80.0, 0.78)?,
                    assistant: accent(hue + 50.0, 0.78)?,
                    system: accent(hue + 175.0, 0.78)?,
                    tools: accent(hue - 40.0, 0.80)?,
                    success: feedback[0],
                    warning: feedback[1],
                    error: feedback[2],
                    info: feedback[3],
                    diff_addition_background: addition,
                    diff_deletion_background: deletion,
                    syntax_keyword: accent(hue + 50.0, 0.80)?,
                    syntax_function: accent(hue - 35.0, 0.81)?,
                    syntax_string: accent(success_hue, 0.81)?,
                    syntax_constant: accent(hue + 175.0, 0.82)?,
                })
            })() else {
                continue;
            };
            let definition = ThemeDefinition {
                schema_version: SCHEMA_VERSION,
                generator_version: GENERATOR_VERSION.into(),
                source_background: background,
                palette,
            };
            if let Err(error) = validate_theme(&definition) {
                last_failure = error.to_string();
                continue;
            }
            let score = workflow_score
                + feedback_score
                + spacing * 4.0
                + contrast(definition.palette.text_primary.color(), background.color()) * 0.01;
            if best.as_ref().is_none_or(|(_, previous)| score > *previous) {
                best = Some((definition, score));
            }
        }
    }
    best.map(|(definition,_)| definition).ok_or_else(|| anyhow::anyhow!("no passing palette was found within the search budget ({GENERATOR_VERSION}, 2 polarities × 3 surface arrangements, beam width {BEAM_WIDTH}); unsatisfied checks: {last_failure}. Try a lighter or darker background; the supplied canvas was not changed"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn successful_backgrounds_are_deterministic_complete_and_valid() {
        for input in [
            "#000000", "#FFFFFF", "#282C34", "#1E1E2E", "#F5EEDF", "#102830", "#EDF4FF",
        ] {
            let background = input.parse().unwrap();
            let definition = generate_theme(background).unwrap_or_else(|e| panic!("{input}: {e}"));
            assert_eq!(definition, generate_theme(background).unwrap());
            assert_eq!(definition.palette.canvas, background);
            validate_theme(&definition).unwrap();
            let t = definition.palette.tokens();
            assert_eq!(t.content.heading, t.workflow.explore);
            assert_eq!(t.content.strong, t.syntax.r#type);
            assert_eq!(t.content.code, t.syntax.string);
            assert_eq!(t.surfaces.selection_foreground, t.surfaces.canvas);
            let serialized = toml::to_string(&definition).unwrap();
            assert_eq!(
                definition,
                toml::from_str::<ThemeDefinition>(&serialized).unwrap()
            );
            assert!(
                toml::from_str::<ThemeDefinition>(
                    &serialized.replace("[palette]", "unknown = 1\n[palette]")
                )
                .is_err()
            );
            assert!(
                toml::from_str::<ThemeDefinition>(&serialized.replace("canvas =", "unknown ="))
                    .is_err()
            );
            let mut saved = definition.clone();
            saved.generator_version = "future-provenance".into();
            validate_theme(&saved).unwrap();
            saved.schema_version += 1;
            assert!(validate_theme(&saved).is_err());
            saved.schema_version -= 1;
            saved.source_background = HexRgb(1, 2, 3);
            assert!(validate_theme(&saved).is_err());
        }
    }

    #[test]
    fn difficult_backgrounds_never_relax_checks() {
        for input in [
            "#808080", "#FF0000", "#00FF00", "#0000FF", "#A08060", "#777777",
        ] {
            match generate_theme(input.parse().unwrap()) {
                Ok(definition) => validate_theme(&definition).unwrap(),
                Err(error) => assert!(error.to_string().contains("within the search budget")),
            }
        }
    }

    #[test]
    fn gamut_mapping_holds_lightness_and_hue_without_channel_clipping() {
        for l in [0.1, 0.3, 0.5, 0.7, 0.9] {
            for h in (0..360).step_by(15) {
                let rgb = mapped_linear(l, 0.4, h as f64);
                assert!(
                    [rgb.red, rgb.green, rgb.blue]
                        .into_iter()
                        .all(|v| (0.0..=1.0).contains(&v))
                );
                let mapped = Oklch::from_color_unclamped(rgb);
                assert!((mapped.l - l).abs() < 1e-6);
                assert!(
                    ((mapped.hue.into_positive_degrees() - h as f64 + 180.0).rem_euclid(360.0)
                        - 180.0)
                        .abs()
                        < 1e-3
                );
                assert!(mapped.chroma <= 0.400001);
            }
        }
    }
}

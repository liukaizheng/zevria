//! Pure post-quantization validation shared by generation and loading.
use std::fmt;

use palette::{LinSrgb, Oklab, Srgb, convert::FromColorUnclamped};
use ratatui::style::Color;

use super::{
    ZevriaTheme,
    definition::{HexRgb, SCHEMA_VERSION, ThemeDefinition},
};

#[derive(Clone, Debug, PartialEq)]
pub struct ValidationFailure {
    pub check: String,
    pub actual: f64,
    pub minimum: f64,
}

#[derive(Clone, Debug, PartialEq)]
pub struct ValidationError(pub Vec<ValidationFailure>);

impl fmt::Display for ValidationError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        for (index, failure) in self.0.iter().enumerate() {
            if index > 0 {
                write!(f, "; ")?;
            }
            write!(
                f,
                "{}: {:.4} < {:.4}",
                failure.check, failure.actual, failure.minimum
            )?;
        }
        Ok(())
    }
}
impl std::error::Error for ValidationError {}

pub fn validate_theme(definition: &ThemeDefinition) -> anyhow::Result<()> {
    anyhow::ensure!(
        definition.schema_version == SCHEMA_VERSION,
        "unsupported theme schema version {}; supported version is {SCHEMA_VERSION}",
        definition.schema_version
    );
    anyhow::ensure!(
        definition.source_background == definition.palette.canvas,
        "source_background must exactly match palette.canvas"
    );
    validate_tokens(&definition.palette.tokens())?;
    Ok(())
}

pub(super) fn validate_tokens(t: &ZevriaTheme) -> Result<(), ValidationError> {
    let mut failures = Vec::new();
    let mut check = |left_name: &str, left, right_name: &str, right, minimum| {
        let actual = contrast(left, right);
        if actual < minimum {
            failures.push(ValidationFailure {
                check: format!("{left_name} on {right_name} contrast"),
                actual,
                minimum,
            });
        }
    };
    let surfaces = [
        ("canvas", t.surfaces.canvas),
        ("panel", t.surfaces.panel),
        ("overlay", t.surfaces.overlay),
    ];
    let text = [
        ("text.primary", t.text.primary),
        ("text.secondary", t.text.secondary),
        ("text.muted", t.text.muted),
    ];
    let workflows = [
        ("workflow.build", t.workflow.build),
        ("workflow.plan", t.workflow.plan),
        ("workflow.review", t.workflow.review),
        ("workflow.explore", t.workflow.explore),
    ];
    let feedback = [
        ("feedback.success", t.feedback.success),
        ("feedback.warning", t.feedback.warning),
        ("feedback.error", t.feedback.error),
        ("feedback.info", t.feedback.info),
    ];
    let syntax = [
        ("syntax.keyword", t.syntax.keyword),
        ("syntax.function", t.syntax.function),
        ("syntax.type", t.syntax.r#type),
        ("syntax.string", t.syntax.string),
        ("syntax.constant", t.syntax.constant),
        ("syntax.comment", t.syntax.comment),
        ("syntax.invalid", t.syntax.invalid),
    ];
    let roles = [
        ("roles.you", t.roles.you),
        ("roles.assistant", t.roles.assistant),
        ("roles.system", t.roles.system),
        ("roles.tools", t.roles.tools),
    ];
    // Content aliases are covered by their canonical workflow/feedback/syntax tokens.
    for (name, foreground) in text
        .into_iter()
        .chain(workflows)
        .chain(feedback)
        .chain(syntax)
        .chain(roles)
    {
        for (surface_name, surface) in surfaces {
            check(name, foreground, surface_name, surface, 4.5);
        }
    }
    check(
        "selection_foreground",
        t.surfaces.selection_foreground,
        "selection_background",
        t.surfaces.selection_background,
        4.5,
    );
    for (name, surface) in surfaces {
        check(
            "border_strong",
            t.surfaces.border_strong,
            name,
            surface,
            3.0,
        );
        check(
            "selection_background",
            t.surfaces.selection_background,
            name,
            surface,
            3.0,
        );
    }
    for (name, color) in workflows {
        check(name, color, "panel focus", t.surfaces.panel, 3.0);
    }
    for (name, bg) in [
        (
            "diff_addition_background",
            t.content.diff_addition_background,
        ),
        (
            "diff_deletion_background",
            t.content.diff_deletion_background,
        ),
    ] {
        for (fg_name, fg) in text.into_iter().chain(syntax).chain([
            ("diff_addition", t.content.diff_addition),
            ("diff_deletion", t.content.diff_deletion),
        ]) {
            check(fg_name, fg, name, bg, 4.5);
        }
    }
    for (group, minimum) in [(workflows, 0.08), (feedback, 0.075)] {
        for (i, (left_name, left)) in group.iter().enumerate() {
            for (right_name, right) in &group[i + 1..] {
                for (simulation, matrix) in SIMULATIONS {
                    let actual = distance(simulate(*left, matrix), simulate(*right, matrix));
                    if actual < minimum {
                        failures.push(ValidationFailure {
                            check: format!(
                                "{left_name}/{right_name} under {simulation} OKLab distance"
                            ),
                            actual,
                            minimum,
                        });
                    }
                }
            }
        }
    }
    if failures.is_empty() {
        Ok(())
    } else {
        Err(ValidationError(failures))
    }
}

pub(super) fn rgb(color: HexRgb) -> LinSrgb<f64> {
    Srgb::new(color.0, color.1, color.2)
        .into_format::<f64>()
        .into_linear()
}

fn linear(color: Color) -> LinSrgb<f64> {
    let Color::Rgb(r, g, b) = color else {
        unreachable!("semantic tokens must be explicit RGB")
    };
    rgb(HexRgb(r, g, b))
}

pub(super) fn contrast(left: Color, right: Color) -> f64 {
    let luminance = |c| {
        let c = linear(c);
        0.2126 * c.red + 0.7152 * c.green + 0.0722 * c.blue
    };
    let (a, b) = (luminance(left), luminance(right));
    (a.max(b) + 0.05) / (a.min(b) + 0.05)
}

// Machado et al., full-severity matrices, applied to linear RGB. Clamping here
// models displayable CVD simulation; it is NOT the generator's gamut mapping.
pub(super) const SIMULATIONS: [(&str, [[f64; 3]; 3]); 3] = [
    (
        "protanopia",
        [
            [0.152286, 1.052583, -0.204868],
            [0.114503, 0.786281, 0.099216],
            [-0.003882, -0.048116, 1.051998],
        ],
    ),
    (
        "deuteranopia",
        [
            [0.367322, 0.860646, -0.227968],
            [0.280085, 0.672501, 0.047413],
            [-0.011820, 0.042940, 0.968881],
        ],
    ),
    (
        "tritanopia",
        [
            [1.255528, -0.076749, -0.178779],
            [-0.078411, 0.930809, 0.147602],
            [0.004733, 0.691367, 0.303900],
        ],
    ),
];

pub(super) fn simulate(color: Color, matrix: [[f64; 3]; 3]) -> Oklab<f64> {
    let c = linear(color);
    let [r, g, b] =
        matrix.map(|row| (row[0] * c.red + row[1] * c.green + row[2] * c.blue).clamp(0.0, 1.0));
    Oklab::from_color_unclamped(LinSrgb::new(r, g, b))
}

pub(super) fn simulations(color: HexRgb) -> [Oklab<f64>; 3] {
    SIMULATIONS.map(|(_, matrix)| simulate(color.color(), matrix))
}

pub(super) fn distance(a: Oklab<f64>, b: Oklab<f64>) -> f64 {
    ((a.l - b.l).powi(2) + (a.a - b.a).powi(2) + (a.b - b.b).powi(2)).sqrt()
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn wcag_luminance_and_quantized_thresholds_match_reference_pairs() {
        let white = Color::Rgb(255, 255, 255);
        assert!((contrast(white, Color::Rgb(0, 0, 0)) - 21.0).abs() < 1e-10);
        assert!((contrast(white, Color::Rgb(119, 119, 119)) - 4.478089453577214).abs() < 1e-10);
        assert!(contrast(white, Color::Rgb(118, 118, 118)) >= 4.5);
        assert_eq!(contrast(white, white), 1.0);
    }

    #[test]
    fn built_in_contract_and_actionable_pairs() {
        validate_tokens(&super::super::ZEVRIA_DARK).unwrap();
        let mut broken = super::super::ZEVRIA_DARK;
        broken.text.primary = broken.surfaces.canvas;
        assert!(
            validate_tokens(&broken)
                .unwrap_err()
                .to_string()
                .contains("text.primary on canvas")
        );
    }
}

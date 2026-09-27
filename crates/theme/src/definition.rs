//! Versioned, concrete reference colors. The filename is the only theme identity.
use std::{fmt, str::FromStr};

use ratatui::style::Color;
use serde::{Deserialize, Deserializer, Serialize, Serializer};

use super::{
    ContentTokens, FeedbackTokens, RoleTokens, SurfaceTokens, SyntaxTokens, TextTokens,
    WorkflowTokens, ZevriaTheme,
};

pub const SCHEMA_VERSION: u32 = 1;

/// Strict opaque 24-bit sRGB, serialized as uppercase `#RRGGBB`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct HexRgb(pub u8, pub u8, pub u8);

impl FromStr for HexRgb {
    type Err = anyhow::Error;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        anyhow::ensure!(
            value.len() == 7
                && value.starts_with('#')
                && value.as_bytes()[1..].iter().all(u8::is_ascii_hexdigit),
            "expected an opaque RGB color in #RRGGBB format, got {value:?}"
        );
        Ok(Self(
            u8::from_str_radix(&value[1..3], 16)?,
            u8::from_str_radix(&value[3..5], 16)?,
            u8::from_str_radix(&value[5..7], 16)?,
        ))
    }
}

impl fmt::Display for HexRgb {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "#{:02X}{:02X}{:02X}", self.0, self.1, self.2)
    }
}

impl Serialize for HexRgb {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.collect_str(self)
    }
}

impl<'de> Deserialize<'de> for HexRgb {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        String::deserialize(deserializer)?
            .parse()
            .map_err(serde::de::Error::custom)
    }
}

impl HexRgb {
    pub(super) const fn color(self) -> Color {
        Color::Rgb(self.0, self.1, self.2)
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ThemeDefinition {
    pub schema_version: u32,
    /// Provenance only: supported documents never rerun a generator on load.
    pub generator_version: String,
    pub source_background: HexRgb,
    pub palette: ReferencePalette,
}

/// All 27 independent references; aliases are deliberately not editable.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ReferencePalette {
    pub canvas: HexRgb,
    pub panel: HexRgb,
    pub overlay: HexRgb,
    pub border: HexRgb,
    pub border_strong: HexRgb,
    pub selection: HexRgb,
    pub text_primary: HexRgb,
    pub text_secondary: HexRgb,
    pub text_muted: HexRgb,
    pub build: HexRgb,
    pub plan: HexRgb,
    pub review: HexRgb,
    pub explore: HexRgb,
    pub you: HexRgb,
    pub assistant: HexRgb,
    pub system: HexRgb,
    pub tools: HexRgb,
    pub success: HexRgb,
    pub warning: HexRgb,
    pub error: HexRgb,
    pub info: HexRgb,
    pub diff_addition_background: HexRgb,
    pub diff_deletion_background: HexRgb,
    pub syntax_keyword: HexRgb,
    pub syntax_function: HexRgb,
    pub syntax_string: HexRgb,
    pub syntax_constant: HexRgb,
}

impl ReferencePalette {
    /// Central reference → semantic graph (38 tokens). Component code never
    /// derives colors or interprets the persisted format.
    pub(super) const fn tokens(&self) -> ZevriaTheme {
        ZevriaTheme {
            surfaces: SurfaceTokens {
                canvas: self.canvas.color(),
                panel: self.panel.color(),
                overlay: self.overlay.color(),
                border: self.border.color(),
                border_strong: self.border_strong.color(),
                selection_background: self.selection.color(),
                selection_foreground: self.canvas.color(),
            },
            text: TextTokens {
                primary: self.text_primary.color(),
                secondary: self.text_secondary.color(),
                muted: self.text_muted.color(),
            },
            workflow: WorkflowTokens {
                build: self.build.color(),
                plan: self.plan.color(),
                review: self.review.color(),
                explore: self.explore.color(),
            },
            roles: RoleTokens {
                you: self.you.color(),
                assistant: self.assistant.color(),
                system: self.system.color(),
                tools: self.tools.color(),
            },
            feedback: FeedbackTokens {
                success: self.success.color(),
                warning: self.warning.color(),
                error: self.error.color(),
                info: self.info.color(),
            },
            content: ContentTokens {
                heading: self.explore.color(),
                strong: self.plan.color(),
                code: self.syntax_string.color(),
                link: self.info.color(),
                diff_addition: self.success.color(),
                diff_addition_background: self.diff_addition_background.color(),
                diff_deletion: self.error.color(),
                diff_deletion_background: self.diff_deletion_background.color(),
                diff_hunk: self.info.color(),
            },
            syntax: SyntaxTokens {
                keyword: self.syntax_keyword.color(),
                function: self.syntax_function.color(),
                r#type: self.plan.color(),
                string: self.syntax_string.color(),
                constant: self.syntax_constant.color(),
                comment: self.text_muted.color(),
                invalid: self.error.color(),
            },
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::validation::validate_theme;

    #[test]
    fn legacy_palette_0_7_6_theme_validates_and_roundtrips_without_regeneration() {
        // Captured by the CLI with palette and palette_derive pinned to 0.7.6.
        // Load concrete saved colors, never regenerate the fixture on upgrade.
        let fixture = include_str!("fixtures/palette-0.7.6.toml");
        let original: toml::Table = toml::from_str(fixture).unwrap();
        let definition: ThemeDefinition = toml::from_str(fixture).unwrap();
        assert_eq!(definition.schema_version, 1);
        assert_eq!(definition.generator_version, "oklch-search-1");
        assert_eq!(definition.source_background, HexRgb(0x1e, 0x1e, 0x2e));
        assert_eq!(original["palette"].as_table().unwrap().len(), 27);
        validate_theme(&definition).unwrap();

        let serialized = toml::to_string_pretty(&definition).unwrap();
        // Compare the entire document, including every RGB reference, schema
        // field and provenance, without depending on TOML formatting.
        assert_eq!(
            toml::from_str::<toml::Table>(&serialized).unwrap(),
            original
        );
        let roundtrip: ThemeDefinition = toml::from_str(&serialized).unwrap();
        assert_eq!(roundtrip, definition);
        validate_theme(&roundtrip).unwrap();
    }

    #[test]
    fn strict_hex_rgb() {
        assert_eq!("#aB00fF".parse::<HexRgb>().unwrap().to_string(), "#AB00FF");
        for bad in [
            "",
            "red",
            "123456",
            "#123",
            "#12345678",
            "#12345G",
            " #123456",
            "#é1234",
        ] {
            assert!(bad.parse::<HexRgb>().is_err(), "{bad}");
        }
    }
}

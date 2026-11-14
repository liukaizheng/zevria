//! Local-only, versioned image cost rules. Never part of provider request identity.
//!
//! Rule provenance (reviewed 2026-11-14):
//! https://developers.openai.com/api/docs/guides/images-vision
//! https://platform.claude.com/docs/en/build-with-claude/vision
//! https://platform.claude.com/docs/en/build-with-claude/vision-coordinates
//! These are estimates, including provider rounding qualifications, not upper bounds.
use serde::{Deserialize, Serialize};

pub const IMAGE_ACCOUNTING_VERSION: u32 = 1;

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ImageDetail {
    Low,
    High,
    Original,
    #[default]
    Auto,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PatchRule {
    pub patch_size: u32,
    pub max_edge: u32,
    pub patch_budget: u32,
    pub multiplier_numerator: u32,
    pub multiplier_denominator: u32,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "rule", rename_all = "snake_case", deny_unknown_fields)]
pub enum ImageAccountingPolicy {
    /// Config-only: resolved using an exact model ID on an official endpoint.
    Auto,
    /// Explicit opt-in for a verified gateway mapping; never inferred by resemblance.
    Preset { model: String },
    #[default]
    Unresolved,
    OpenaiPatch {
        high: PatchRule,
        low: Option<PatchRule>,
        original: Option<PatchRule>,
        auto: ImageDetail,
    },
    OpenaiTile {
        base_tokens: u32,
        tile_tokens: u32,
        tile_size: u32,
        max_edge: u32,
        short_edge: u32,
        auto: ImageDetail,
    },
    AnthropicPatch {
        patch_size: u32,
        max_edge: u32,
        max_patches: u32,
        many_image_threshold: u32,
        many_image_max_edge: u32,
    },
}

impl ImageAccountingPolicy {
    pub fn version(&self) -> u32 {
        IMAGE_ACCOUNTING_VERSION
    }
    pub fn label(&self) -> &'static str {
        match self {
            Self::OpenaiPatch { .. } => "openai-patch/v1",
            Self::OpenaiTile { .. } => "openai-tile/v1",
            Self::AnthropicPatch { .. } => "anthropic-patch/v1",
            _ => "unresolved/v1",
        }
    }
    /// Deliberately small allowlist. No prefixes, substrings, or speculative snapshots.
    pub fn preset(model: &str) -> Option<Self> {
        let tile = |base_tokens, tile_tokens| Self::OpenaiTile {
            base_tokens,
            tile_tokens,
            tile_size: 512,
            max_edge: 2048,
            short_edge: 768,
            auto: ImageDetail::High,
        };
        let patch = |multiplier_numerator| {
            let rule = PatchRule {
                patch_size: 32,
                max_edge: 2048,
                patch_budget: 6144,
                multiplier_numerator,
                multiplier_denominator: 100,
            };
            Self::OpenaiPatch {
                high: rule,
                low: Some(rule),
                original: None,
                auto: ImageDetail::High,
            }
        };
        Some(match model {
            "gpt-4.1" | "gpt-4.1-2025-04-14" | "gpt-4o" | "gpt-4o-2024-08-06"
            | "gpt-4o-2024-11-20" => tile(85, 170),
            "gpt-4o-mini" | "gpt-4o-mini-2024-07-18" => tile(2833, 5667),
            "gpt-5.1" => tile(70, 140),
            "gpt-4.1-mini" | "gpt-4.1-mini-2025-04-14" => patch(162),
            "gpt-5.2" => patch(120),
            // Published standard-resolution tier. Unknown/new aliases stay unknown.
            "claude-sonnet-4-5-20250929"
            | "claude-haiku-4-5-20251001"
            | "claude-opus-4-5-20251101"
            | "claude-opus-4-6"
            | "claude-sonnet-4-6" => Self::AnthropicPatch {
                patch_size: 28,
                max_edge: 1568,
                max_patches: 1568,
                many_image_threshold: 20,
                many_image_max_edge: 3000,
            },
            _ => return None,
        })
    }
    pub fn resolve(
        &self,
        model: &str,
        official_endpoint: bool,
        anthropic: bool,
    ) -> anyhow::Result<Self> {
        let resolved = match self {
            Self::Auto => {
                if official_endpoint {
                    Self::preset(model)
                        .filter(|policy| policy.validate(anthropic).is_ok())
                        .unwrap_or_default()
                } else {
                    Self::Unresolved
                }
            }
            Self::Preset { model } => Self::preset(model)
                .ok_or_else(|| anyhow::anyhow!("unverified image-accounting preset {model}"))?,
            policy => policy.clone(),
        };
        resolved.validate(anthropic)?;
        Ok(resolved)
    }
    pub fn validate(&self, anthropic: bool) -> anyhow::Result<()> {
        let bounded = |n: u32, max| n > 0 && n <= max;
        match self {
            Self::Auto | Self::Unresolved => {}
            Self::Preset { model } => {
                Self::preset(model)
                    .ok_or_else(|| anyhow::anyhow!("unverified image-accounting preset {model}"))?
                    .validate(anthropic)?;
            }
            Self::OpenaiPatch {
                high,
                low,
                original,
                auto,
            } => {
                anyhow::ensure!(
                    !anthropic && *auto != ImageDetail::Auto,
                    "OpenAI patch policy requires Responses and an explicit auto detail mapping"
                );
                anyhow::ensure!(
                    *auto != ImageDetail::Original || original.is_some(),
                    "auto original requires an original rule"
                );
                for rule in [Some(high), low.as_ref(), original.as_ref()]
                    .into_iter()
                    .flatten()
                {
                    anyhow::ensure!(
                        bounded(rule.patch_size, 1024)
                            && bounded(rule.max_edge, 65_535)
                            && bounded(rule.patch_budget, 30_000)
                            && bounded(rule.multiplier_numerator, 100_000)
                            && bounded(rule.multiplier_denominator, 100_000),
                        "image patch rule parameters out of bounds"
                    );
                }
            }
            Self::OpenaiTile {
                base_tokens,
                tile_tokens,
                tile_size,
                max_edge,
                short_edge,
                auto,
            } => {
                anyhow::ensure!(
                    *auto != ImageDetail::Original,
                    "tile rules do not support original detail"
                );
                anyhow::ensure!(
                    !anthropic
                        && bounded(*base_tokens, 100_000)
                        && bounded(*tile_tokens, 100_000)
                        && bounded(*tile_size, 4096)
                        && bounded(*max_edge, 65_535)
                        && bounded(*short_edge, *max_edge),
                    "invalid or protocol-incompatible image tile policy"
                );
            }
            Self::AnthropicPatch {
                patch_size,
                max_edge,
                max_patches,
                many_image_threshold,
                many_image_max_edge,
            } => {
                anyhow::ensure!(
                    anthropic
                        && bounded(*patch_size, 1024)
                        && bounded(*max_edge, 65_535)
                        && *max_edge >= *patch_size
                        && bounded(*max_patches, 30_000)
                        && bounded(*many_image_threshold, 1000)
                        && bounded(*many_image_max_edge, 65_535),
                    "invalid or protocol-incompatible Anthropic patch policy"
                );
            }
        }
        Ok(())
    }
    /// Cost of the representation actually sent. Anthropic has no detail input.
    pub fn image_tokens(&self, dimensions: Option<(u32, u32)>, detail: ImageDetail) -> Option<u64> {
        if self
            .validate(matches!(self, Self::AnthropicPatch { .. }))
            .is_err()
        {
            return None;
        }
        let detail = match self {
            Self::OpenaiTile { auto, .. } if detail == ImageDetail::Auto => *auto,
            _ => detail,
        };
        if let Self::OpenaiTile { base_tokens, .. } = self
            && detail == ImageDetail::Low
        {
            return Some(u64::from(*base_tokens));
        }
        let (width, height) = dimensions.filter(|(w, h)| *w > 0 && *h > 0)?;
        let patches = |w: u32, h: u32, p: u32| u64::from(w.div_ceil(p)) * u64::from(h.div_ceil(p));
        match self {
            Self::OpenaiTile {
                base_tokens,
                tile_tokens,
                tile_size,
                max_edge,
                short_edge,
                ..
            } if detail == ImageDetail::High => {
                let scale = (f64::from(*max_edge) / f64::from(width.max(height))).min(1.0);
                let mut w = (f64::from(width) * scale).round().max(1.0) as u32;
                let mut h = (f64::from(height) * scale).round().max(1.0) as u32;
                if w.min(h) > *short_edge {
                    let scale = f64::from(*short_edge) / f64::from(w.min(h));
                    w = (f64::from(w) * scale).floor().max(1.0) as u32;
                    h = (f64::from(h) * scale).floor().max(1.0) as u32;
                }
                Some(u64::from(*base_tokens) + patches(w, h, *tile_size) * u64::from(*tile_tokens))
            }
            Self::OpenaiPatch {
                high,
                low,
                original,
                auto,
            } => {
                let detail = if detail == ImageDetail::Auto {
                    *auto
                } else {
                    detail
                };
                let rule = match detail {
                    ImageDetail::Low => low.as_ref()?,
                    ImageDetail::High => high,
                    ImageDetail::Original => original.as_ref()?,
                    ImageDetail::Auto => return None,
                };
                let p = f64::from(rule.patch_size);
                let scale = (f64::from(rule.max_edge) / f64::from(width.max(height))).min(1.0);
                let mut w = (f64::from(width) * scale).round().max(1.0);
                let mut h = (f64::from(height) * scale).round().max(1.0);
                if patches(w as u32, h as u32, rule.patch_size) > u64::from(rule.patch_budget) {
                    let scale = (p * p * f64::from(rule.patch_budget) / (w * h)).sqrt();
                    let adjust = ((w * scale / p).floor() / (w * scale / p))
                        .min((h * scale / p).floor() / (h * scale / p));
                    w = (w * scale * adjust).floor().max(1.0);
                    h = (h * scale * adjust).floor().max(1.0);
                }
                let count = patches(w as u32, h as u32, rule.patch_size);
                if count > u64::from(rule.patch_budget) {
                    return None;
                }
                Some(
                    (count * u64::from(rule.multiplier_numerator))
                        .div_ceil(u64::from(rule.multiplier_denominator)),
                )
            }
            Self::AnthropicPatch {
                patch_size,
                max_edge,
                max_patches,
                ..
            } => {
                // Published reference: largest aspect-preserving integer size that fits;
                // round ties to even, matching the Python reference (not Rust round()).
                let (w, h) = if width >= height {
                    (width, height)
                } else {
                    (height, width)
                };
                let fits = |w: u32, h: u32| {
                    w.div_ceil(*patch_size) * *patch_size <= *max_edge
                        && h.div_ceil(*patch_size) * *patch_size <= *max_edge
                        && patches(w, h, *patch_size) <= u64::from(*max_patches)
                };
                if fits(w, h) {
                    return Some(patches(w, h, *patch_size));
                }
                let aspect = f64::from(w) / f64::from(h);
                let scaled_h = |w| (f64::from(w) / aspect).round_ties_even().max(1.0) as u32;
                let (mut lo, mut hi) = (1, w);
                while lo + 1 < hi {
                    let mid = (lo + hi) / 2;
                    if fits(mid, scaled_h(mid)) {
                        lo = mid;
                    } else {
                        hi = mid;
                    }
                }
                Some(patches(lo, scaled_h(lo), *patch_size))
            }
            _ => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn detail_rounding_bounds_and_protocol_mapping_are_explicit() {
        let tile = ImageAccountingPolicy::preset("gpt-4o").unwrap();
        assert_eq!(
            tile.image_tokens(Some((512, 512)), ImageDetail::High),
            Some(255)
        );
        assert_eq!(
            tile.image_tokens(Some((513, 512)), ImageDetail::High),
            Some(425)
        );
        assert_eq!(
            tile.image_tokens(Some((512, 512)), ImageDetail::Auto),
            Some(255),
            "published auto sizing uses the tile high rule"
        );
        assert_eq!(
            tile.image_tokens(Some((512, 512)), ImageDetail::Original),
            None
        );
        let patch = ImageAccountingPolicy::preset("gpt-4.1-mini-2025-04-14").unwrap();
        for (w, h, tokens) in [(31, 32, 2), (32, 32, 2), (33, 32, 4), (96, 64, 10)] {
            assert_eq!(
                patch.image_tokens(Some((w, h)), ImageDetail::High),
                Some(tokens)
            );
        }
        assert_eq!(
            patch.image_tokens(Some((32, 32)), ImageDetail::Low),
            Some(2)
        );
        let invalid = ImageAccountingPolicy::OpenaiPatch {
            high: PatchRule {
                patch_size: 0,
                max_edge: 2048,
                patch_budget: 6144,
                multiplier_numerator: 162,
                multiplier_denominator: 100,
            },
            low: None,
            original: None,
            auto: ImageDetail::High,
        };
        assert!(invalid.validate(false).is_err());
        assert_eq!(
            invalid.image_tokens(Some((32, 32)), ImageDetail::High),
            None
        );
        assert!(
            ImageAccountingPolicy::Preset {
                model: "claude-opus-4-6".into()
            }
            .resolve("alias", false, false)
            .is_err()
        );
    }

    #[test]
    fn parameterized_patch_resize_detail_and_high_resolution_golden_vectors() {
        let rule = |max_edge, patch_budget| PatchRule {
            patch_size: 32,
            max_edge,
            patch_budget,
            multiplier_numerator: 120,
            multiplier_denominator: 100,
        };
        let policy = ImageAccountingPolicy::OpenaiPatch {
            high: rule(65535, 2500),
            low: Some(rule(512, 256)),
            original: Some(rule(6000, 10000)),
            auto: ImageDetail::Original,
        };
        assert_eq!(
            policy.image_tokens(Some((1024, 1024)), ImageDetail::Low),
            Some(308)
        );
        assert_eq!(
            policy.image_tokens(Some((1024, 1024)), ImageDetail::High),
            Some(1229)
        );
        assert_eq!(
            policy.image_tokens(Some((2048, 2048)), ImageDetail::High),
            Some(3000)
        );
        assert_eq!(
            policy.image_tokens(Some((4096, 512)), ImageDetail::High),
            Some(2458)
        );
        assert_eq!(
            policy.image_tokens(Some((4096, 4096)), ImageDetail::Original),
            Some(12000)
        );
        assert_eq!(
            policy.image_tokens(Some((4096, 4096)), ImageDetail::Auto),
            Some(12000)
        );
        let high_resolution = ImageAccountingPolicy::AnthropicPatch {
            patch_size: 28,
            max_edge: 2576,
            max_patches: 4784,
            many_image_threshold: 20,
            many_image_max_edge: 3000,
        };
        assert_eq!(
            high_resolution.image_tokens(Some((1920, 1080)), ImageDetail::Auto),
            Some(2691)
        );
        assert_eq!(
            high_resolution.image_tokens(Some((3840, 2160)), ImageDetail::Auto),
            Some(4784)
        );
    }

    #[test]
    fn documented_golden_costs_and_unknown_mapping() {
        let tile = ImageAccountingPolicy::preset("gpt-4o").unwrap();
        assert_eq!(
            tile.image_tokens(Some((1024, 1024)), ImageDetail::High),
            Some(765)
        );
        assert_eq!(
            tile.image_tokens(Some((2048, 4096)), ImageDetail::High),
            Some(1105)
        );
        assert_eq!(tile.image_tokens(None, ImageDetail::Low), Some(85));
        assert_eq!(
            ImageAccountingPolicy::preset("gpt-4o-mini")
                .unwrap()
                .image_tokens(None, ImageDetail::Low),
            Some(2833)
        );
        assert_eq!(
            ImageAccountingPolicy::preset("gpt-5.1")
                .unwrap()
                .image_tokens(None, ImageDetail::Low),
            Some(70)
        );
        assert_eq!(
            ImageAccountingPolicy::preset("gpt-5.2")
                .unwrap()
                .image_tokens(Some((1024, 1024)), ImageDetail::Auto),
            Some(1229)
        );
        let patch = ImageAccountingPolicy::preset("gpt-4.1-mini-2025-04-14").unwrap();
        assert_eq!(
            patch.image_tokens(Some((1024, 1024)), ImageDetail::Auto),
            Some(1659)
        );
        let claude = ImageAccountingPolicy::preset("claude-sonnet-4-5-20250929").unwrap();
        for (w, h, t) in [
            (200, 200, 64),
            (1000, 1000, 1296),
            (1092, 1092, 1521),
            (1920, 1080, 1560),
            (1075, 1520, 1551),
        ] {
            assert_eq!(
                claude.image_tokens(Some((w, h)), ImageDetail::Auto),
                Some(t),
                "{w}x{h}"
            );
        }
        assert!(ImageAccountingPolicy::preset("gpt-5.1-codex-max").is_none());
        assert_eq!(
            ImageAccountingPolicy::Auto
                .resolve("gpt-4o", false, false)
                .unwrap(),
            ImageAccountingPolicy::Unresolved
        );
        assert!(tile.validate(true).is_err());
    }
}

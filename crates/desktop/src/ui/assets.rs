//! Application asset boundary, shared by native launch and the screenshot gallery.
//! All currently used Lucide controls are in Kit's default bundle; no full catalog
//! or duplicate SVGs are embedded. Add icon_assets! extras here when one is needed.
use gpui_kit::{AssetSource, SharedString};
use std::borrow::Cow;
#[derive(Clone, Copy)]
pub struct AppAssets;
impl AssetSource for AppAssets {
    fn load(&self, path: &str) -> anyhow::Result<Option<Cow<'static, [u8]>>> {
        gpui_kit::assets::Assets.load(path)
    }
    fn list(&self, path: &str) -> anyhow::Result<Vec<SharedString>> {
        gpui_kit::assets::Assets.list(path)
    }
}

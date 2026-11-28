//! Native image ingress shared by the main and Side Chat composers.
use super::style;
use crate::controller::SessionController;
use gpui_kit::{assets::IconName as Lucide, *};
use std::{
    io::{Cursor, Read},
    path::PathBuf,
};
use zevria_content::{
    PromptImage,
    image_preparation::{MAX_IMAGE_SOURCE_BYTES, prepare_file_bytes, prepare_rgba},
    prompt::{MAX_PROMPT_IMAGE_BYTES, MAX_PROMPT_IMAGES},
};
use zevria_foundation::SideChatId;

pub(super) type Target = Option<SideChatId>;

enum Source {
    File(PathBuf),
    Clipboard(Vec<u8>),
}

pub(super) fn attach_button(id: &'static str) -> gpui_kit::component::button::Button {
    style::icon_button(id, Lucide::Paperclip, "Attach images")
}

pub(super) fn choose(controller: Entity<SessionController>, target: Target, cx: &mut App) {
    let Some(generation) = controller.update(cx, |c, cx| c.begin_image_dialog(target, cx)) else {
        return;
    };
    let paths = cx.prompt_for_paths(PathPromptOptions {
        files: true,
        directories: false,
        multiple: true,
        prompt: Some("Attach images".into()),
    });
    let controller = controller.downgrade();
    cx.spawn(async move |cx| {
        let paths = paths.await;
        let Some(controller) = controller.upgrade() else {
            return;
        };
        if !controller.update(cx, |c, cx| c.finish_image_dialog(generation, cx)) {
            return;
        }
        match paths {
            Ok(Ok(Some(paths))) => {
                cx.update(|cx| {
                    prepare(
                        controller,
                        target,
                        generation,
                        paths.into_iter().map(Source::File).collect(),
                        cx,
                    )
                });
            }
            Ok(Err(error)) => {
                let _ = controller.update(cx, |c, cx| {
                    c.finish_image_attachment(
                        target,
                        generation,
                        Err(format!("Could not open the image picker: {error}")),
                        cx,
                    )
                });
            }
            _ => {}
        }
    })
    .detach();
}

pub(super) fn paste(
    controller: &Entity<SessionController>,
    target: Target,
    clipboard: &ClipboardItem,
    cx: &mut App,
) -> bool {
    let Some(generation) = controller.read(cx).image_attachment_generation(target) else {
        return false;
    };
    let mut sources = Vec::new();
    for entry in clipboard.entries() {
        match entry {
            ClipboardEntry::Image(image) => {
                if image.bytes.len() > MAX_IMAGE_SOURCE_BYTES {
                    controller.update(cx, |c, cx| {
                        c.finish_image_attachment(
                            target,
                            generation,
                            Err("Clipboard image exceeds 32 MiB.".into()),
                            cx,
                        )
                    });
                    return true;
                }
                sources.push(Source::Clipboard(image.bytes.clone()));
            }
            ClipboardEntry::ExternalPaths(paths) => sources.extend(
                paths
                    .0
                    .iter()
                    .filter(|path| {
                        path.extension()
                            .and_then(|ext| ext.to_str())
                            .is_some_and(|ext| {
                                matches!(
                                    ext.to_ascii_lowercase().as_str(),
                                    "png"
                                        | "jpg"
                                        | "jpeg"
                                        | "webp"
                                        | "gif"
                                        | "tiff"
                                        | "tif"
                                        | "bmp"
                                )
                            })
                    })
                    .cloned()
                    .map(Source::File),
            ),
            ClipboardEntry::String(_) => {}
        }
        if sources.len() > MAX_PROMPT_IMAGES {
            break;
        }
    }
    if sources.is_empty() {
        return false;
    }
    prepare(controller.clone(), target, generation, sources, cx);
    true
}

pub(super) fn drop_paths(
    controller: &Entity<SessionController>,
    target: Target,
    paths: Vec<PathBuf>,
    cx: &mut App,
) {
    if let Some(generation) = controller.read(cx).image_attachment_generation(target) {
        prepare(
            controller.clone(),
            target,
            generation,
            paths.into_iter().map(Source::File).collect(),
            cx,
        );
    }
}

fn prepare(
    controller: Entity<SessionController>,
    target: Target,
    generation: u64,
    sources: Vec<Source>,
    cx: &mut App,
) {
    if sources.is_empty() {
        return;
    }
    let job = cx
        .background_executor()
        .spawn(async move { decode(sources) });
    let controller = controller.downgrade();
    cx.spawn(async move |cx| {
        let result = job.await;
        let _ = controller.update(cx, |c, cx| {
            c.finish_image_attachment(target, generation, result, cx)
        });
    })
    .detach();
}

fn decode(sources: Vec<Source>) -> Result<Vec<PromptImage>, String> {
    if sources.len() > MAX_PROMPT_IMAGES {
        return Err(format!(
            "Attach at most {MAX_PROMPT_IMAGES} images per message."
        ));
    }
    let mut images = Vec::with_capacity(sources.len());
    let mut total = 0;
    for source in sources {
        let (name, bytes) = match source {
            Source::File(path) => {
                let name = path
                    .file_name()
                    .unwrap_or(path.as_os_str())
                    .to_string_lossy()
                    .into_owned();
                if !path.is_file() {
                    return Err(format!("{name} is not a regular file."));
                }
                let mut bytes = Vec::new();
                std::fs::File::open(&path)
                    .and_then(|file| {
                        file.take(MAX_IMAGE_SOURCE_BYTES as u64 + 1)
                            .read_to_end(&mut bytes)
                    })
                    .map_err(|error| format!("Could not read {name}: {error}"))?;
                (name, bytes)
            }
            Source::Clipboard(bytes) => ("clipboard image".into(), bytes),
        };
        let image =
            decode_image(bytes).map_err(|error| format!("Could not attach {name}: {error}"))?;
        total += image.encoded_len();
        if total > MAX_PROMPT_IMAGE_BYTES {
            return Err("Images exceed the 20 MiB per-message limit.".into());
        }
        images.push(image);
    }
    Ok(images)
}

#[cfg(test)]
mod tests {
    use super::{Cursor, decode_image};

    #[test]
    fn native_clipboard_rasters_are_prepared_for_prompt_storage() {
        let raster = image::DynamicImage::new_rgba8(3, 2);
        for format in [image::ImageFormat::Tiff, image::ImageFormat::Bmp] {
            let mut bytes = Cursor::new(Vec::new());
            raster.write_to(&mut bytes, format).unwrap();
            let image = decode_image(bytes.into_inner()).unwrap();
            assert_eq!(image.dimensions(), (3, 2));
            assert_eq!(image.mime_type(), "image/png");
        }
    }
}

fn decode_image(bytes: Vec<u8>) -> anyhow::Result<PromptImage> {
    anyhow::ensure!(
        bytes.len() <= MAX_IMAGE_SOURCE_BYTES,
        "image exceeds 32 MiB"
    );
    match image::guess_format(&bytes) {
        Ok(format @ (image::ImageFormat::Tiff | image::ImageFormat::Bmp)) => {
            let (w, h) =
                image::ImageReader::with_format(Cursor::new(&bytes), format).into_dimensions()?;
            anyhow::ensure!(
                u64::from(w) * u64::from(h) <= 40_000_000,
                "image has too many pixels"
            );
            let mut reader = image::ImageReader::with_format(Cursor::new(bytes), format);
            let mut limits = image::Limits::default();
            limits.max_alloc = Some(160_000_000);
            reader.limits(limits);
            let rgba = reader.decode()?.into_rgba8();
            Ok(prepare_rgba(w as usize, h as usize, rgba.as_raw())?.image)
        }
        _ => Ok(prepare_file_bytes(bytes)?.image),
    }
}

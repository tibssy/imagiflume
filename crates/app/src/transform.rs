//! Rotate and flip the image on the canvas.
//!
//! A recorded image becomes a new history entry, so the transform joins the
//! chain like an adjustment. A reference that is only a cache copy is written
//! back into the reference set instead, keeping the edit workflow intact.

use std::path::Path;
use std::rc::Rc;
use std::time::Instant;

use gtk::prelude::ToggleButtonExt;
use image::{DynamicImage, GenericImageView};
use imagiflume_core::paths;

use crate::history::{Generation, NewGeneration};
use crate::{
    Ui, annotate_image, history_view, now_millis, reload_history, show_image_with_references,
    unix_seconds, write_thumbnail,
};

#[derive(Clone, Copy)]
pub(crate) enum Transform {
    RotateLeft,
    RotateRight,
    FlipHorizontal,
    FlipVertical,
}

impl Transform {
    fn apply(self, image: DynamicImage) -> DynamicImage {
        match self {
            Self::RotateLeft => image.rotate270(),
            Self::RotateRight => image.rotate90(),
            Self::FlipHorizontal => image.fliph(),
            Self::FlipVertical => image.flipv(),
        }
    }

    fn label(self) -> &'static str {
        match self {
            Self::RotateLeft => "Rotated 90° left",
            Self::RotateRight => "Rotated 90° right",
            Self::FlipHorizontal => "Flipped horizontally",
            Self::FlipVertical => "Flipped vertically",
        }
    }

    fn slug(self) -> &'static str {
        match self {
            Self::RotateLeft => "rotated-left",
            Self::RotateRight => "rotated-right",
            Self::FlipHorizontal => "flipped-horizontal",
            Self::FlipVertical => "flipped-vertical",
        }
    }
}

pub(crate) fn apply(ui: &Rc<Ui>, transform: Transform) {
    if (ui.generating.get() && !ui.upscaling.get()) || ui.zooming.get() || ui.classifying.get() {
        return;
    }
    let Some(source) = ui.state.borrow().display.clone() else {
        ui.toast
            .add_toast(adw::Toast::new("No image to rotate or flip."));
        return;
    };
    ui.crop_button.set_active(false);

    let started = Instant::now();
    let image = match image::open(&source) {
        Ok(image) => image,
        Err(error) => {
            tracing::warn!(%error, path = %source.display(), "failed to open image for transform");
            ui.toast
                .add_toast(adw::Toast::new("Could not read this image."));
            return;
        }
    };
    let transformed = transform.apply(image);
    let (width, height) = transformed.dimensions();

    let source_text = source.to_string_lossy().into_owned();
    let source_entry = ui
        .history
        .borrow()
        .find_by_image_path(&source_text)
        .ok()
        .flatten();

    let output = match &source_entry {
        Some(_) => {
            let output_dir = ui.config.borrow().output_dir.clone();
            let stem = output_stem(&source);
            output_dir.join(format!("{stem}-{}-{}.png", transform.slug(), now_millis()))
        }
        None => paths::cache_dir().join("transforms").join(format!(
            "{}-{}.png",
            transform.slug(),
            now_millis()
        )),
    };
    if let Some(parent) = output.parent()
        && let Err(error) = std::fs::create_dir_all(parent)
    {
        tracing::error!(%error, path = %parent.display(), "failed to create the output folder");
        ui.toast
            .add_toast(adw::Toast::new("Could not save the transformed image."));
        return;
    }
    if let Err(error) = transformed.save(&output) {
        tracing::error!(%error, path = %output.display(), "failed to save transform");
        ui.toast
            .add_toast(adw::Toast::new("Could not save the transformed image."));
        return;
    }
    let duration_ms = started.elapsed().as_millis() as i64;

    let mut references = ui.state.borrow().references.clone();
    for reference in &mut references {
        if *reference == source {
            *reference = output.clone();
        }
    }

    let shown = show_image_with_references(ui, &output, references);
    if !shown {
        ui.toast
            .add_toast(adw::Toast::new("Could not show the transformed image."));
        return;
    }

    if let Some(entry) = source_entry {
        record_transform(
            ui,
            &source_text,
            &output,
            transform,
            &entry,
            duration_ms,
            (width, height),
        );
    }

    ui.undo.borrow_mut().clear();
    ui.status
        .set_text(&format!("{} · {width} × {height}", transform.label()));
    ui.toast
        .add_toast(adw::Toast::new(&format!("{}.", transform.label())));
}

fn record_transform(
    ui: &Rc<Ui>,
    source_text: &str,
    output: &Path,
    transform: Transform,
    source_entry: &Generation,
    duration_ms: i64,
    (width, height): (u32, u32),
) {
    let title = if source_entry.title.trim().is_empty() {
        String::new()
    } else {
        format!("{} · {}", transform.label(), source_entry.title)
    };
    let image_text = output.to_string_lossy().into_owned();
    let created_at = unix_seconds();
    let inserted = ui.history.borrow().insert(&NewGeneration {
        prompt: transform.label(),
        title: &title,
        model: &source_entry.model,
        style: "none",
        width,
        height,
        steps: 0,
        seed: "",
        created_at,
        duration_ms,
        image_path: &image_text,
        reference_path: None,
        reference_paths: &[],
        kind: "transform",
        source_path: Some(source_text),
    });

    match inserted {
        Ok((id, parent_id)) => {
            if let Err(error) = write_thumbnail(output, id) {
                tracing::warn!(%error, "failed to write thumbnail");
            }
            let entry = Generation {
                id,
                prompt: transform.label().into(),
                title,
                model: source_entry.model.clone(),
                style: "none".into(),
                width,
                height,
                steps: 0,
                seed: String::new(),
                created_at,
                duration_ms,
                image_path: image_text,
                reference_path: None,
                reference_paths: Vec::new(),
                kind: "transform".into(),
                source_path: Some(source_text.to_string()),
                parent_id,
                favorite: false,
            };
            annotate_image(output, &entry, 0.0, "", &[]);
            reload_history(ui, false);
            history_view::highlight(ui, output);
        }
        Err(error) => tracing::error!(%error, "failed to record transform"),
    }
}

fn output_stem(source: &Path) -> String {
    let stem: String = source
        .file_stem()
        .and_then(|stem| stem.to_str())
        .unwrap_or("image")
        .chars()
        .filter(|character| character.is_alphanumeric() || matches!(character, '-' | '_'))
        .take(48)
        .collect();
    if stem.is_empty() {
        "image".into()
    } else {
        stem
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use image::{Rgba, RgbaImage};

    fn image(width: u32, height: u32) -> DynamicImage {
        DynamicImage::ImageRgba8(RgbaImage::from_pixel(width, height, Rgba([1, 2, 3, 255])))
    }

    #[test]
    fn rotations_swap_the_dimensions() {
        assert_eq!(
            Transform::RotateRight.apply(image(2, 3)).dimensions(),
            (3, 2)
        );
        assert_eq!(
            Transform::RotateLeft.apply(image(2, 3)).dimensions(),
            (3, 2)
        );
    }

    #[test]
    fn flips_keep_the_dimensions() {
        assert_eq!(
            Transform::FlipHorizontal.apply(image(2, 3)).dimensions(),
            (2, 3)
        );
        assert_eq!(
            Transform::FlipVertical.apply(image(2, 3)).dimensions(),
            (2, 3)
        );
    }
}

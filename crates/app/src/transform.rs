//! Previewed rotate/flip operations.
//!
//! The transform popover only changes what is on the canvas; the image and
//! History are untouched until the checkmark is pressed. A recorded image then
//! becomes a new history entry, while a cache-only reference is replaced in the
//! reference set.

use std::path::Path;
use std::rc::Rc;
use std::time::Instant;

use gtk::gdk;
use gtk::glib;
use gtk::prelude::*;
use image::{DynamicImage, GenericImageView};
use imagiflume_core::paths;

use crate::history::{Generation, NewGeneration};
use crate::{
    Ui, annotate_image, history_view, naming, now_millis, reload_history,
    show_image_with_references, show_transform_preview, unix_seconds, write_thumbnail,
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
            Self::RotateLeft => "Rotated -90",
            Self::RotateRight => "Rotated +90",
            Self::FlipHorizontal => "Flipped horizontal",
            Self::FlipVertical => "Flipped vertical",
        }
    }
}

pub(crate) struct Preview {
    source: std::path::PathBuf,
    current: DynamicImage,
    operations: Vec<Transform>,
}

/// Start a preview session when the popover opens.
pub(crate) fn begin(ui: &Rc<Ui>) -> bool {
    if ui.transform_preview.borrow().is_some() {
        return true;
    }
    let Some(source) = ui.state.borrow().display.clone() else {
        ui.transform_apply_button.set_sensitive(false);
        return false;
    };
    let image = match image::open(&source) {
        Ok(image) => image,
        Err(error) => {
            tracing::warn!(%error, path = %source.display(), "failed to open image for transform");
            ui.toast
                .add_toast(adw::Toast::new("Could not read this image."));
            return false;
        }
    };
    *ui.transform_preview.borrow_mut() = Some(Preview {
        source,
        current: image,
        operations: Vec::new(),
    });
    ui.transform_apply_button.set_sensitive(false);
    true
}

/// Apply one transform to the canvas preview only.
pub(crate) fn preview(ui: &Rc<Ui>, transform: Transform) {
    if ui.transform_preview.borrow().is_none() && !begin(ui) {
        return;
    }
    let (texture, label) = {
        let mut guard = ui.transform_preview.borrow_mut();
        let Some(preview) = guard.as_mut() else {
            return;
        };
        let current = std::mem::replace(&mut preview.current, DynamicImage::new_rgba8(1, 1));
        preview.current = transform.apply(current);
        preview.operations.push(transform);
        (texture_for(&preview.current), transform.label())
    };
    show_transform_preview(ui, &texture);
    ui.transform_apply_button.set_sensitive(true);
    ui.status.set_text(&format!("Preview · {label}"));
}

/// Save the previewed image and add it to History (or the reference set).
pub(crate) fn commit(ui: &Rc<Ui>) {
    let Some(preview) = ui.transform_preview.borrow_mut().take() else {
        return;
    };
    ui.transform_apply_button.set_sensitive(false);
    if preview.operations.is_empty()
        || ui.state.borrow().display.as_deref() != Some(preview.source.as_path())
    {
        return;
    }

    let suffix = preview
        .operations
        .iter()
        .map(|operation| operation.label())
        .collect::<Vec<_>>()
        .join(" + ");
    let title = naming::derived(ui, &preview.source, &suffix);
    let source_text = preview.source.to_string_lossy().into_owned();
    let source_entry = ui
        .history
        .borrow()
        .find_by_image_path(&source_text)
        .ok()
        .flatten();

    let started = Instant::now();
    let output = if source_entry.is_some() {
        let output_dir = ui.config.borrow().output_dir.clone();
        naming::unique_output_path(&output_dir, &title)
    } else {
        paths::cache_dir()
            .join("transforms")
            .join(format!("transform-{}.png", now_millis()))
    };
    if let Some(parent) = output.parent()
        && let Err(error) = std::fs::create_dir_all(parent)
    {
        tracing::error!(%error, path = %parent.display(), "failed to create the output folder");
        ui.toast
            .add_toast(adw::Toast::new("Could not save the transformed image."));
        return;
    }
    if let Err(error) = preview.current.save(&output) {
        tracing::error!(%error, path = %output.display(), "failed to save transform");
        ui.toast
            .add_toast(adw::Toast::new("Could not save the transformed image."));
        return;
    }
    let duration_ms = started.elapsed().as_millis() as i64;
    let (width, height) = preview.current.dimensions();

    let mut references = ui.state.borrow().references.clone();
    for reference in &mut references {
        if *reference == preview.source {
            *reference = output.clone();
        }
    }

    if !show_image_with_references(ui, &output, references) {
        ui.toast
            .add_toast(adw::Toast::new("Could not show the transformed image."));
        return;
    }

    if let Some(entry) = source_entry {
        record_transform(
            ui,
            &source_text,
            &output,
            &title,
            &entry,
            duration_ms,
            (width, height),
        );
    }

    ui.undo.borrow_mut().clear();
    ui.status
        .set_text(&format!("{} · {width} × {height}", suffix));
    ui.toast.add_toast(adw::Toast::new(&format!("{suffix}.")));
    ui.transform_button.popdown();
}

/// Restore the original image when the popover closes without applying.
pub(crate) fn cancel(ui: &Rc<Ui>) {
    let preview = ui.transform_preview.borrow_mut().take();
    ui.transform_apply_button.set_sensitive(false);
    if preview.is_some_and(|preview| !preview.operations.is_empty()) {
        ui.status.set_text("Transform cancelled");
        crate::update_image_view(ui);
    }
}

/// Discard a preview because the canvas is changing for another reason.
pub(crate) fn forget(ui: &Ui) {
    ui.transform_preview.borrow_mut().take();
    ui.transform_apply_button.set_sensitive(false);
}

fn record_transform(
    ui: &Rc<Ui>,
    source_text: &str,
    output: &Path,
    title: &str,
    source_entry: &Generation,
    duration_ms: i64,
    (width, height): (u32, u32),
) {
    let image_text = output.to_string_lossy().into_owned();
    let created_at = unix_seconds();
    let inserted = ui.history.borrow().insert(&NewGeneration {
        prompt: title,
        title,
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
                prompt: title.into(),
                title: title.into(),
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

fn texture_for(image: &DynamicImage) -> gdk::Texture {
    let rgba = image.to_rgba8();
    let (width, height) = rgba.dimensions();
    let bytes = glib::Bytes::from_owned(rgba.into_raw());
    gdk::MemoryTexture::new(
        width as i32,
        height as i32,
        gdk::MemoryFormat::R8g8b8a8,
        &bytes,
        (width * 4) as usize,
    )
    .upcast()
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

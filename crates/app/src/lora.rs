//! LoRA selection and weight settings.
//!
//! Weights live in a folder next to the models (`models/loras` by default).
//! The sidebar selects the active LoRAs; Preferences → LoRAs holds the folder
//! controls and the weight of each active LoRA.

use std::cell::RefCell;
use std::fs::File;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::rc::Rc;

use adw::prelude::*;
use imagiflume_core::config::LoraSelection;

use crate::Ui;

#[derive(Clone)]
struct Entry {
    path: String,
    file_name: String,
    label: String,
    size: u64,
    validation: Result<(), String>,
}

fn scan(folder: &Path) -> Vec<Entry> {
    let mut files = Vec::new();
    collect(folder, &mut files);
    files.sort();
    files
        .into_iter()
        .map(|path| {
            let relative = path
                .strip_prefix(folder)
                .unwrap_or(&path)
                .to_string_lossy()
                .replace('\\', "/");
            let file_name = path
                .file_name()
                .map(|name| name.to_string_lossy().into_owned())
                .unwrap_or_else(|| relative.clone());
            let label = Path::new(&file_name)
                .file_stem()
                .map(|name| name.to_string_lossy().into_owned())
                .filter(|name| !name.is_empty())
                .unwrap_or_else(|| file_name.clone());
            let size = std::fs::metadata(&path).map(|meta| meta.len()).unwrap_or(0);
            let validation = validate(folder, &relative);
            Entry {
                path: relative,
                file_name,
                label,
                size,
                validation,
            }
        })
        .collect()
}

fn current_choice(ui: &Rc<Ui>) -> Option<String> {
    let selected = ui.lora_dropdown.selected();
    if selected == gtk::INVALID_LIST_POSITION {
        return None;
    }
    ui.lora_choices.borrow().get(selected as usize).cloned()
}

/// Refresh the sidebar dropdown, chips and empty state.
pub(crate) fn refresh(ui: &Rc<Ui>) {
    let entries = scan(&ui.lora_dir);
    let usable: Vec<Entry> = entries
        .iter()
        .filter(|entry| entry.validation.is_ok())
        .cloned()
        .collect();
    let usable_paths: Vec<&str> = usable.iter().map(|entry| entry.path.as_str()).collect();

    let removed: Vec<String> = {
        let mut config = ui.config.borrow_mut();
        let removed: Vec<String> = config
            .loras
            .iter()
            .filter(|lora| !usable_paths.iter().any(|path| *path == lora.path))
            .map(|lora| {
                Path::new(&lora.path)
                    .file_name()
                    .map(|name| name.to_string_lossy().into_owned())
                    .unwrap_or_else(|| lora.path.clone())
            })
            .collect();
        if !removed.is_empty() {
            config
                .loras
                .retain(|lora| usable_paths.iter().any(|path| *path == lora.path));
            let _ = config.save();
        }
        removed
    };

    let current = current_choice(ui);
    if usable_paths.is_empty() {
        let model = gtk::StringList::new(&["No LoRAs installed"]);
        ui.lora_dropdown.set_model(Some(&model));
        ui.lora_dropdown.set_selected(0);
        ui.lora_dropdown.set_sensitive(false);
        ui.lora_choices.borrow_mut().clear();
    } else {
        let model = gtk::StringList::new(&usable_paths);
        ui.lora_dropdown.set_model(Some(&model));
        *ui.lora_choices.borrow_mut() = usable_paths.iter().map(|path| path.to_string()).collect();
        let index = current
            .and_then(|path| usable_paths.iter().position(|candidate| *candidate == path))
            .unwrap_or(0);
        ui.lora_dropdown.set_selected(index as u32);
        ui.lora_dropdown.set_sensitive(true);
    }

    while let Some(child) = ui.lora_chips.first_child() {
        ui.lora_chips.remove(&child);
    }
    let active = ui.config.borrow().loras.clone();
    for lora in &active {
        let Some(entry) = usable.iter().find(|entry| entry.path == lora.path) else {
            continue;
        };
        let chip = gtk::Box::new(gtk::Orientation::Horizontal, 4);
        chip.add_css_class("lora-chip");
        chip.set_valign(gtk::Align::Center);
        chip.set_tooltip_text(Some(&format!("{} · weight {:.2}", lora.path, lora.weight)));
        let label = gtk::Label::new(Some(&entry.label));
        label.add_css_class("caption");
        label.set_ellipsize(gtk::pango::EllipsizeMode::End);
        label.set_max_width_chars(18);
        chip.append(&label);
        let remove = gtk::Button::builder()
            .icon_name("window-close-symbolic")
            .tooltip_text("Remove this LoRA")
            .valign(gtk::Align::Center)
            .css_classes(["flat"])
            .build();
        {
            let ui = ui.clone();
            let path = lora.path.clone();
            remove.connect_clicked(move |_| {
                if set_lora(&ui, &path, None) {
                    refresh(&ui);
                }
            });
        }
        chip.append(&remove);
        ui.lora_chips.insert(&chip, -1);
    }

    if active.is_empty() && !usable.is_empty() {
        ui.lora_empty.set_text("None selected");
        ui.lora_empty.set_visible(true);
    } else {
        ui.lora_empty.set_visible(false);
    }

    if !removed.is_empty() {
        ui.toast.add_toast(adw::Toast::new(&format!(
            "Removed {}: the file is missing or cannot load.",
            removed.join(", ")
        )));
    }
    update_add_sensitivity(ui);
}

pub(crate) fn update_add_sensitivity(ui: &Rc<Ui>) {
    let selected = current_choice(ui);
    let active = selected.as_ref().is_some_and(|path| {
        ui.config
            .borrow()
            .loras
            .iter()
            .any(|lora| &lora.path == path)
    });
    ui.lora_add.set_sensitive(selected.is_some() && !active);
}

pub(crate) fn add_selected(ui: &Rc<Ui>) {
    let Some(path) = current_choice(ui) else {
        return;
    };
    if ui
        .config
        .borrow()
        .loras
        .iter()
        .any(|lora| lora.path == path)
    {
        update_add_sensitivity(ui);
        return;
    }
    if let Err(reason) = validate(&ui.lora_dir, &path) {
        ui.toast
            .add_toast(adw::Toast::new(&format!("Cannot add {path}: {reason}")));
        refresh(ui);
        return;
    }
    if set_lora(ui, &path, Some(1.0)) {
        refresh(ui);
    }
}

/// Preferences → LoRAs page.
pub(crate) fn build_page(ui: &Rc<Ui>) -> adw::PreferencesPage {
    let page = adw::PreferencesPage::builder()
        .title("LoRAs")
        .icon_name("application-x-addon-symbolic")
        .build();

    let folder = ui.lora_dir.clone();
    if let Err(error) = std::fs::create_dir_all(&folder) {
        tracing::warn!(%error, path = %folder.display(), "failed to create the LoRA folder");
    }

    let folder_group = adw::PreferencesGroup::builder().title("Folder").build();
    let folder_row = adw::ActionRow::builder()
        .title("LoRA folder")
        .subtitle(folder.display().to_string())
        .build();
    let open = gtk::Button::builder()
        .label("Open")
        .valign(gtk::Align::Center)
        .tooltip_text("Open the LoRA folder")
        .build();
    {
        let folder = folder.clone();
        open.connect_clicked(move |_| open_folder(&folder));
    }
    folder_row.add_suffix(&open);
    let rescan = gtk::Button::builder()
        .label("Rescan")
        .valign(gtk::Align::Center)
        .css_classes(["flat"])
        .tooltip_text("Scan the folder again")
        .build();
    folder_row.add_suffix(&rescan);
    folder_group.add(&folder_row);
    page.add(&folder_group);

    let weights = adw::PreferencesGroup::builder()
        .title("Weights")
        .description(
            "LoRAs are model-specific; use weights trained for the active model family. Add or remove active LoRAs from the sidebar; weights apply to the selected LoRAs.",
        )
        .build();
    page.add(&weights);
    let rows: Rc<RefCell<Vec<gtk::Widget>>> = Rc::new(RefCell::new(Vec::new()));
    populate_page(ui, &weights, &folder, &rows);
    {
        let ui = ui.clone();
        let weights = weights.clone();
        let folder = folder.clone();
        let rows = rows.clone();
        rescan.connect_clicked(move |_| populate_page(&ui, &weights, &folder, &rows));
    }

    page
}

fn populate_page(
    ui: &Rc<Ui>,
    group: &adw::PreferencesGroup,
    folder: &Path,
    rows: &Rc<RefCell<Vec<gtk::Widget>>>,
) {
    for row in rows.borrow_mut().drain(..) {
        group.remove(&row);
    }

    let entries = scan(folder);
    if entries.is_empty() {
        let row = adw::ActionRow::builder()
            .title("No LoRAs found")
            .subtitle(format!(
                "Put .safetensors (or .ckpt/.gguf) files in {} and press Rescan.",
                folder.display()
            ))
            .build();
        group.add(&row);
        rows.borrow_mut().push(row.upcast());
        refresh(ui);
        return;
    }

    let active = ui.config.borrow().loras.clone();
    let mut disabled = Vec::new();
    for entry in entries {
        let selected = active.iter().find(|lora| lora.path == entry.path).cloned();
        let mut is_active = selected.is_some() && entry.validation.is_ok();
        if selected.is_some() && entry.validation.is_err() {
            disabled.push(entry.file_name.clone());
            set_lora(ui, &entry.path, None);
            is_active = false;
        }
        let weight_value = selected.map(|lora| lora.weight).unwrap_or(1.0);
        let subtitle = match &entry.validation {
            Ok(()) => format!("{} · {}", entry.path, size_label(entry.size)),
            Err(reason) => format!("{} · {} · {reason}", entry.path, size_label(entry.size)),
        };
        let row = adw::ActionRow::builder()
            .title(&entry.file_name)
            .subtitle(subtitle)
            .build();
        if let Err(reason) = &entry.validation {
            let warning = gtk::Image::from_icon_name("dialog-warning-symbolic");
            warning.set_tooltip_text(Some(reason));
            row.add_prefix(&warning);
        }

        let weight = gtk::SpinButton::with_range(0.0, 2.0, 0.05);
        weight.set_digits(2);
        weight.set_width_chars(5);
        weight.set_value(weight_value as f64);
        weight.set_sensitive(is_active);
        weight.set_tooltip_text(Some(if is_active {
            "LoRA weight"
        } else {
            "Add this LoRA from the sidebar to change its weight"
        }));

        let status = gtk::Label::new(Some(if is_active { "Active" } else { "Not selected" }));
        status.add_css_class("caption");
        if !is_active {
            status.add_css_class("dim-label");
        }

        let suffix = gtk::Box::new(gtk::Orientation::Horizontal, 8);
        suffix.set_valign(gtk::Align::Center);
        suffix.append(&weight);
        suffix.append(&status);
        row.add_suffix(&suffix);
        group.add(&row);
        rows.borrow_mut().push(row.clone().upcast());

        if is_active {
            let ui = ui.clone();
            let path = entry.path.clone();
            weight.connect_value_changed(move |weight| {
                if set_lora(&ui, &path, Some(weight.value() as f32)) {
                    refresh(&ui);
                }
            });
        }
    }

    if !disabled.is_empty() {
        ui.toast.add_toast(adw::Toast::new(&format!(
            "Disabled {}: the file cannot load. Replace it and rescan.",
            disabled.join(", ")
        )));
    }
    refresh(ui);
}
fn set_lora(ui: &Rc<Ui>, path: &str, weight: Option<f32>) -> bool {
    let mut config = ui.config.borrow_mut();
    let previous = config.loras.clone();
    config.loras.retain(|lora| lora.path != path);
    if let Some(weight) = weight {
        config.loras.push(LoraSelection {
            path: path.to_string(),
            weight: weight.clamp(0.0, 2.0),
        });
    }
    match config.save() {
        Ok(()) => true,
        Err(error) => {
            config.loras = previous;
            tracing::warn!(%error, "failed to save the LoRA selection");
            ui.toast
                .add_toast(adw::Toast::new("Could not save the LoRA selection."));
            false
        }
    }
}

/// Check that a weight can plausibly load before sending it to the server.
///
/// stable-diffusion.cpp treats a LoRA it cannot register as a warning and
/// keeps generating without it, so a broken file otherwise looks like a
/// LoRA that simply had no effect. Safetensors headers are cheap to inspect:
/// they declare every tensor's byte range, which catches truncated downloads.
pub(crate) fn validate(folder: &Path, relative: &str) -> Result<(), String> {
    let path = folder.join(relative);
    if !path.is_file() {
        return Err("File not found".into());
    }
    match path
        .extension()
        .and_then(|value| value.to_str())
        .unwrap_or("")
        .to_ascii_lowercase()
        .as_str()
    {
        "safetensors" | "sft" => validate_safetensors(&path),
        "gguf" => validate_gguf(&path),
        // Torch checkpoints are opaque without a torch reader; let the
        // server be the judge instead of rejecting every one of them.
        _ => Ok(()),
    }
}

fn validate_safetensors(path: &Path) -> Result<(), String> {
    let mut file = File::open(path).map_err(|error| format!("Cannot read file: {error}"))?;
    let file_len = file
        .metadata()
        .map_err(|error| format!("Cannot read file: {error}"))?
        .len();
    let mut length = [0u8; 8];
    file.read_exact(&mut length)
        .map_err(|_| "Not a valid safetensors file".to_string())?;
    let header_len = u64::from_le_bytes(length);
    const MAX_HEADER: u64 = 64 * 1024 * 1024;
    if header_len == 0 || header_len > MAX_HEADER || header_len.saturating_add(8) > file_len {
        return Err("Not a valid safetensors file".into());
    }
    let mut header = vec![0u8; header_len as usize];
    file.read_exact(&mut header)
        .map_err(|_| "Not a valid safetensors file".to_string())?;
    let parsed: serde_json::Value =
        serde_json::from_slice(&header).map_err(|_| "Not a valid safetensors file".to_string())?;
    let Some(tensors) = parsed.as_object() else {
        return Err("Not a valid safetensors file".into());
    };

    let data_len = file_len - 8 - header_len;
    let mut declared = 0u64;
    let mut looks_like_lora = false;
    for (name, tensor) in tensors {
        if name == "__metadata__" {
            continue;
        }
        if name.to_ascii_lowercase().contains("lora") {
            looks_like_lora = true;
        }
        let offsets = tensor
            .get("data_offsets")
            .and_then(|value| value.as_array())
            .filter(|offsets| offsets.len() == 2)
            .and_then(|offsets| Some((offsets[0].as_u64()?, offsets[1].as_u64()?)));
        let Some((start, end)) = offsets else {
            return Err("Not a valid safetensors file".into());
        };
        if start > end {
            return Err("Not a valid safetensors file".into());
        }
        declared = declared.max(end);
    }
    if !looks_like_lora {
        return Err("No LoRA tensors found".into());
    }
    if declared > data_len {
        let expected = file_len - data_len + declared;
        return Err(format!(
            "Incomplete file ({} of {})",
            size_label(file_len),
            size_label(expected)
        ));
    }
    Ok(())
}

fn validate_gguf(path: &Path) -> Result<(), String> {
    let mut magic = [0u8; 4];
    File::open(path)
        .and_then(|mut file| file.read_exact(&mut magic))
        .map_err(|_| "Not a valid GGUF file".to_string())?;
    if &magic == b"GGUF" {
        Ok(())
    } else {
        Err("Not a valid GGUF file".into())
    }
}

fn collect(dir: &Path, files: &mut Vec<PathBuf>) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            collect(&path, files);
            continue;
        }
        let extension = path
            .extension()
            .and_then(|value| value.to_str())
            .unwrap_or("")
            .to_ascii_lowercase();
        if matches!(
            extension.as_str(),
            "safetensors" | "ckpt" | "sft" | "gguf" | "pt" | "pth"
        ) {
            files.push(path);
        }
    }
}

fn open_folder(folder: &Path) {
    if let Err(error) = std::fs::create_dir_all(folder) {
        tracing::warn!(%error, path = %folder.display(), "failed to create the LoRA folder");
        return;
    }
    let file = gtk::gio::File::for_path(folder);
    if let Err(error) =
        gtk::gio::AppInfo::launch_default_for_uri(&file.uri(), None::<&gtk::gio::AppLaunchContext>)
    {
        tracing::warn!(%error, path = %folder.display(), "failed to open the LoRA folder");
    }
}

fn size_label(bytes: u64) -> String {
    const MB: u64 = 1024 * 1024;
    const GB: u64 = 1024 * MB;
    if bytes >= GB {
        format!("{:.1} GB", bytes as f64 / GB as f64)
    } else if bytes >= MB {
        format!("{:.0} MB", bytes as f64 / MB as f64)
    } else {
        format!("{:.0} KB", bytes as f64 / 1024.0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs::File;
    use std::io::Write;

    fn test_dir(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "imagiflume-lora-validation-{name}-{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn lora_header() -> serde_json::Value {
        serde_json::json!({
            "lora_unet_double_blocks_0.lora_A.weight": {
                "dtype": "F32", "shape": [1, 4], "data_offsets": [0, 4]
            },
            "lora_unet_double_blocks_0.lora_B.weight": {
                "dtype": "F32", "shape": [4, 1], "data_offsets": [4, 8]
            }
        })
    }

    fn write_safetensors(path: &Path, header: &serde_json::Value, data: &[u8]) {
        let header = serde_json::to_vec(header).unwrap();
        let mut file = File::create(path).unwrap();
        file.write_all(&(header.len() as u64).to_le_bytes())
            .unwrap();
        file.write_all(&header).unwrap();
        file.write_all(data).unwrap();
    }

    #[test]
    fn accepts_a_complete_lora() {
        let dir = test_dir("complete");
        write_safetensors(&dir.join("ok.safetensors"), &lora_header(), &[0u8; 8]);
        assert_eq!(validate(&dir, "ok.safetensors"), Ok(()));
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn rejects_a_truncated_lora() {
        let dir = test_dir("truncated");
        write_safetensors(&dir.join("short.safetensors"), &lora_header(), &[0u8; 4]);
        let error = validate(&dir, "short.safetensors").unwrap_err();
        assert!(error.starts_with("Incomplete file"), "{error}");
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn rejects_a_file_without_lora_tensors() {
        let dir = test_dir("plain");
        let header = serde_json::json!({
            "model.weight": {"dtype": "F32", "shape": [1], "data_offsets": [0, 4]}
        });
        write_safetensors(&dir.join("plain.safetensors"), &header, &[0u8; 4]);
        assert_eq!(
            validate(&dir, "plain.safetensors"),
            Err("No LoRA tensors found".into())
        );
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn rejects_a_missing_file() {
        let dir = test_dir("missing");
        assert_eq!(
            validate(&dir, "nope.safetensors"),
            Err("File not found".into())
        );
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn checks_the_gguf_magic() {
        let dir = test_dir("gguf");
        std::fs::write(dir.join("good.gguf"), b"GGUF\x03\x00\x00\x00").unwrap();
        std::fs::write(dir.join("bad.gguf"), b"NOPE").unwrap();
        assert_eq!(validate(&dir, "good.gguf"), Ok(()));
        assert_eq!(
            validate(&dir, "bad.gguf"),
            Err("Not a valid GGUF file".into())
        );
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn accepts_opaque_torch_checkpoints() {
        let dir = test_dir("checkpoint");
        std::fs::write(dir.join("weights.pt"), b"not really a torch file").unwrap();
        assert_eq!(validate(&dir, "weights.pt"), Ok(()));
        let _ = std::fs::remove_dir_all(dir);
    }
}

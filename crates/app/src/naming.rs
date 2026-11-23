//! Human-readable history names and file names.
//!
//! A text-to-image result gets a base title from llama.cpp (or a prompt
//! fallback). Derived images keep that base and append a short suffix, so an
//! upscale of "Standing fox in a forest" becomes
//! "Standing fox in a forest - Upscaled 2x".

use std::path::{Path, PathBuf};

use futures_util::future::{Either, select};
use gtk::glib;
use imagiflume_core::preset::ModelPreset;

use crate::{GenerationMode, Ui, history_view, prompt_ai, queue};

const MAX_TITLE_CHARS: usize = 80;

/// Short, deterministic title from a prompt, used when llama.cpp is unavailable.
pub(crate) fn fallback_title(prompt: &str) -> String {
    let first_clause = prompt
        .split(['.', ',', ';', '!', '?', '\n'])
        .next()
        .unwrap_or(prompt)
        .trim();
    let mut words: Vec<&str> = first_clause.split_whitespace().collect();
    while let Some(word) = words.first() {
        let normalized = word
            .trim_matches(|character: char| !character.is_alphanumeric())
            .to_lowercase();
        if is_leading_filler(&normalized) {
            words.remove(0);
        } else {
            break;
        }
    }
    let title = words.into_iter().take(6).collect::<Vec<_>>().join(" ");
    let title = clean_title(&title);
    if title.is_empty() {
        "Untitled image".to_string()
    } else {
        capitalize_first(&title)
    }
}

/// Start a llama.cpp title run for a text-to-image prompt.
pub(crate) fn text_to_image_title_task(
    ui: &Ui,
    model: &'static ModelPreset,
    prompt: &str,
) -> Option<async_channel::Receiver<Result<String, String>>> {
    let (cli, configured) = {
        let config = ui.config.borrow();
        (
            prompt_ai::find_cli(&config),
            config.prompt_helper_model.clone(),
        )
    };
    let cli = cli?;
    let encoder = configured.or_else(|| {
        model
            .llm_encoder()
            .map(|file| ui.models_dir.join(file.rel_path))
    })?;
    if !encoder.exists() {
        return None;
    }

    let prompt = prompt.to_string();
    let (sender, receiver) = async_channel::bounded(1);
    std::thread::spawn(move || {
        let result = prompt_ai::title(&prompt, &encoder, &cli).map_err(|error| error.to_string());
        let _ = sender.send_blocking(result);
    });
    Some(receiver)
}

/// Wait for a title run, falling back after a short grace period.
pub(crate) async fn await_title(
    task: Option<async_channel::Receiver<Result<String, String>>>,
    fallback: String,
) -> String {
    let Some(receiver) = task else {
        return fallback;
    };
    let timeout = glib::timeout_future_seconds(6);
    match select(Box::pin(receiver.recv()), timeout).await {
        Either::Left((Ok(Ok(title)), _)) => {
            let title = clean_title(&title);
            if title.is_empty() { fallback } else { title }
        }
        _ => fallback,
    }
}

/// Title for an edit or effect, derived from its source entry.
pub(crate) fn generation_title(ui: &Ui, generate: &queue::GenerateJob) -> String {
    let source = generate
        .reference_paths
        .first()
        .map(PathBuf::as_path)
        .or_else(|| generate.source_path.as_deref().map(Path::new));
    let base = source
        .map(|path| base_for_path(ui, path))
        .unwrap_or_else(|| "Image".to_string());
    let suffix = if matches!(generate.mode, GenerationMode::Edit | GenerationMode::Custom) {
        edit_suffix(&generate.prompt)
    } else if generate.mode.kind() != "generate" {
        history_view::kind_label(generate.mode.kind())
            .unwrap_or("Effect")
            .to_string()
    } else {
        String::new()
    };
    if suffix.is_empty() {
        base
    } else if base.is_empty() {
        suffix
    } else {
        format!("{base} - {suffix}")
    }
}

/// Suffix for an edit, taken from the user's edit instruction.
pub(crate) fn edit_suffix(prompt: &str) -> String {
    let suffix = fallback_title(prompt);
    if suffix == "Untitled image" {
        "Edited".to_string()
    } else {
        suffix
    }
}

/// Base title of an image, from History or its file stem for cache references.
pub(crate) fn base_for_path(ui: &Ui, path: &Path) -> String {
    let text = path.to_string_lossy();
    if let Ok(Some(entry)) = ui.history.borrow().find_by_image_path(&text) {
        let title = entry.title.trim();
        if !title.is_empty() {
            return title.to_string();
        }
        let prompt = entry.prompt.trim();
        if !prompt.is_empty() {
            return fallback_title(prompt);
        }
    }

    let stem = path
        .file_stem()
        .and_then(|stem| stem.to_str())
        .unwrap_or("")
        .trim();
    if stem.is_empty()
        || stem.starts_with("camera-")
        || stem.starts_with("paste-")
        || stem.starts_with("crop-")
        || stem.starts_with("transform-")
        || stem.starts_with("imported-")
    {
        "Image".to_string()
    } else {
        clean_title(stem)
    }
}

/// Base title plus a suffix, e.g. `Standing fox in a forest - Rotated +90`.
pub(crate) fn derived(ui: &Ui, source: &Path, suffix: &str) -> String {
    let base = base_for_path(ui, source);
    let suffix = clean_title(suffix);
    if suffix.is_empty() {
        base
    } else if base.is_empty() {
        suffix
    } else {
        format!("{base} - {suffix}")
    }
}

/// Pick a free `title.png`, `title-2.png`, … path in `directory`.
pub(crate) fn unique_output_path(directory: &Path, title: &str) -> PathBuf {
    let base = title_slug(title);
    let mut candidate = directory.join(format!("{base}.png"));
    let mut counter = 2;
    while candidate.exists() {
        candidate = directory.join(format!("{base}-{counter}.png"));
        counter += 1;
    }
    candidate
}

/// Rename `path` to a title-based file name. Returns the path to use.
pub(crate) fn rename_to_title(path: &Path, title: &str) -> PathBuf {
    let Some(directory) = path.parent() else {
        return path.to_path_buf();
    };
    let target = unique_output_path(directory, title);
    if target == path {
        return path.to_path_buf();
    }
    match std::fs::rename(path, &target) {
        Ok(()) => target,
        Err(error) => {
            tracing::warn!(%error, path = %path.display(), "failed to rename the image");
            path.to_path_buf()
        }
    }
}

/// Lowercase, hyphenated file name for a title.
pub(crate) fn title_slug(title: &str) -> String {
    let mut words: Vec<String> = Vec::new();
    for word in title.split_whitespace() {
        let cleaned: String = word
            .chars()
            .filter(|character| character.is_alphanumeric())
            .collect();
        if !cleaned.is_empty() {
            words.push(cleaned.to_lowercase());
        }
    }
    let mut slug = words.join("-");
    if slug.chars().count() > MAX_TITLE_CHARS {
        slug = slug.chars().take(MAX_TITLE_CHARS).collect();
        while slug.ends_with('-') {
            slug.pop();
        }
    }
    if slug.is_empty() {
        "image".to_string()
    } else {
        slug
    }
}

pub(crate) fn capitalize_first(text: &str) -> String {
    let mut characters = text.chars();
    match characters.next() {
        Some(first) => first.to_uppercase().collect::<String>() + characters.as_str(),
        None => String::new(),
    }
}

fn clean_title(text: &str) -> String {
    text.trim()
        .trim_matches(|character: char| {
            matches!(
                character,
                '"' | '\'' | '“' | '”' | '‘' | '’' | '`' | '*' | '#' | '-'
            )
        })
        .trim()
        .trim_end_matches(['.', ',', ';', ':', '!', '?'])
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
}

fn is_leading_filler(word: &str) -> bool {
    matches!(
        word,
        "a" | "an"
            | "the"
            | "reimagine"
            | "create"
            | "generate"
            | "make"
            | "draw"
            | "paint"
            | "render"
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fallback_titles_are_short_and_descriptive() {
        assert_eq!(
            fallback_title(
                "A fox stands in a sunlit forest clearing, its fur golden-brown with a black tail."
            ),
            "Fox stands in a sunlit forest"
        );
        assert_eq!(fallback_title("!!! ???"), "Untitled image");
        assert_eq!(
            fallback_title("Reimagine the scene from a photo"),
            "Scene from a photo"
        );
    }

    #[test]
    fn slugs_are_lowercase_and_bounded() {
        assert_eq!(
            title_slug("Standing fox - Rotated +90"),
            "standing-fox-rotated-90"
        );
        let long =
            "A very long title that keeps going past the point of usefulness and should be cut";
        let slug = title_slug(long);
        assert!(slug.chars().count() <= MAX_TITLE_CHARS, "{slug}");
        assert!(!slug.ends_with('-'));
    }

    #[test]
    fn titles_capitalize_the_first_letter() {
        assert_eq!(capitalize_first("standing fox"), "Standing fox");
        assert_eq!(capitalize_first(""), "");
    }
}

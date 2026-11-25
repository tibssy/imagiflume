//! Prompt rewriting and small vision decisions through llama.cpp.
//!
//! The stable-diffusion.cpp server keeps the encoder for conditioning only and
//! has no text-generation API, so this spawns `llama-cli` with the same GGUF
//! file for a few seconds. The optional day/night helper uses the small
//! LFM2.5-VL model plus its multimodal projector instead.

use std::collections::HashMap;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant, UNIX_EPOCH};

use anyhow::{Context, Result, anyhow, bail};
use imagiflume_core::Config;

const SYSTEM_PROMPT: &str = "\
Rewrite the user's image-generation prompt to be more specific and visually \
descriptive while preserving their intent. Add details about lighting, \
composition, materials and atmosphere, but do not add new subjects or change \
the scene. Keep it under 60 words. Return only the improved prompt wrapped in \
<prompt> and </prompt> tags.";

const EDIT_SYSTEM_PROMPT: &str = r#"Rewrite the user's edit request into one short instruction (max 20 words). Use the current image description, or the original scene and previous edits when there is no current description, to identify the subject and what stays the same. Name the concrete change. If the request is a vague style word, name a concrete visual style. Do not describe the whole scene or add motion. Examples: "make it eat" -> "Make the main subject hold food in its mouth, keeping its pose and background." "make it night" -> "Turn the scene into night with moonlight and deep shadows, keeping the composition unchanged." "make it animated" -> "Turn the main subject into a 2D animated cartoon with bold outlines and flat colors, keeping its pose and background." Return only the instruction wrapped in <prompt> and </prompt> tags."#;

const DESCRIBE_PROMPT: &str = "\
Describe this image in one concise paragraph for an image-editing assistant. \
Include the main subject, its pose and position, the setting, lighting, colors \
and art style. Do not speculate or add details that are not visible. Return \
only the description wrapped in <description> and </description> tags.";

const TITLE_PROMPT: &str = "\
Write a short, descriptive title for an image from the user's generation prompt. \
Use 2 to 6 words, no quotes and no trailing punctuation. Do not mention loading, \
models, prompts or instructions. Return only the title wrapped in <title> and \
</title> tags.";

const TIMEOUT: Duration = Duration::from_secs(120);
const MAX_TOKENS: u32 = 220;
const EDIT_MAX_TOKENS: u32 = 90;
const DESCRIBE_TIMEOUT: Duration = Duration::from_secs(60);
const DESCRIBE_MAX_TOKENS: u32 = 180;
const TITLE_TIMEOUT: Duration = Duration::from_secs(60);
const TITLE_MAX_TOKENS: u32 = 32;
const DESCRIPTION_CACHE_LIMIT: usize = 64;
/// Titles are cheap to reuse, and serialising the runs avoids four llama-cli
/// processes for a four-image batch.
static TITLE_CACHE: OnceLock<Mutex<HashMap<String, String>>> = OnceLock::new();
static DESCRIPTION_CACHE: OnceLock<Mutex<HashMap<String, String>>> = OnceLock::new();
const VISION_PROMPT: &str =
    "Is this a night scene or a daylight scene? Answer with one word: night or daylight.";
const VISION_TIMEOUT: Duration = Duration::from_secs(60);

/// Locate `llama-cli` from config, the environment, or `PATH`.
pub(crate) fn find_cli(config: &Config) -> Option<PathBuf> {
    if let Some(path) = &config.prompt_helper_cli
        && is_executable(path)
    {
        return Some(path.clone());
    }
    if let Ok(path) = std::env::var("IMAGIFLUME_LLAMA_CLI") {
        let path = PathBuf::from(path);
        if is_executable(&path) {
            return Some(path);
        }
    }
    // Bundled and system-wide installs live next to the app or under
    // /usr/lib/imagiflume, neither of which is necessarily on PATH.
    if let Some(path) = imagiflume_core::runtime::find_system_llama_cli()
        && is_executable(&path)
    {
        return Some(path);
    }
    let path_var = std::env::var_os("PATH")?;
    for directory in std::env::split_paths(&path_var) {
        let candidate = directory.join("llama-cli");
        if is_executable(&candidate) {
            return Some(candidate);
        }
    }
    None
}

fn is_executable(path: &Path) -> bool {
    if !path.is_file() {
        return false;
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        path.metadata()
            .map(|metadata| metadata.permissions().mode() & 0o111 != 0)
            .unwrap_or(false)
    }
    #[cfg(not(unix))]
    {
        true
    }
}

/// Rewrite `prompt` and stream the tagged answer to `on_chunk`.
///
/// `context` is an optional block describing the current image and the edit
/// history. When present, the edit system prompt is used and the model is
/// asked for a complete target-scene description instead of a generic rewrite.
/// Returns `None` when `cancel` was set before completion.
pub(crate) fn improve_streaming<F>(
    prompt: &str,
    context: Option<&str>,
    encoder: &Path,
    cli: &Path,
    cancel: Arc<AtomicBool>,
    on_chunk: F,
) -> Result<Option<String>>
where
    F: FnMut(&str) + Send + 'static,
{
    if !encoder.exists() {
        bail!("text encoder not found at {}", encoder.display());
    }
    let command = match context {
        Some(context) if !context.trim().is_empty() => {
            build_edit_command(cli, encoder, context.trim(), prompt)
        }
        _ => build_command(cli, encoder, prompt),
    };
    run_streaming(command, TIMEOUT, cancel, on_chunk)
}

/// Ask `encoder` for a short display title for the prompt.
pub(crate) fn title(prompt: &str, encoder: &Path, cli: &Path) -> Result<String> {
    if !encoder.exists() {
        bail!("text encoder not found at {}", encoder.display());
    }

    let key = format!("{}\n{}", encoder.display(), prompt);
    let cache = TITLE_CACHE.get_or_init(|| Mutex::new(HashMap::new()));
    let mut cache = cache
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    if let Some(title) = cache.get(&key) {
        return Ok(title.clone());
    }

    let stdout = run_command(build_title_command(cli, encoder, prompt), TITLE_TIMEOUT)?;
    let title = parse_title(&stdout).ok_or_else(|| anyhow!("the model did not return a title"))?;
    // Some text encoders echo a truncated prompt instead of titling the image.
    if title.split_whitespace().count() >= 4
        && prompt
            .trim()
            .to_lowercase()
            .starts_with(&title.to_lowercase())
    {
        bail!("the model echoed the prompt instead of titling it");
    }
    cache.insert(key, title.clone());
    Ok(title)
}

/// Ask the vision helper whether `image` is a night scene.
pub(crate) fn classify_lighting(
    image: &Path,
    model: &Path,
    mmproj: &Path,
    cli: &Path,
) -> Result<bool> {
    if !model.exists() {
        bail!("vision model not found at {}", model.display());
    }
    if !mmproj.exists() {
        bail!("vision projector not found at {}", mmproj.display());
    }
    if !image.exists() {
        bail!("image not found at {}", image.display());
    }

    let stdout = run_command(
        build_vision_command(cli, model, mmproj, image),
        VISION_TIMEOUT,
    )?;
    parse_lighting(&stdout)
        .ok_or_else(|| anyhow!("the vision model did not return night or daylight"))
}

/// Describe `image` with the small LFM vision helper.
///
/// The description is cached by image, mtime and model so repeated Improve
/// clicks on the same reference are instant.
pub(crate) fn describe_image(
    image: &Path,
    model: &Path,
    mmproj: &Path,
    cli: &Path,
) -> Result<String> {
    if !model.exists() {
        bail!("vision model not found at {}", model.display());
    }
    if !mmproj.exists() {
        bail!("vision projector not found at {}", mmproj.display());
    }
    if !image.exists() {
        bail!("image not found at {}", image.display());
    }

    let key = description_key(image, model, mmproj);
    let cache = DESCRIPTION_CACHE.get_or_init(|| Mutex::new(HashMap::new()));
    {
        let cache = cache
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if let Some(description) = cache.get(&key) {
            return Ok(description.clone());
        }
    }

    let stdout = run_command(
        build_describe_command(cli, model, mmproj, image),
        DESCRIBE_TIMEOUT,
    )?;
    let description = parse_description(&stdout)
        .ok_or_else(|| anyhow!("the vision model did not return a description"))?;

    let mut cache = cache
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    if cache.len() >= DESCRIPTION_CACHE_LIMIT {
        cache.clear();
    }
    cache.insert(key, description.clone());
    Ok(description)
}

fn description_key(image: &Path, model: &Path, mmproj: &Path) -> String {
    let modified = std::fs::metadata(image)
        .and_then(|metadata| metadata.modified())
        .ok()
        .and_then(|time| time.duration_since(UNIX_EPOCH).ok())
        .map(|duration| duration.as_nanos())
        .unwrap_or(0);
    format!(
        "{}\n{}\n{}\n{}",
        image.display(),
        modified,
        model.display(),
        mmproj.display()
    )
}

fn run_command(mut command: Command, timeout: Duration) -> Result<String> {
    if let Some(parent) = log_path().parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    let program = command.get_program().to_string_lossy().into_owned();
    let mut child = command
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .with_context(|| format!("failed to start {program}"))?;

    let stdout = child.stdout.take().expect("piped stdout");
    let stderr = child.stderr.take().expect("piped stderr");
    let stdout_reader = std::thread::spawn(move || read_all(stdout));
    let stderr_reader = std::thread::spawn(move || read_all(stderr));

    let started = Instant::now();
    let status = loop {
        if let Some(status) = child.try_wait()? {
            break status;
        }
        if started.elapsed() > timeout {
            let _ = child.kill();
            let _ = child.wait();
            bail!("llama-cli timed out after {}s", timeout.as_secs());
        }
        std::thread::sleep(Duration::from_millis(100));
    };

    let stdout = stdout_reader.join().unwrap_or_default();
    let stderr = stderr_reader.join().unwrap_or_default();

    if !status.success() {
        bail!("llama-cli exited with {status}{}", stderr_tail(&stderr));
    }

    Ok(stdout)
}

/// Run a command and stream `<prompt>…</prompt>` text to `on_chunk`.
///
/// Returns `None` if `cancel` was set. When the model ignores the tags, the
/// complete output is passed through `extract()` at the end.
fn run_streaming<F>(
    mut command: Command,
    timeout: Duration,
    cancel: Arc<AtomicBool>,
    mut on_chunk: F,
) -> Result<Option<String>>
where
    F: FnMut(&str) + Send + 'static,
{
    if let Some(parent) = log_path().parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    let program = command.get_program().to_string_lossy().into_owned();
    let mut child = command
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .with_context(|| format!("failed to start {program}"))?;

    let stdout = child.stdout.take().expect("piped stdout");
    let stderr = child.stderr.take().expect("piped stderr");
    let reader_cancel = cancel.clone();
    let reader = std::thread::spawn(move || {
        let mut extractor = PromptExtractor::default();
        let mut reader = stdout;
        let mut buffer = [0u8; 2048];
        loop {
            if reader_cancel.load(Ordering::SeqCst) {
                break;
            }
            match reader.read(&mut buffer) {
                Ok(0) => break,
                Ok(read) => extractor.feed_bytes(&buffer[..read], &mut on_chunk),
                Err(error) if error.kind() == std::io::ErrorKind::Interrupted => continue,
                Err(_) => break,
            }
        }
        extractor.finish(&mut on_chunk);
        (extractor.raw, extractor.streamed)
    });
    let stderr_reader = std::thread::spawn(move || read_all(stderr));

    let started = Instant::now();
    let status = loop {
        if cancel.load(Ordering::SeqCst) {
            let _ = child.kill();
            let _ = child.wait();
            break None;
        }
        if let Some(status) = child.try_wait()? {
            break Some(status);
        }
        if started.elapsed() > timeout {
            let _ = child.kill();
            let _ = child.wait();
            let _ = reader.join();
            let _ = stderr_reader.join();
            bail!("llama-cli timed out after {}s", timeout.as_secs());
        }
        std::thread::sleep(Duration::from_millis(100));
    };

    let (raw, streamed) = reader.join().unwrap_or_default();
    let stderr = stderr_reader.join().unwrap_or_default();

    if status.is_none() || cancel.load(Ordering::SeqCst) {
        return Ok(None);
    }
    let status = status.expect("status checked");
    if !status.success() {
        bail!("llama-cli exited with {status}{}", stderr_tail(&stderr));
    }
    if !streamed.trim().is_empty() {
        Ok(Some(clean(&streamed)))
    } else {
        Ok(extract(&raw))
    }
}

#[derive(Default)]
struct PromptExtractor {
    raw: String,
    streamed: String,
    start: Option<usize>,
    emitted: usize,
    done: bool,
    ansi: AnsiStripper,
    pending: Vec<u8>,
}

impl PromptExtractor {
    fn feed_bytes(&mut self, bytes: &[u8], on_chunk: &mut dyn FnMut(&str)) {
        self.pending.extend_from_slice(bytes);
        loop {
            match std::str::from_utf8(&self.pending) {
                Ok(text) => {
                    let text = text.to_string();
                    self.pending.clear();
                    self.feed_text(&text, on_chunk);
                    break;
                }
                Err(error) => {
                    let valid = error.valid_up_to();
                    if valid > 0 {
                        let text = std::str::from_utf8(&self.pending[..valid])
                            .expect("valid UTF-8 prefix")
                            .to_string();
                        self.feed_text(&text, on_chunk);
                        self.pending.drain(..valid);
                    }
                    if error.error_len().is_some() {
                        if !self.pending.is_empty() {
                            self.pending.drain(..1);
                        }
                    } else {
                        break;
                    }
                }
            }
        }
    }

    fn feed_text(&mut self, text: &str, on_chunk: &mut dyn FnMut(&str)) {
        let text = self.ansi.push(text);
        if text.is_empty() {
            return;
        }
        self.raw.push_str(&text);
        if self.done {
            return;
        }
        if self.start.is_none() {
            if let Some(index) = self.raw.find("<prompt>") {
                let start = index + "<prompt>".len();
                self.start = Some(start);
                self.emitted = start;
            } else {
                return;
            }
        }
        let start = self.start.expect("start set");
        if let Some(end) = self.raw[start..].find("</prompt>") {
            self.emit_until(start + end, on_chunk);
            self.done = true;
        } else {
            let mut safe = self.raw.len().saturating_sub("</prompt>".len() - 1);
            while safe > self.emitted && !self.raw.is_char_boundary(safe) {
                safe -= 1;
            }
            self.emit_until(safe, on_chunk);
        }
    }

    fn emit_until(&mut self, end: usize, on_chunk: &mut dyn FnMut(&str)) {
        if end <= self.emitted {
            return;
        }
        let chunk = self.raw[self.emitted..end].to_string();
        self.streamed.push_str(&chunk);
        on_chunk(&chunk);
        self.emitted = end;
    }

    fn finish(&mut self, on_chunk: &mut dyn FnMut(&str)) {
        if !self.pending.is_empty() {
            let text = String::from_utf8_lossy(&self.pending).into_owned();
            self.pending.clear();
            self.feed_text(&text, on_chunk);
        }
        if self.start.is_some() && !self.done {
            let end = self.raw.len();
            self.emit_until(end, on_chunk);
            self.done = true;
        }
    }
}

#[derive(Default)]
struct AnsiStripper {
    escaping: bool,
}

impl AnsiStripper {
    fn push(&mut self, text: &str) -> String {
        let mut output = String::with_capacity(text.len());
        for character in text.chars() {
            if self.escaping {
                if character.is_ascii_alphabetic() {
                    self.escaping = false;
                }
                continue;
            }
            if character == '\u{1b}' {
                self.escaping = true;
            } else {
                output.push(character);
            }
        }
        output
    }
}

/// Persistent llama.cpp log, next to the sd-server log.
pub(crate) fn log_path() -> PathBuf {
    imagiflume_core::paths::cache_dir().join("llama-cli.log")
}

/// Devices reported by the selected helper, used to detect CPU fallback.
pub(crate) fn list_devices(cli: &Path) -> Option<String> {
    let output = Command::new(cli).arg("--list-devices").output().ok()?;
    if !output.status.success() {
        return None;
    }
    Some(String::from_utf8_lossy(&output.stdout).into_owned())
}

fn build_command(cli: &Path, encoder: &Path, prompt: &str) -> Command {
    build_prompt_command(cli, encoder, SYSTEM_PROMPT, prompt, MAX_TOKENS)
}

fn build_edit_command(cli: &Path, encoder: &Path, context: &str, instruction: &str) -> Command {
    let prompt = format!("{context}\nNew edit: {instruction}");
    build_prompt_command(cli, encoder, EDIT_SYSTEM_PROMPT, &prompt, EDIT_MAX_TOKENS)
}

fn build_prompt_command(
    cli: &Path,
    encoder: &Path,
    system: &str,
    prompt: &str,
    max_tokens: u32,
) -> Command {
    let mut command = Command::new(cli);
    command
        .arg("-m")
        .arg(encoder)
        .arg("-sys")
        .arg(system)
        .arg("-p")
        .arg(prompt)
        .arg("-st")
        .arg("--no-display-prompt")
        .arg("--color")
        .arg("off")
        // Qwen3 base spends the whole budget on chain-of-thought otherwise.
        .arg("-rea")
        .arg("off")
        .arg("-c")
        .arg("4096")
        .arg("--no-warmup")
        .arg("-n")
        .arg(max_tokens.to_string())
        .arg("--temp")
        .arg("0.4")
        .arg("-t")
        .arg(threads().to_string())
        .arg("--simple-io")
        .arg("--log-file")
        .arg(log_path())
        .arg("-lv")
        .arg("4");
    command
}

fn build_title_command(cli: &Path, encoder: &Path, prompt: &str) -> Command {
    let mut command = Command::new(cli);
    command
        .arg("-m")
        .arg(encoder)
        .arg("-sys")
        .arg(TITLE_PROMPT)
        .arg("-p")
        .arg(prompt)
        .arg("-st")
        .arg("--no-display-prompt")
        .arg("--color")
        .arg("off")
        .arg("-rea")
        .arg("off")
        .arg("-c")
        .arg("2048")
        .arg("--no-warmup")
        .arg("-n")
        .arg(TITLE_MAX_TOKENS.to_string())
        .arg("--temp")
        .arg("0.3")
        .arg("-t")
        // Title runs in the background; keep them gentle on CPU-only rigs.
        .arg(threads().min(2).to_string())
        .arg("--simple-io");
    command
}

fn parse_title(stdout: &str) -> Option<String> {
    let text = strip_ansi(stdout);

    let candidate = if let Some(start) = text.find("<title>") {
        let rest = &text[start + "<title>".len()..];
        let end = rest
            .find("</title>")
            .or_else(|| rest.find("\n[ Prompt:"))
            .unwrap_or(rest.len());
        rest[..end].trim()
    } else {
        // Without tags, skip llama-cli's loading/benchmark chatter and use the
        // last meaningful line, which is where the answer appears.
        let mut answer = "";
        for line in text.lines().map(str::trim) {
            let lower = line.to_lowercase();
            if line.is_empty()
                || lower.starts_with("loading")
                || lower.starts_with("load ")
                || lower.starts_with("llama_")
                || lower.starts_with("llama.")
                || lower.starts_with("ggml")
                || lower.starts_with("build:")
                || lower.starts_with("main:")
                || lower.starts_with("system_info")
                || lower.starts_with("print_info")
                || lower.starts_with("common_")
                || lower.starts_with("using ")
                || lower.starts_with("available ")
                || lower.starts_with("exiting")
                || lower.starts_with("[ prompt")
                || lower.starts_with("[ generation")
            {
                continue;
            }
            answer = line;
        }
        answer
    };

    let line = candidate.trim().trim_start_matches('>').trim();
    let line = line
        .strip_prefix("Title:")
        .or_else(|| line.strip_prefix("title:"))
        .unwrap_or(line)
        .trim();
    let line = line.trim();
    // Drop leaked control tokens such as </s>.
    let line = line.split('<').next().unwrap_or(line).trim();
    let line = line
        .trim_matches(|c: char| {
            matches!(
                c,
                '"' | '\'' | '“' | '”' | '‘' | '’' | '`' | '*' | '#' | '-'
            )
        })
        .trim();
    let title = line
        .split_whitespace()
        .take(8)
        .collect::<Vec<_>>()
        .join(" ");
    let title = title
        .trim_end_matches(['.', ',', ';', ':', '!', '?'])
        .trim();
    let lower = title.to_lowercase();
    if title.chars().count() < 2
        || title.chars().count() > 80
        || title.starts_with('<')
        || title.starts_with("```")
        || lower.contains("loading model")
        || lower.contains("short, descriptive title")
        || lower.contains("return only the title")
        || lower.contains("user's prompt")
        || lower.starts_with("here is")
        || lower.starts_with("here's")
    {
        return None;
    }
    Some(title.to_string())
}

fn build_vision_command(cli: &Path, model: &Path, mmproj: &Path, image: &Path) -> Command {
    build_vision_command_with(cli, model, mmproj, image, VISION_PROMPT, 8, 2048)
}

fn build_describe_command(cli: &Path, model: &Path, mmproj: &Path, image: &Path) -> Command {
    build_vision_command_with(
        cli,
        model,
        mmproj,
        image,
        DESCRIBE_PROMPT,
        DESCRIBE_MAX_TOKENS,
        4096,
    )
}

fn build_vision_command_with(
    cli: &Path,
    model: &Path,
    mmproj: &Path,
    image: &Path,
    prompt: &str,
    max_tokens: u32,
    context: u32,
) -> Command {
    let mut command = Command::new(cli);
    command
        .arg("-m")
        .arg(model)
        .arg("--mmproj")
        .arg(mmproj)
        .arg("--image")
        .arg(image)
        .arg("-p")
        .arg(prompt)
        .arg("-st")
        .arg("--no-display-prompt")
        .arg("--color")
        .arg("off")
        .arg("-c")
        .arg(context.to_string())
        .arg("--no-warmup")
        .arg("-n")
        .arg(max_tokens.to_string())
        .arg("--temp")
        .arg("0")
        .arg("-t")
        .arg(threads().to_string())
        .arg("--simple-io")
        .arg("--log-file")
        .arg(log_path())
        .arg("-lv")
        .arg("4");
    command
}

/// Pick the last lighting word in the model's answer. The echoed prompt is
/// removed first so its "night or daylight" wording cannot be mistaken for the
/// answer.
fn parse_lighting(stdout: &str) -> Option<bool> {
    let text = strip_ansi(stdout);
    let body = if let Some((_, rest)) = text.rsplit_once(VISION_PROMPT) {
        rest
    } else {
        text.as_str()
    };
    let body = body.split("\n[ Prompt:").next().unwrap_or(body);
    let answer = if text.contains(VISION_PROMPT) {
        body.lines().find(|line| !line.trim().is_empty())?
    } else {
        body.lines().rev().find(|line| !line.trim().is_empty())?
    };
    let answer = answer.to_ascii_lowercase();
    let night = answer.rfind("night");
    let day = answer.rfind("day");

    match (night, day) {
        (Some(night), Some(day)) => Some(night > day),
        (Some(_), None) => Some(true),
        (None, Some(_)) => Some(false),
        (None, None) => None,
    }
}

/// Pull the description out of the vision helper's output.
fn parse_description(stdout: &str) -> Option<String> {
    let text = strip_ansi(stdout);

    // The echoed instruction also contains the tag names, so use the last
    // opening tag (the model's answer) rather than the first.
    if let Some(start) = text.rfind("<description>") {
        let rest = &text[start + "<description>".len()..];
        let end = rest
            .find("</description>")
            .or_else(|| rest.find("\n[ Prompt:"))
            .unwrap_or(rest.len());
        let candidate = clean(&rest[..end]);
        if !candidate.is_empty() {
            return Some(candidate);
        }
    }

    // Fallback for models that ignore the tags: skip the echoed prompt line
    // and stop at llama-cli's timing footer.
    let (_, body) = text.rsplit_once("\n> ")?;
    let body = body.split("\n[ Prompt:").next().unwrap_or(body);
    let mut lines = body.lines();
    let _ = lines.next();
    let candidate = clean(&lines.collect::<Vec<_>>().join("\n"));
    if candidate.is_empty() {
        None
    } else {
        Some(candidate)
    }
}

fn threads() -> u32 {
    std::thread::available_parallelism()
        .map(|count| (count.get() / 2).clamp(1, 8) as u32)
        .unwrap_or(4)
}

fn read_all(mut reader: impl Read) -> String {
    let mut buffer = String::new();
    let _ = reader.read_to_string(&mut buffer);
    buffer
}

/// Package hint for the common missing-library failures in prebuilt helpers.
pub(crate) fn dependency_hint(error: &str) -> Option<&'static str> {
    if error.contains("libssl.so") {
        Some("Install OpenSSL (Fedora: openssl-libs; Debian/Ubuntu: libssl3).")
    } else {
        None
    }
}

fn stderr_tail(stderr: &str) -> String {
    let lines: Vec<&str> = stderr
        .lines()
        .filter(|line| !line.trim().is_empty())
        .collect();
    if lines.is_empty() {
        return String::new();
    }
    let start = lines.len().saturating_sub(5);
    format!(": {}", lines[start..].join(" | "))
}

/// Pull the rewritten prompt out of `llama-cli`'s chat output.
fn extract(stdout: &str) -> Option<String> {
    let text = strip_ansi(stdout);

    if let Some(start) = text.find("<prompt>") {
        let rest = &text[start + "<prompt>".len()..];
        let end = rest
            .find("</prompt>")
            .or_else(|| rest.find("\n[ Prompt:"))
            .unwrap_or(rest.len());
        let candidate = clean(&rest[..end]);
        if !candidate.is_empty() {
            return Some(candidate);
        }
    }

    // Fallback for models that ignore the tags: take the lines after the
    // echoed prompt and stop at llama-cli's timing footer.
    let (_, body) = text.rsplit_once("\n> ")?;
    let body = body.split("\n[ Prompt:").next().unwrap_or(body);
    let mut lines = body.lines();
    let _ = lines.next();
    let candidate = clean(&lines.collect::<Vec<_>>().join("\n"));
    if candidate.is_empty() || candidate.contains("available commands:") {
        None
    } else {
        Some(candidate)
    }
}

fn clean(text: &str) -> String {
    let mut text = text.trim();
    for label in [
        "Improved prompt:",
        "improved prompt:",
        "Prompt:",
        "prompt:",
        "Here is the improved prompt:",
    ] {
        if let Some(rest) = text.strip_prefix(label) {
            text = rest.trim();
        }
    }
    text.trim_matches(|character| matches!(character, '"' | '\'' | '“' | '”' | '‘' | '’'))
        .trim()
        .to_string()
}

fn strip_ansi(text: &str) -> String {
    let mut output = String::with_capacity(text.len());
    let mut characters = text.chars().peekable();
    while let Some(character) = characters.next() {
        if character == '\u{1b}' {
            if characters.peek() == Some(&'[') {
                characters.next();
                for character in characters.by_ref() {
                    if character.is_ascii_alphabetic() {
                        break;
                    }
                }
            }
        } else {
            output.push(character);
        }
    }
    output
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hints_missing_openssl() {
        let error = "llama-cli: error while loading shared libraries: \
                     libssl.so.3: cannot open shared object file";
        assert!(dependency_hint(error).is_some());
        assert!(dependency_hint("llama-cli timed out").is_none());
    }

    fn command_args() -> Vec<String> {
        build_command(
            Path::new("/usr/bin/llama-cli"),
            Path::new("/models/Qwen3-4B-Q4_K_M.gguf"),
            "a red fox",
        )
        .get_args()
        .map(|argument| argument.to_string_lossy().into_owned())
        .collect()
    }

    #[test]
    fn builds_the_llama_command() {
        let args = command_args();
        assert!(
            args.windows(2)
                .any(|pair| pair == ["-m", "/models/Qwen3-4B-Q4_K_M.gguf"])
        );
        assert!(args.windows(2).any(|pair| pair == ["-p", "a red fox"]));
        assert!(args.windows(2).any(|pair| pair == ["-rea", "off"]));
        assert!(args.windows(2).any(|pair| pair == ["-c", "4096"]));
        assert!(args.windows(2).any(|pair| pair == ["-n", "220"]));
        assert!(args.contains(&"--no-warmup".to_string()));
        assert!(args.contains(&"--simple-io".to_string()));
    }

    #[test]
    fn builds_the_edit_command_with_scene_context() {
        let args: Vec<String> = build_edit_command(
            Path::new("/usr/bin/llama-cli"),
            Path::new("/models/Qwen3-4B-Q4_K_M.gguf"),
            "Current image: a fox on a path\nOriginal scene: a fox in a forest",
            "make it eat",
        )
        .get_args()
        .map(|argument| argument.to_string_lossy().into_owned())
        .collect();
        let system = args
            .windows(2)
            .find(|pair| pair[0] == "-sys")
            .map(|pair| pair[1].as_str())
            .unwrap();
        assert!(system.contains("short instruction"));
        assert!(system.contains("2D animated cartoon"));
        assert!(args.windows(2).any(|pair| pair == ["-n", "90"]));
        let prompt = args
            .windows(2)
            .find(|pair| pair[0] == "-p")
            .map(|pair| pair[1].as_str())
            .unwrap();
        assert!(prompt.contains("Current image: a fox on a path"));
        assert!(prompt.contains("New edit: make it eat"));
    }

    #[test]
    fn streams_only_the_tagged_prompt() {
        let mut extractor = PromptExtractor::default();
        let mut chunks = Vec::new();
        extractor.feed_bytes(
            b"> echo\n<prompt>The fox sits in a sunlit forest clearing with tall grass",
            &mut |chunk| chunks.push(chunk.to_string()),
        );
        extractor.feed_bytes(
            b" and fallen leaves.</prompt>\n[ Prompt: 1 ]\n",
            &mut |chunk| chunks.push(chunk.to_string()),
        );
        extractor.finish(&mut |chunk| chunks.push(chunk.to_string()));
        assert_eq!(
            chunks.concat(),
            "The fox sits in a sunlit forest clearing with tall grass and fallen leaves."
        );
        assert_eq!(
            extract(&extractor.raw).as_deref(),
            Some("The fox sits in a sunlit forest clearing with tall grass and fallen leaves.")
        );
    }

    #[test]
    fn streams_split_tags_and_utf8() {
        let mut extractor = PromptExtractor::default();
        let mut chunks = Vec::new();
        extractor.feed_bytes(b"<pro", &mut |chunk| chunks.push(chunk.to_string()));
        extractor.feed_bytes(b"mpt>caf", &mut |chunk| chunks.push(chunk.to_string()));
        extractor.feed_bytes(&[0xC3], &mut |chunk| chunks.push(chunk.to_string()));
        extractor.feed_bytes(&[0xA9], &mut |chunk| chunks.push(chunk.to_string()));
        extractor.feed_bytes(b" au lait</pro", &mut |chunk| {
            chunks.push(chunk.to_string())
        });
        extractor.feed_bytes(b"mpt>", &mut |chunk| chunks.push(chunk.to_string()));
        extractor.finish(&mut |chunk| chunks.push(chunk.to_string()));
        assert_eq!(chunks.concat(), "café au lait");
    }

    #[test]
    fn falls_back_when_streaming_tags_are_missing() {
        let mut extractor = PromptExtractor::default();
        let mut chunks = Vec::new();
        extractor.feed_bytes(
            b"\n> a red fox\nA red fox in autumn leaves.\n[ Prompt: 1 ]\n",
            &mut |chunk| chunks.push(chunk.to_string()),
        );
        extractor.finish(&mut |chunk| chunks.push(chunk.to_string()));
        assert!(chunks.is_empty());
        assert_eq!(
            extract(&extractor.raw).as_deref(),
            Some("A red fox in autumn leaves.")
        );
    }

    #[test]
    fn builds_the_description_command() {
        let args: Vec<String> = build_describe_command(
            Path::new("/usr/bin/llama-cli"),
            Path::new("/models/vision/model.gguf"),
            Path::new("/models/vision/mmproj.gguf"),
            Path::new("/tmp/photo.png"),
        )
        .get_args()
        .map(|argument| argument.to_string_lossy().into_owned())
        .collect();
        assert!(
            args.windows(2)
                .any(|pair| pair == ["--mmproj", "/models/vision/mmproj.gguf"])
        );
        assert!(
            args.windows(2)
                .any(|pair| pair == ["--image", "/tmp/photo.png"])
        );
        assert!(args.windows(2).any(|pair| pair == ["-n", "180"]));
        assert!(args.windows(2).any(|pair| pair == ["-c", "4096"]));
        let prompt = args
            .windows(2)
            .find(|pair| pair[0] == "-p")
            .map(|pair| pair[1].as_str())
            .unwrap();
        assert!(prompt.contains("<description>"));
    }

    #[test]
    fn parses_image_descriptions() {
        assert_eq!(
            parse_description(
                "\n> describe\n<description>A cartoon fox on a forest path.</description>\n[ Prompt: 1 ]\n"
            )
            .as_deref(),
            Some("A cartoon fox on a forest path.")
        );
        assert_eq!(
            parse_description(
                "\n> Describe the image and wrap it in <description> and </description> tags.\n<description>A cartoon fox on a forest path.</description>\n[ Prompt: 1 ]\n"
            )
            .as_deref(),
            Some("A cartoon fox on a forest path.")
        );
        assert_eq!(
            parse_description("\n> describe\nA cartoon fox on a forest path.\n[ Prompt: 1 ]\n")
                .as_deref(),
            Some("A cartoon fox on a forest path.")
        );
        assert_eq!(parse_description(""), None);
    }

    #[test]
    fn parses_generated_titles() {
        assert_eq!(
            parse_title("Title: \"Stormy Lighthouse at Dusk.\"\n"),
            Some("Stormy Lighthouse at Dusk".into())
        );
        assert_eq!(
            parse_title("  **Golden Hour Fox**  "),
            Some("Golden Hour Fox".into())
        );
        assert_eq!(parse_title(""), None);
        assert_eq!(parse_title("<|im_end|>"), None);
        assert_eq!(
            parse_title("Loading model...\n<title>Standing fox in a forest</title>\nExiting..."),
            Some("Standing fox in a forest".into())
        );
        assert_eq!(
            parse_title("Loading model...\n> A fox in a forest\n[ Prompt: 10 t/s ]\nExiting..."),
            Some("A fox in a forest".into())
        );
        assert_eq!(parse_title("Loading model...\nExiting..."), None);
    }

    #[cfg(unix)]
    #[test]
    fn reads_a_title_from_the_helper() {
        use std::os::unix::fs::PermissionsExt;

        let directory =
            std::env::temp_dir().join(format!("flux-title-test-{}", std::process::id()));
        std::fs::create_dir_all(&directory).unwrap();
        let cli = directory.join("llama-cli");
        let encoder = directory.join("encoder.gguf");
        std::fs::write(&encoder, b"test").unwrap();
        std::fs::write(&cli, b"#!/bin/sh\necho 'Title: \"Golden Hour Fox.\"'\n").unwrap();
        std::fs::set_permissions(&cli, std::fs::Permissions::from_mode(0o755)).unwrap();

        let title = title("a unique test fox", &encoder, &cli).unwrap();
        assert_eq!(title, "Golden Hour Fox");
        let _ = std::fs::remove_dir_all(&directory);
    }

    #[cfg(unix)]
    #[test]
    fn streams_prompt_chunks_from_the_helper() {
        use std::os::unix::fs::PermissionsExt;

        let directory =
            std::env::temp_dir().join(format!("flux-stream-test-{}", std::process::id()));
        std::fs::create_dir_all(&directory).unwrap();
        let cli = directory.join("llama-cli");
        let encoder = directory.join("encoder.gguf");
        std::fs::write(&encoder, b"test").unwrap();
        std::fs::write(
            &cli,
            b"#!/bin/sh\nprintf '\\n> prompt\\n<prompt>Hello '\nsleep 0.2\nprintf 'world</prompt>\\n[ Prompt: 1 ]\\n'\n",
        )
        .unwrap();
        std::fs::set_permissions(&cli, std::fs::Permissions::from_mode(0o755)).unwrap();

        let cancel = Arc::new(AtomicBool::new(false));
        let (sender, receiver) = std::sync::mpsc::channel();
        let result = improve_streaming("a fox", None, &encoder, &cli, cancel, move |chunk| {
            let _ = sender.send(chunk.to_string());
        })
        .unwrap();
        assert_eq!(result.as_deref(), Some("Hello world"));
        let chunks: Vec<String> = receiver.try_iter().collect();
        assert_eq!(chunks.concat(), "Hello world");
        let _ = std::fs::remove_dir_all(&directory);
    }

    #[cfg(unix)]
    #[test]
    fn cancels_a_streaming_helper() {
        use std::os::unix::fs::PermissionsExt;

        let directory =
            std::env::temp_dir().join(format!("flux-cancel-test-{}", std::process::id()));
        std::fs::create_dir_all(&directory).unwrap();
        let cli = directory.join("llama-cli");
        let encoder = directory.join("encoder.gguf");
        std::fs::write(&encoder, b"test").unwrap();
        std::fs::write(
            &cli,
            b"#!/bin/sh\nprintf '\\n> prompt\\n<prompt>Hello '\nexec sleep 5\nprintf 'world</prompt>'\n",
        )
        .unwrap();
        std::fs::set_permissions(&cli, std::fs::Permissions::from_mode(0o755)).unwrap();

        let cancel = Arc::new(AtomicBool::new(false));
        let worker_cancel = cancel.clone();
        let handle = std::thread::spawn(move || {
            improve_streaming("a fox", None, &encoder, &cli, cancel, |_| {})
        });
        std::thread::sleep(Duration::from_millis(400));
        worker_cancel.store(true, Ordering::SeqCst);
        let started = Instant::now();
        let result = handle.join().unwrap().unwrap();
        assert!(result.is_none());
        assert!(started.elapsed() < Duration::from_secs(2));
        let _ = std::fs::remove_dir_all(&directory);
    }

    #[test]
    fn extracts_tagged_output() {
        let output = "\n> a red fox\n<prompt>A red fox in autumn leaves, golden hour.</prompt>\n\n[ Prompt: 70 t/s ]\nExiting...\n";
        assert_eq!(
            extract(output).as_deref(),
            Some("A red fox in autumn leaves, golden hour.")
        );
    }

    #[test]
    fn extracts_unclosed_tagged_output() {
        let output = "\n> a red fox\n<prompt>A red fox in autumn leaves\n[ Prompt: 70 t/s ]\n";
        assert_eq!(
            extract(output).as_deref(),
            Some("A red fox in autumn leaves")
        );
    }

    #[test]
    fn falls_back_to_plain_output() {
        let output = "\n> a red fox\nA red fox in autumn leaves, golden hour.\n\n[ Prompt: 70 t/s ]\nExiting...\n";
        assert_eq!(
            extract(output).as_deref(),
            Some("A red fox in autumn leaves, golden hour.")
        );
    }

    #[test]
    fn ignores_cli_chatter() {
        let output = "available commands:\n  /exit\n";
        assert_eq!(extract(output), None);
    }

    #[test]
    fn strips_ansi_and_quotes() {
        let output = "\u{1b}[32m<prompt>\"a red fox\"</prompt>\u{1b}[0m";
        assert_eq!(extract(output).as_deref(), Some("a red fox"));
        assert_eq!(clean("Improved prompt: a red fox"), "a red fox");
    }

    #[test]
    fn splits_paths_for_the_cli_lookup() {
        // Sanity check that PATH splitting yields usable directories.
        if let Some(path) = std::env::var_os("PATH") {
            assert!(std::env::split_paths(&path).next().is_some());
        }
    }

    #[test]
    fn builds_the_vision_command() {
        let args: Vec<String> = build_vision_command(
            Path::new("/usr/bin/llama-cli"),
            Path::new("/models/vision/model.gguf"),
            Path::new("/models/vision/mmproj.gguf"),
            Path::new("/tmp/photo.png"),
        )
        .get_args()
        .map(|argument| argument.to_string_lossy().into_owned())
        .collect();
        assert!(
            args.windows(2)
                .any(|pair| pair == ["-m", "/models/vision/model.gguf"])
        );
        assert!(
            args.windows(2)
                .any(|pair| pair == ["--mmproj", "/models/vision/mmproj.gguf"])
        );
        assert!(
            args.windows(2)
                .any(|pair| pair == ["--image", "/tmp/photo.png"])
        );
        assert!(args.windows(2).any(|pair| pair == ["-n", "8"]));
        assert!(args.windows(2).any(|pair| pair == ["--temp", "0"]));
    }

    #[test]
    fn parses_lighting_answers() {
        let night = format!("\n> {VISION_PROMPT}\nNight\n\n[ Prompt: 500 t/s ]\n");
        let day = format!("\n> {VISION_PROMPT}\nDaylight\n\n[ Prompt: 500 t/s ]\n");
        assert_eq!(parse_lighting(&night), Some(true));
        assert_eq!(parse_lighting(&day), Some(false));
        assert_eq!(
            parse_lighting("It is not night, it is daylight."),
            Some(false)
        );
        assert_eq!(parse_lighting("night"), Some(true));
        assert_eq!(parse_lighting("unclear"), None);
    }
}

//! Image upscaling through `sd-server`, with an `sd-cli` fallback.
//!
//! Older backends expose `/sdcpp/v1/upscale` and keep the model loaded in a
//! separate model-less server process. stable-diffusion.cpp `3b12781` removed
//! that endpoint, so newer backends run `sd-cli -M upscale` per request. Both
//! paths avoid loading the multi-gigabyte diffusion model.

use std::fs::OpenOptions;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, anyhow, bail};
use base64::Engine;
use base64::engine::general_purpose::STANDARD as BASE64;
use image::imageops::FilterType;
use imagiflume_core::upscaler::{
    DEFAULT_TILE_SIZE, FALLBACK_TILE_SIZE, UpscaleRequest, UpscaledImage, upscaler,
};
use serde::Deserialize;
use serde_json::json;

use crate::server::SdServerConfig;

const IDLE_TIMEOUT: Duration = Duration::from_secs(60);
const START_TIMEOUT: Duration = Duration::from_secs(30);
const REQUEST_TIMEOUT: Duration = Duration::from_secs(3600);

pub struct SdUpscaleServer {
    inner: Arc<Mutex<Inner>>,
    busy: Arc<AtomicBool>,
    cancelling: Arc<AtomicBool>,
    /// PID of a running `sd-cli -M upscale` fallback process.
    cli_pid: Arc<AtomicU32>,
}

struct Inner {
    config: SdServerConfig,
    client: reqwest::blocking::Client,
    child: Option<Child>,
    port: u16,
    log: PathBuf,
    last_used: Instant,
    /// `None` until the backend is probed. `false` means the backend dropped
    /// `/sdcpp/v1/upscale` and uses the `sd-cli` fallback.
    http_upscale: Option<bool>,
}

struct RunContext<'a> {
    model_path: &'a Path,
    native_width: u32,
    native_height: u32,
    request: &'a UpscaleRequest,
}

impl SdUpscaleServer {
    pub fn new(config: SdServerConfig) -> Self {
        let log = imagiflume_core::paths::cache_dir().join("sd-upscale-server.log");
        let inner = Arc::new(Mutex::new(Inner {
            config,
            client: reqwest::blocking::Client::builder()
                .build()
                .expect("HTTP client"),
            child: None,
            port: 0,
            log,
            last_used: Instant::now(),
            http_upscale: None,
        }));
        let busy = Arc::new(AtomicBool::new(false));

        // Keep the small server alive for repeated upscales, then release it.
        let monitor = inner.clone();
        let monitor_busy = busy.clone();
        std::thread::spawn(move || {
            loop {
                std::thread::sleep(Duration::from_secs(2));
                if monitor_busy.load(Ordering::SeqCst) {
                    continue;
                }
                let mut inner = monitor.lock().expect("upscale server mutex");
                if inner.child.is_some() && inner.last_used.elapsed() >= IDLE_TIMEOUT {
                    tracing::info!(
                        "upscaler idle for {}s, stopping sd-server",
                        IDLE_TIMEOUT.as_secs()
                    );
                    inner.stop();
                }
            }
        });

        Self {
            inner,
            busy,
            cancelling: Arc::new(AtomicBool::new(false)),
            cli_pid: Arc::new(AtomicU32::new(0)),
        }
    }

    /// Point the upscale server at another binary and backend.
    pub fn set_server(&self, cli: PathBuf, backend: Option<String>) {
        let mut inner = self.inner.lock().expect("upscale server mutex");
        inner.stop();
        inner.config.cli = cli;
        inner.config.backend = backend;
        inner.http_upscale = None;
    }

    /// Release the upscale server process and its CUDA context.
    pub fn unload(&self) {
        if self.busy.load(Ordering::SeqCst) {
            return;
        }
        let mut inner = self.inner.lock().expect("upscale server mutex");
        inner.stop();
    }

    pub fn cancel(&self) -> Result<()> {
        self.cancelling.store(true, Ordering::SeqCst);
        kill_process(self.cli_pid.swap(0, Ordering::SeqCst));
        let mut inner = self.inner.lock().expect("upscale server mutex");
        inner.stop();
        Ok(())
    }

    pub fn upscale(&self, request: &UpscaleRequest) -> Result<UpscaledImage> {
        self.cancelling.store(false, Ordering::SeqCst);

        let preset = upscaler(&request.upscaler_id)
            .ok_or_else(|| anyhow!("unknown upscaler {:?}", request.upscaler_id))?;
        if !matches!(request.factor, 2 | 4) {
            bail!("unsupported upscale factor {}", request.factor);
        }
        let models_dir = {
            let inner = self.inner.lock().expect("upscale server mutex");
            inner.config.models_dir.clone()
        };
        let model_path = preset.path(&models_dir);
        if !model_path.exists() {
            bail!(
                "{} is not installed (expected {})",
                preset.label,
                model_path.display()
            );
        }
        if !request.input.exists() {
            bail!("input image not found at {}", request.input.display());
        }
        let (input_width, input_height) = image::image_dimensions(&request.input)
            .with_context(|| format!("failed to read {}", request.input.display()))?;
        let native_width = input_width
            .checked_mul(preset.scale)
            .ok_or_else(|| anyhow!("image is too large to upscale"))?;
        let native_height = input_height
            .checked_mul(preset.scale)
            .ok_or_else(|| anyhow!("image is too large to upscale"))?;

        self.busy.store(true, Ordering::SeqCst);
        let result = (|| -> Result<UpscaledImage> {
            let (http_upscale, port, client, sd_cli, backend, log) = {
                let mut inner = self.inner.lock().expect("upscale server mutex");
                let http = inner.ensure_started()?;
                (
                    http,
                    inner.port,
                    inner.client.clone(),
                    sibling_cli(&inner.config.cli),
                    inner.config.backend.clone(),
                    inner.log.clone(),
                )
            };

            if http_upscale {
                let bytes = std::fs::read(&request.input)
                    .with_context(|| format!("failed to read {}", request.input.display()))?;
                let data_url = data_url(&request.input, &bytes);
                let context = RunContext {
                    model_path: &model_path,
                    native_width,
                    native_height,
                    request,
                };
                self.run_http(port, &client, preset.server_name, &data_url, &context)
            } else {
                let context = RunContext {
                    model_path: &model_path,
                    native_width,
                    native_height,
                    request,
                };
                self.run_cli(&sd_cli, backend.as_deref(), &log, &context)
            }
        })();
        self.busy.store(false, Ordering::SeqCst);
        {
            let mut inner = self.inner.lock().expect("upscale server mutex");
            inner.last_used = Instant::now();
        }
        result
    }

    fn run_http(
        &self,
        port: u16,
        client: &reqwest::blocking::Client,
        server_name: &str,
        data_url: &str,
        context: &RunContext<'_>,
    ) -> Result<UpscaledImage> {
        let (native_width, native_height, request) =
            (context.native_width, context.native_height, context.request);
        let mut tile_size = if request.tile_size == 0 {
            DEFAULT_TILE_SIZE
        } else {
            request.tile_size
        };

        loop {
            if self.cancelling.load(Ordering::SeqCst) {
                bail!("upscaling cancelled");
            }
            let url = format!("http://127.0.0.1:{port}/sdcpp/v1/upscale");
            let body = json!({
                "image": data_url,
                "upscaler": server_name,
                "repeats": 1,
                "tile_size": tile_size,
                "output_format": "png",
            });
            let response = client
                .post(&url)
                .json(&body)
                .timeout(REQUEST_TIMEOUT)
                .send();

            let response = match response {
                Ok(response) => response,
                Err(error) => {
                    if self.cancelling.load(Ordering::SeqCst) {
                        bail!("upscaling cancelled");
                    }
                    bail!("upscale request failed: {error}");
                }
            };

            let status = response.status();
            if !status.is_success() {
                let detail = response.text().unwrap_or_default();
                if is_out_of_memory(&detail) && tile_size > FALLBACK_TILE_SIZE {
                    tracing::warn!(
                        tile_size,
                        fallback = FALLBACK_TILE_SIZE,
                        "upscaler ran out of VRAM; retrying with a smaller tile"
                    );
                    tile_size = FALLBACK_TILE_SIZE;
                    continue;
                }
                bail!("upscaler returned HTTP {status}: {}", detail.trim());
            }

            let decoded: UpscaleResponse = response
                .json()
                .context("failed to decode the upscale response")?;
            let encoded = decoded
                .images
                .into_iter()
                .next()
                .ok_or_else(|| anyhow!("upscaler returned no image"))?;
            let image_bytes = BASE64
                .decode(encoded.b64_json.as_bytes())
                .context("upscaler returned invalid image data")?;

            if decoded.width != native_width || decoded.height != native_height {
                bail!(
                    "upscaler returned {} × {} instead of {} × {}",
                    decoded.width,
                    decoded.height,
                    native_width,
                    native_height
                );
            }

            if let Some(parent) = request.output.parent() {
                std::fs::create_dir_all(parent)?;
            }

            if request.factor == 2 {
                let image = image::load_from_memory(&image_bytes)
                    .context("upscaler returned an unreadable image")?;
                let width = decoded.width / 2;
                let height = decoded.height / 2;
                let resized = image.resize_exact(width, height, FilterType::Lanczos3);
                resized
                    .save(&request.output)
                    .with_context(|| format!("failed to write {}", request.output.display()))?;
                return Ok(UpscaledImage {
                    path: request.output.clone(),
                    width,
                    height,
                });
            }

            std::fs::write(&request.output, &image_bytes)
                .with_context(|| format!("failed to write {}", request.output.display()))?;
            return Ok(UpscaledImage {
                path: request.output.clone(),
                width: decoded.width,
                height: decoded.height,
            });
        }
    }

    fn run_cli(
        &self,
        sd_cli: &Path,
        backend: Option<&str>,
        log_path: &Path,
        context: &RunContext<'_>,
    ) -> Result<UpscaledImage> {
        let (model_path, native_width, native_height, request) = (
            context.model_path,
            context.native_width,
            context.native_height,
            context.request,
        );
        if !sd_cli.exists() {
            bail!(
                "{} is missing; install a backend that includes sd-cli or select an older backend",
                sd_cli.display()
            );
        }
        let mut tile_size = if request.tile_size == 0 {
            DEFAULT_TILE_SIZE
        } else {
            request.tile_size
        };

        loop {
            if self.cancelling.load(Ordering::SeqCst) {
                bail!("upscaling cancelled");
            }
            let temp = imagiflume_core::paths::cache_dir()
                .join(format!("upscale-cli-{}.png", std::process::id()));
            let _ = std::fs::remove_file(&temp);

            let mut command = Command::new(sd_cli);
            command
                .arg("-M")
                .arg("upscale")
                .arg("--init-img")
                .arg(&request.input)
                .arg("--upscale-model")
                .arg(model_path)
                .arg("--upscale-repeats")
                .arg("1")
                .arg("--upscale-tile-size")
                .arg(tile_size.to_string())
                .arg("-o")
                .arg(&temp)
                .arg("-v")
                .stdin(Stdio::null())
                .stdout(Stdio::piped())
                .stderr(Stdio::piped());
            if let Some(backend) = backend {
                command.arg("--backend").arg(backend);
            }

            #[cfg(target_os = "linux")]
            unsafe {
                use std::os::unix::process::CommandExt;
                command.pre_exec(|| {
                    let _ = libc::prctl(libc::PR_SET_PDEATHSIG, libc::SIGTERM);
                    Ok(())
                });
            }

            let child = command
                .spawn()
                .with_context(|| format!("failed to spawn {}", sd_cli.display()))?;
            let pid = child.id();
            self.cli_pid.store(pid, Ordering::SeqCst);
            if self.cancelling.load(Ordering::SeqCst) {
                kill_process(pid);
            }
            let output = child
                .wait_with_output()
                .context("failed to wait for sd-cli")?;
            self.cli_pid.store(0, Ordering::SeqCst);

            let log = format!(
                "{}{}",
                String::from_utf8_lossy(&output.stdout),
                String::from_utf8_lossy(&output.stderr)
            );
            append_log(log_path, &log);

            if self.cancelling.load(Ordering::SeqCst) {
                let _ = std::fs::remove_file(&temp);
                bail!("upscaling cancelled");
            }
            if !output.status.success() {
                if is_out_of_memory(&log) && tile_size > FALLBACK_TILE_SIZE {
                    tracing::warn!(
                        tile_size,
                        fallback = FALLBACK_TILE_SIZE,
                        "upscaler ran out of VRAM; retrying with a smaller tile"
                    );
                    tile_size = FALLBACK_TILE_SIZE;
                    let _ = std::fs::remove_file(&temp);
                    continue;
                }
                bail!("upscaler failed: {}", log_tail(&log));
            }

            let (width, height) = image::image_dimensions(&temp).with_context(|| {
                format!(
                    "upscaler did not write {}: {}",
                    temp.display(),
                    log_tail(&log)
                )
            })?;
            if width != native_width || height != native_height {
                let _ = std::fs::remove_file(&temp);
                bail!(
                    "upscaler returned {width} × {height} instead of {native_width} × {native_height}"
                );
            }

            if let Some(parent) = request.output.parent() {
                std::fs::create_dir_all(parent)?;
            }

            let result = if request.factor == 2 {
                let image = image::open(&temp).context("upscaler returned an unreadable image")?;
                let width = width / 2;
                let height = height / 2;
                let resized = image.resize_exact(width, height, FilterType::Lanczos3);
                resized
                    .save(&request.output)
                    .with_context(|| format!("failed to write {}", request.output.display()))?;
                UpscaledImage {
                    path: request.output.clone(),
                    width,
                    height,
                }
            } else {
                std::fs::copy(&temp, &request.output)
                    .with_context(|| format!("failed to write {}", request.output.display()))?;
                UpscaledImage {
                    path: request.output.clone(),
                    width,
                    height,
                }
            };

            let _ = std::fs::remove_file(&temp);
            return Ok(result);
        }
    }
}

impl Inner {
    fn ensure_started(&mut self) -> Result<bool> {
        if let Some(child) = self.child.as_mut()
            && child.try_wait()?.is_some()
        {
            self.child = None;
        }
        if self.http_upscale == Some(false) {
            return Ok(false);
        }
        if self.child.is_some() {
            self.last_used = Instant::now();
            return Ok(true);
        }

        let port = free_port()?;
        if let Some(parent) = self.log.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let log = OpenOptions::new()
            .create(true)
            .truncate(true)
            .write(true)
            .open(&self.log)
            .with_context(|| format!("failed to open {}", self.log.display()))?;
        let log_err = log.try_clone()?;
        let upscalers_dir = self.config.models_dir.join("upscalers");

        let mut command = Command::new(&self.config.cli);
        if let Some(backend) = &self.config.backend {
            command.arg("--backend").arg(backend);
        }
        command
            .arg("--listen-ip")
            .arg("127.0.0.1")
            .arg("--listen-port")
            .arg(port.to_string())
            .arg("--hires-upscalers-dir")
            .arg(&upscalers_dir)
            .arg("-v")
            .stdin(Stdio::null())
            .stdout(Stdio::from(log))
            .stderr(Stdio::from(log_err));

        #[cfg(target_os = "linux")]
        unsafe {
            use std::os::unix::process::CommandExt;
            command.pre_exec(|| {
                let _ = libc::prctl(libc::PR_SET_PDEATHSIG, libc::SIGTERM);
                Ok(())
            });
        }

        let child = command
            .spawn()
            .with_context(|| format!("failed to spawn {}", self.config.cli.display()))?;
        self.child = Some(child);
        self.port = port;
        let http = self.wait_ready(&upscalers_dir)?;
        self.http_upscale = Some(http);
        if !http {
            self.stop();
        }
        self.last_used = Instant::now();
        Ok(http)
    }

    fn wait_ready(&mut self, upscalers_dir: &Path) -> Result<bool> {
        let deadline = Instant::now() + START_TIMEOUT;
        let url = format!("http://127.0.0.1:{}/sdcpp/v1/capabilities", self.port);
        loop {
            if let Some(child) = self.child.as_mut()
                && let Some(status) = child.try_wait()?
            {
                self.child = None;
                bail!(
                    "sd-server exited with {status} while loading the upscaler (see {})",
                    self.log.display()
                );
            }

            if let Ok(response) = self.client.get(&url).timeout(Duration::from_secs(2)).send()
                && response.status().is_success()
                && let Ok(capabilities) = response.json::<Capabilities>()
            {
                match capabilities.upscale {
                    Some(true) => return Ok(true),
                    // 3b12781+ dropped the endpoint; use `sd-cli -M upscale`.
                    None => return Ok(false),
                    Some(false) => {
                        self.stop();
                        bail!(
                            "no compatible upscaler models found in {}",
                            upscalers_dir.display()
                        );
                    }
                }
            }

            if Instant::now() >= deadline {
                self.stop();
                bail!("timed out starting the upscale server");
            }
            std::thread::sleep(Duration::from_millis(100));
        }
    }

    fn stop(&mut self) {
        if let Some(mut child) = self.child.take() {
            let _ = child.kill();
            let _ = child.wait();
        }
    }
}

impl Drop for Inner {
    fn drop(&mut self) {
        self.stop();
    }
}

#[derive(Deserialize)]
struct Capabilities {
    /// Missing on builds that removed `/sdcpp/v1/upscale`.
    upscale: Option<bool>,
}

#[derive(Deserialize)]
struct UpscaleResponse {
    images: Vec<EncodedImage>,
    width: u32,
    height: u32,
}

#[derive(Deserialize)]
struct EncodedImage {
    b64_json: String,
}

fn sibling_cli(server: &Path) -> PathBuf {
    server.with_file_name("sd-cli")
}

fn kill_process(pid: u32) {
    #[cfg(unix)]
    if pid > 0 {
        // The fallback CLI is short-lived; SIGTERM is enough.
        unsafe {
            libc::kill(pid as i32, libc::SIGTERM);
        }
    }
    #[cfg(not(unix))]
    let _ = pid;
}

fn append_log(path: &Path, text: &str) {
    use std::io::Write;
    if let Ok(mut file) = OpenOptions::new().create(true).append(true).open(path) {
        let _ = writeln!(file, "{text}");
    }
}

fn log_tail(text: &str) -> String {
    let lines: Vec<&str> = text
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .collect();
    let start = lines.len().saturating_sub(6);
    lines[start..].join(" | ")
}

fn is_out_of_memory(detail: &str) -> bool {
    let detail = detail.to_ascii_lowercase();
    detail.contains("out of memory")
        || detail.contains("cannot make enough memory")
        || detail.contains("failed during weight preparation")
}

fn data_url(path: &Path, bytes: &[u8]) -> String {
    let extension = path
        .extension()
        .and_then(|extension| extension.to_str())
        .map(|extension| extension.to_ascii_lowercase());
    let mime = match extension.as_deref() {
        Some("jpg" | "jpeg") => "image/jpeg",
        Some("webp") => "image/webp",
        _ => "image/png",
    };
    format!("data:{mime};base64,{}", BASE64.encode(bytes))
}

fn free_port() -> Result<u16> {
    let listener = std::net::TcpListener::bind(("127.0.0.1", 0))?;
    let port = listener.local_addr()?.port();
    drop(listener);
    Ok(port)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    #[test]
    fn detects_vram_failures() {
        assert!(is_out_of_memory(
            "execution engine cannot make enough memory available on CUDA0"
        ));
        assert!(is_out_of_memory("CUDA error: out of memory"));
        assert!(!is_out_of_memory("no compatible image upscaler called x"));
    }

    #[test]
    fn builds_data_urls_for_supported_formats() {
        let png = data_url(Path::new("/tmp/a.png"), b"abc");
        assert!(png.starts_with("data:image/png;base64,"));
        let jpeg = data_url(Path::new("/tmp/a.JPG"), b"abc");
        assert!(jpeg.starts_with("data:image/jpeg;base64,"));
    }

    #[test]
    fn detects_a_missing_http_upscale_endpoint() {
        let old: Capabilities = serde_json::from_str(r#"{"upscale":true}"#).unwrap();
        assert_eq!(old.upscale, Some(true));
        let new: Capabilities = serde_json::from_str(r#"{"upscalers":[]}"#).unwrap();
        assert_eq!(new.upscale, None);
    }

    #[test]
    fn finds_sd_cli_next_to_the_server() {
        assert_eq!(
            sibling_cli(Path::new("/opt/imagiflume/sd-server")),
            PathBuf::from("/opt/imagiflume/sd-cli")
        );
    }

    #[test]
    fn unknown_upscaler_is_rejected_before_starting_a_server() {
        let dir = std::env::temp_dir().join(format!("flux-upscale-test-{}", std::process::id()));
        let server = SdUpscaleServer::new(SdServerConfig {
            cli: PathBuf::from("/bin/false"),
            lora_dir: dir.join("loras"),
            models_dir: dir,
            backend: None,
        });
        let error = server
            .upscale(&UpscaleRequest {
                input: PathBuf::from("/tmp/missing.png"),
                output: PathBuf::from("/tmp/out.png"),
                upscaler_id: "nope".into(),
                factor: 4,
                tile_size: DEFAULT_TILE_SIZE,
            })
            .unwrap_err();
        assert!(error.to_string().contains("unknown upscaler"));
    }
}

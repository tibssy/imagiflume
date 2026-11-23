//! Live camera preview and capture.
//!
//! When a desktop portal is available, capture goes through
//! `org.freedesktop.portal.Camera`: the portal asks the user for permission,
//! records the answer in the desktop's permission store and hands back a
//! PipeWire file descriptor. The first button press opens a live preview; the
//! second saves the current frame and releases the camera.
//!
//! Native sessions without a camera portal fall back to a one-shot V4L2 grab,
//! but a sandboxed app never does: its camera access must stay visible to the
//! portal.

use std::os::fd::{AsRawFd, OwnedFd};
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};

use adw::prelude::*;
use gst::prelude::{
    DeviceExt, DeviceProviderExt, DeviceProviderExtManual, ElementExt, ElementExtManual,
    GstBinExtManual, PadExt,
};
use gstreamer as gst;
use gstreamer_app::AppSink;
use gtk::glib::prelude::Cast;
use gtk::{gdk, glib};
use imagiflume_core::paths;

use crate::{Ui, add_references, now_millis};

const PHOTO_ICON: &str = "camera-photo-symbolic";
const RECORD_ICON: &str = "media-record-symbolic";
const BLOCKED_HINT: &str =
    "Camera access is blocked. Enable Imagiflume in Settings → Privacy & Security → Camera.";

static GST_INIT: OnceLock<Result<(), String>> = OnceLock::new();
/// Portal PipeWire remotes are expensive to set up and their fd numbers are
/// reused by the kernel; GStreamer caches PipeWire cores by fd, so reopening a
/// remote after every shot can hand back a stale core with no camera nodes.
static PORTAL_FD: OnceLock<Mutex<Option<Arc<OwnedFd>>>> = OnceLock::new();
static CAMERA_SESSION: OnceLock<Mutex<Option<CameraSession>>> = OnceLock::new();

struct CameraSession {
    fd: Arc<OwnedFd>,
    provider: gst::DeviceProvider,
}

impl Drop for CameraSession {
    fn drop(&mut self) {
        self.provider.stop();
        self.provider.set_property("fd", -1);
    }
}

#[derive(Clone)]
struct Frame {
    width: u32,
    height: u32,
    rgba: Vec<u8>,
}

/// A running camera preview. Dropping it without `stop` would leave the
/// pipeline playing, so every path uses [`stop_preview`].
pub(crate) struct Preview {
    pipeline: gst::Pipeline,
    latest: Arc<Mutex<Option<Frame>>>,
    stop: Arc<AtomicBool>,
    join: Option<std::thread::JoinHandle<()>>,
}

/// Initialise GStreamer once, preferably on the main thread.
pub fn init() {
    if let Err(error) = ensure_gst() {
        tracing::warn!(%error, "GStreamer is not available; camera capture is disabled");
    }
}

/// Register the host app so portals can show the real name and keep a
/// per-application permission. Flatpak apps are already identified.
pub fn register_host_app() {
    glib::spawn_future_local(async {
        let Ok(app_id) = imagiflume_core::APP_ID.try_into() else {
            return;
        };
        if let Err(error) = ashpd::register_host_app(app_id).await {
            tracing::debug!(%error, "could not register the host app with the portal");
        }
    });
}

/// Refresh whether the camera button should be visible.
pub fn refresh_availability(ui: &Rc<Ui>) {
    ui.camera_button.set_visible(false);
    let ui = ui.clone();
    glib::spawn_future_local(async move {
        ui.camera_button.set_visible(is_available().await);
        if ui.camera_button.is_visible() {
            ui.camera_button.set_sensitive(!ui.camera_busy.get());
        }
    });
}

/// First click previews, second click captures.
pub fn toggle(ui: &Rc<Ui>) {
    if ui.camera_busy.get() {
        return;
    }
    if ui.camera_preview.borrow().is_some() {
        capture(ui);
    } else {
        start(ui);
    }
}

fn start(ui: &Rc<Ui>) {
    if ui.camera_busy.replace(true) {
        return;
    }
    if ui.generating.get() && ui.live_view.get() {
        reset_button(ui);
        ui.toast
            .add_toast(adw::Toast::new("Wait for the running generation first."));
        return;
    }
    ui.camera_button.set_sensitive(false);
    ui.status.set_text("Opening the camera…");

    let ui = ui.clone();
    glib::spawn_future_local(async move {
        match portal_fd().await {
            Ok(Some(fd)) => start_portal_preview(&ui, fd).await,
            Ok(None) => {
                if in_flatpak() {
                    fail(
                        &ui,
                        "Could not open the camera",
                        "Camera access needs the desktop camera portal.",
                    );
                } else {
                    start_v4l2_fallback(&ui).await;
                }
            }
            Err(error) => fail(&ui, "Could not open the camera", &error),
        }
    });
}

async fn start_portal_preview(ui: &Rc<Ui>, fd: Arc<OwnedFd>) {
    let (signal_tx, signal_rx) = async_channel::bounded(1);
    let result = run_blocking(move || start_preview_pipeline(fd, signal_tx)).await;
    match result {
        Ok(preview) => {
            *ui.camera_preview.borrow_mut() = Some(preview);
            crate::begin_camera_preview_view(ui);
            ui.camera_button.set_icon_name(RECORD_ICON);
            ui.camera_button.set_tooltip_text(Some("Take the photo"));
            ui.camera_busy.set(false);
            ui.camera_button.set_sensitive(true);
            ui.status
                .set_text("Camera preview · click the camera button to take a photo");
            spawn_frame_loop(ui.clone(), signal_rx);
        }
        Err(error) => fail(ui, "Could not open the camera", &error),
    }
}

async fn start_v4l2_fallback(ui: &Rc<Ui>) {
    let Some(device) = v4l2_device() else {
        fail(ui, "Could not open the camera", "No camera found.");
        return;
    };
    let directory = paths::cache_dir().join("camera");
    if let Err(error) = std::fs::create_dir_all(&directory) {
        fail(
            ui,
            "Could not take a photo",
            &format!("Could not prepare the camera: {error}"),
        );
        return;
    }
    let path = directory.join(format!("camera-{}.png", now_millis()));
    let captured = path.clone();
    let result = run_blocking(move || capture_v4l2(&device, &captured)).await;
    finish(ui, result.map(|()| path));
}

/// Save the current preview frame and release the camera.
fn capture(ui: &Rc<Ui>) {
    if ui.camera_busy.replace(true) {
        return;
    }
    ui.camera_button.set_sensitive(false);
    let Some(preview) = ui.camera_preview.borrow_mut().take() else {
        reset_button(ui);
        return;
    };
    let frame = preview.latest.lock().ok().and_then(|latest| latest.clone());
    stop_preview(preview);
    set_photo_button(ui);

    let Some(frame) = frame else {
        capture_failed(ui, "The camera did not send a frame.");
        return;
    };
    let directory = paths::cache_dir().join("camera");
    if let Err(error) = std::fs::create_dir_all(&directory) {
        capture_failed(ui, &format!("Could not prepare the camera: {error}"));
        return;
    }
    let Some(image) = image::RgbaImage::from_raw(frame.width, frame.height, frame.rgba) else {
        capture_failed(ui, "The camera frame is invalid.");
        return;
    };
    let path = directory.join(format!("camera-{}.png", now_millis()));
    if let Err(error) = image.save(&path) {
        capture_failed(ui, &format!("Could not save the photo: {error}"));
        return;
    }
    ui.status.set_text("Photo added as a reference");
    finish(ui, Ok(path));
}

/// Close the preview and restore the previous canvas image.
pub fn cancel(ui: &Rc<Ui>) {
    let preview = ui.camera_preview.borrow_mut().take();
    let had_preview = preview.is_some();
    if let Some(preview) = preview {
        stop_preview(preview);
    }
    reset_button(ui);
    if had_preview {
        ui.status.set_text("Camera preview closed");
        crate::update_image_view(ui);
    }
}

/// Release the camera because the canvas is changing for another reason.
pub fn forget(ui: &Ui) {
    if let Some(preview) = ui.camera_preview.borrow_mut().take() {
        stop_preview(preview);
    }
    reset_button(ui);
}

fn finish(ui: &Rc<Ui>, outcome: Result<PathBuf, String>) {
    reset_button(ui);
    match outcome {
        Ok(path) => {
            add_references(ui, vec![path.clone()]);
            let present = {
                let state = ui.state.borrow();
                state.display.as_deref() == Some(path.as_path())
                    || state.references.iter().any(|reference| reference == &path)
            };
            if !present {
                // The photo was rejected (for example, the reference limit is
                // full); restore what the canvas showed before the preview.
                crate::update_image_view(ui);
            }
        }
        Err(error) => fail(ui, "Could not take a photo", &error),
    }
}

fn capture_failed(ui: &Rc<Ui>, error: &str) {
    fail(ui, "Could not take a photo", error);
    crate::update_image_view(ui);
}

fn fail(ui: &Rc<Ui>, title: &str, error: &str) {
    reset_button(ui);
    let short = error.lines().last().unwrap_or(error);
    ui.status.set_text("Camera capture failed");
    ui.toast
        .add_toast(adw::Toast::new(&format!("{title}: {short}")));
}

fn reset_button(ui: &Ui) {
    ui.camera_busy.set(false);
    ui.camera_button.set_sensitive(true);
    set_photo_button(ui);
}

fn set_photo_button(ui: &Ui) {
    ui.camera_button.set_icon_name(PHOTO_ICON);
    ui.camera_button
        .set_tooltip_text(Some("Take a photo with the camera"));
}

fn stop_preview(preview: Preview) {
    preview.stop.store(true, Ordering::Relaxed);
    let _ = preview.pipeline.set_state(gst::State::Null);
    if let Some(join) = preview.join {
        std::thread::spawn(move || {
            let _ = join.join();
        });
    }
}

async fn is_available() -> bool {
    if ensure_gst().is_err() {
        return false;
    }
    match portal_presence().await {
        Some(present) => present,
        None => {
            !in_flatpak()
                && v4l2_device().is_some()
                && gst::ElementFactory::find("v4l2src").is_some()
        }
    }
}

/// `None` means that no usable camera portal was found.
async fn portal_presence() -> Option<bool> {
    let camera = ashpd::desktop::camera::Camera::new().await.ok()?;
    camera.is_present().await.ok()
}

/// Ask the portal for access and return a PipeWire remote.
///
/// `Ok(None)` is only returned when no camera portal exists; a denial is an
/// error so that sandboxed builds never fall back to the raw device.
fn cached_portal_fd() -> Option<Arc<OwnedFd>> {
    PORTAL_FD
        .get_or_init(|| Mutex::new(None))
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .clone()
}

async fn portal_fd() -> Result<Option<Arc<OwnedFd>>, String> {
    let camera = match ashpd::desktop::camera::Camera::new().await {
        Ok(camera) => camera,
        Err(_) => return Ok(None),
    };
    if cached_portal_fd().is_none() {
        let present = match camera.is_present().await {
            Ok(present) => present,
            // The portal frontend exists but its backend has no camera support.
            Err(_) => return Ok(None),
        };
        if !present {
            tracing::warn!("the camera portal reports no camera");
            return Err("No camera found (portal).".into());
        }
    }

    // Re-check permission every time so a Settings toggle takes effect even
    // with a cached PipeWire remote.
    let request = camera
        .request_access(Default::default())
        .await
        .map_err(portal_error)?;
    request.response().map_err(portal_error)?;

    if let Some(fd) = cached_portal_fd() {
        return Ok(Some(fd));
    }
    let fd = camera
        .open_pipe_wire_remote(Default::default())
        .await
        .map_err(portal_error)?;
    let fd = Arc::new(fd);
    let mut cache = PORTAL_FD
        .get_or_init(|| Mutex::new(None))
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    if let Some(existing) = cache.as_ref() {
        return Ok(Some(existing.clone()));
    }
    *cache = Some(fd.clone());
    Ok(Some(fd))
}

fn portal_error(error: ashpd::Error) -> String {
    use ashpd::PortalError;
    use ashpd::desktop::ResponseError;

    match &error {
        ashpd::Error::Response(ResponseError::Cancelled)
        | ashpd::Error::Portal(PortalError::Cancelled(_)) => "Camera access was cancelled.".into(),
        ashpd::Error::Response(ResponseError::Other) => "Camera access was not granted.".into(),
        ashpd::Error::Portal(PortalError::NotAllowed(_)) => BLOCKED_HINT.into(),
        ashpd::Error::Zbus(ashpd::zbus::Error::MethodError(name, _, _))
            if name.as_str() == "org.freedesktop.portal.Error.NotAllowed" =>
        {
            BLOCKED_HINT.into()
        }
        ashpd::Error::PortalNotFound(_) => "Camera access needs xdg-desktop-portal.".into(),
        _ => format!("Camera permission failed: {error}"),
    }
}

fn ensure_gst() -> Result<(), String> {
    GST_INIT
        .get_or_init(|| gst::init().map_err(|error| error.to_string()))
        .clone()
}

fn in_flatpak() -> bool {
    Path::new("/.flatpak-info").exists()
}

/// First video capture device exposed by the kernel.
fn v4l2_device() -> Option<PathBuf> {
    (0..10)
        .map(|index| PathBuf::from(format!("/dev/video{index}")))
        .find(|path| path.exists())
}

async fn run_blocking<T, F>(task: F) -> Result<T, String>
where
    T: Send + 'static,
    F: FnOnce() -> Result<T, String> + Send + 'static,
{
    let (sender, receiver) = async_channel::bounded(1);
    std::thread::spawn(move || {
        let _ = sender.send_blocking(task());
    });
    receiver
        .recv()
        .await
        .map_err(|_| "Camera capture stopped.".to_string())?
}

fn camera_provider(fd: &Arc<OwnedFd>) -> Result<gst::DeviceProvider, String> {
    let mut session = CAMERA_SESSION
        .get_or_init(|| Mutex::new(None))
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    if let Some(existing) = session.as_ref()
        && Arc::ptr_eq(&existing.fd, fd)
    {
        return Ok(existing.provider.clone());
    }
    // A different portal remote replaces the old provider; dropping it stops
    // the provider and releases its PipeWire core.
    let _ = session.take();

    let provider =
        gst::DeviceProviderFactory::by_name("pipewiredeviceprovider").ok_or_else(|| {
            "The PipeWire GStreamer plugin is not installed. Install gst-plugin-pipewire."
                .to_string()
        })?;
    provider.set_property("fd", fd.as_raw_fd());
    provider
        .start()
        .map_err(|error| format!("Could not start the camera provider: {error}"))?;
    *session = Some(CameraSession {
        fd: fd.clone(),
        provider: provider.clone(),
    });
    Ok(provider)
}

fn camera_device(provider: &gst::DeviceProvider) -> Result<gst::Device, String> {
    // The camera node can be briefly missing while a previous stream shuts
    // down; a persistent provider usually has it, but give PipeWire a moment.
    let deadline = Instant::now() + Duration::from_secs(3);
    loop {
        if let Some(device) = provider
            .devices()
            .into_iter()
            .find(|device| device.device_class().starts_with("Video"))
        {
            return Ok(device);
        }
        if Instant::now() >= deadline {
            tracing::warn!("the PipeWire provider returned no camera device");
            return Err("No camera found (PipeWire).".to_string());
        }
        std::thread::sleep(Duration::from_millis(100));
    }
}

fn start_preview_pipeline(
    fd: Arc<OwnedFd>,
    signal: async_channel::Sender<()>,
) -> Result<Preview, String> {
    ensure_gst()?;
    let provider = camera_provider(&fd)?;
    let device = camera_device(&provider)?;
    tracing::debug!(
        camera = %device.display_name(),
        class = %device.device_class(),
        "camera preview"
    );

    let source = device
        .create_element(None)
        .map_err(|error| format!("Could not open the camera: {error}"))?;
    let convert = element("videoconvert")?;
    let filter = element("capsfilter")?;
    let caps = gst::Caps::builder("video/x-raw")
        .field("format", "RGBA")
        .build();
    filter.set_property("caps", &caps);
    let sink = element("appsink")?;
    sink.set_property("max-buffers", 1u32);
    sink.set_property("drop", true);
    sink.set_property("sync", false);
    sink.set_property("emit-signals", false);
    let appsink = sink
        .clone()
        .downcast::<AppSink>()
        .map_err(|_| "Could not create the camera sink.".to_string())?;

    let pipeline = gst::Pipeline::new();
    pipeline
        .add_many([&source, &convert, &filter, &sink])
        .map_err(|error| format!("Could not build the camera preview: {error}"))?;
    gst::Element::link_many([&source, &convert, &filter, &sink])
        .map_err(|error| format!("Could not link the camera preview: {error}"))?;
    start_pipeline(&pipeline)?;

    let Some(first) = appsink.try_pull_sample(gst::ClockTime::from_seconds(5)) else {
        let _ = pipeline.set_state(gst::State::Null);
        return Err("The camera did not send a frame.".to_string());
    };
    let Some(frame) = frame_from_sample(&first) else {
        let _ = pipeline.set_state(gst::State::Null);
        return Err("The camera frame is invalid.".to_string());
    };

    let latest = Arc::new(Mutex::new(Some(frame)));
    let stop = Arc::new(AtomicBool::new(false));
    let _ = signal.try_send(());
    let join = spawn_preview_worker(appsink, latest.clone(), stop.clone(), signal);
    Ok(Preview {
        pipeline,
        latest,
        stop,
        join: Some(join),
    })
}

fn spawn_preview_worker(
    appsink: AppSink,
    latest: Arc<Mutex<Option<Frame>>>,
    stop: Arc<AtomicBool>,
    signal: async_channel::Sender<()>,
) -> std::thread::JoinHandle<()> {
    std::thread::spawn(move || {
        while !stop.load(Ordering::Relaxed) {
            let Some(sample) = appsink.try_pull_sample(gst::ClockTime::from_mseconds(200)) else {
                continue;
            };
            let Some(frame) = frame_from_sample(&sample) else {
                continue;
            };
            if let Ok(mut latest) = latest.lock() {
                *latest = Some(frame);
            }
            let _ = signal.try_send(());
        }
    })
}

fn spawn_frame_loop(ui: Rc<Ui>, signal: async_channel::Receiver<()>) {
    glib::spawn_future_local(async move {
        while signal.recv().await.is_ok() {
            if ui.camera_preview.borrow().is_none() {
                break;
            }
            let frame = ui
                .camera_preview
                .borrow()
                .as_ref()
                .and_then(|preview| preview.latest.lock().ok().and_then(|frame| frame.clone()));
            let Some(frame) = frame else {
                continue;
            };
            if let Some(texture) = frame_texture(frame) {
                crate::show_camera_preview_frame(&ui, &texture);
            }
        }
    });
}

fn frame_from_sample(sample: &gst::Sample) -> Option<Frame> {
    let caps = sample.caps()?;
    let structure = caps.structure(0)?;
    let width = structure.get::<i32>("width").ok()?;
    let height = structure.get::<i32>("height").ok()?;
    if width <= 0 || height <= 0 {
        return None;
    }
    let buffer = sample.buffer()?;
    let map = buffer.map_readable().ok()?;
    let data = map.as_slice();
    let expected = width as usize * height as usize * 4;
    if data.len() < expected {
        return None;
    }
    Some(Frame {
        width: width as u32,
        height: height as u32,
        rgba: data[..expected].to_vec(),
    })
}

fn frame_texture(frame: Frame) -> Option<gdk::Texture> {
    let bytes = glib::Bytes::from_owned(frame.rgba);
    Some(
        gdk::MemoryTexture::new(
            frame.width as i32,
            frame.height as i32,
            gdk::MemoryFormat::R8g8b8a8,
            &bytes,
            frame.width as usize * 4,
        )
        .upcast(),
    )
}

fn element(name: &str) -> Result<gst::Element, String> {
    gst::ElementFactory::make(name)
        .build()
        .map_err(|error| format!("The GStreamer {name} plugin is missing: {error}"))
}

fn start_pipeline(pipeline: &gst::Pipeline) -> Result<(), String> {
    if let Err(error) = pipeline.set_state(gst::State::Playing) {
        // The state-change error itself is generic; the actual reason (device
        // busy, not-negotiated, PipeWire permission, missing plugin) is on the
        // bus.
        let detail = pipeline
            .bus()
            .and_then(|bus| {
                bus.timed_pop_filtered(gst::ClockTime::from_seconds(2), &[gst::MessageType::Error])
            })
            .and_then(|message| match message.view() {
                gst::MessageView::Error(error) => Some(match error.debug() {
                    Some(debug) if !debug.is_empty() => {
                        format!("{} ({debug})", error.error())
                    }
                    _ => error.error().to_string(),
                }),
                _ => None,
            });
        let _ = pipeline.set_state(gst::State::Null);
        return Err(match detail {
            Some(detail) => format!("Could not start the camera: {detail}"),
            None => format!("Could not start the camera: {error}"),
        });
    }
    Ok(())
}

fn capture_v4l2(device: &Path, path: &Path) -> Result<(), String> {
    ensure_gst()?;
    let source = element("v4l2src")?;
    source.set_property("device", device.to_string_lossy().as_ref());
    let decode = element("decodebin")?;
    let convert = element("videoconvert")?;
    let encode = element("pngenc")?;
    let sink = element("filesink")?;
    sink.set_property("location", path.to_string_lossy().as_ref());
    if source.find_property("num-buffers").is_some() {
        source.set_property("num-buffers", 1);
    }

    let pipeline = gst::Pipeline::new();
    pipeline
        .add_many([&source, &decode, &convert, &encode, &sink])
        .map_err(|error| format!("Could not build the camera pipeline: {error}"))?;
    source
        .link(&decode)
        .map_err(|error| format!("Could not link the camera source: {error}"))?;
    gst::Element::link_many([&convert, &encode, &sink])
        .map_err(|error| format!("Could not link the camera pipeline: {error}"))?;
    let convert_pad = convert.clone();
    decode.connect_pad_added(move |_, pad| {
        let caps = pad.query_caps(None);
        let is_audio = caps
            .structure(0)
            .is_some_and(|structure| structure.name().starts_with("audio/"));
        if is_audio {
            return;
        }
        let Some(sink_pad) = convert_pad.static_pad("sink") else {
            return;
        };
        if !sink_pad.is_linked() {
            let _ = pad.link(&sink_pad);
        }
    });
    start_pipeline(&pipeline)?;

    let bus = pipeline
        .bus()
        .ok_or_else(|| "The camera pipeline has no bus.".to_string())?;
    let message = bus.timed_pop_filtered(
        gst::ClockTime::from_seconds(20),
        &[gst::MessageType::Eos, gst::MessageType::Error],
    );
    let result = match message {
        Some(message) => match message.view() {
            gst::MessageView::Eos(_) => {
                if path.metadata().is_ok_and(|metadata| metadata.len() > 0) {
                    Ok(())
                } else {
                    Err("The camera returned an empty photo.".into())
                }
            }
            gst::MessageView::Error(error) => {
                Err(format!("Camera capture failed: {}", error.error()))
            }
            _ => Err("Camera capture ended unexpectedly.".into()),
        },
        None => Err("Camera capture timed out.".into()),
    };
    let _ = pipeline.set_state(gst::State::Null);
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    #[ignore = "opens the camera and needs portal permission"]
    fn portal_preview_probe() {
        let context = glib::MainContext::default();
        for attempt in 0..3 {
            let fd = context
                .block_on(portal_fd())
                .expect("portal request")
                .expect("a camera");
            let (sender, receiver) = async_channel::bounded(1);
            let preview = start_preview_pipeline(fd, sender).expect("preview");
            std::thread::sleep(Duration::from_millis(400));
            assert!(preview.latest.lock().unwrap().is_some(), "no preview frame");
            let _ = receiver.try_recv();
            stop_preview(preview);
            std::thread::sleep(Duration::from_millis(300));
            println!("attempt {attempt}: preview ok");
        }
    }

    #[test]
    fn portal_denial_points_at_settings() {
        let error = ashpd::Error::Portal(ashpd::PortalError::NotAllowed("no".into()));
        assert!(portal_error(error).contains("Privacy & Security"));
    }

    #[test]
    fn cancelled_dialog_is_not_a_permission_block() {
        let error = ashpd::Error::Response(ashpd::desktop::ResponseError::Cancelled);
        let message = portal_error(error);
        assert!(message.contains("cancelled"));
        assert!(!message.contains("Privacy & Security"));
    }
}

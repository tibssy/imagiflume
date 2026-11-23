//! One-shot camera capture.
//!
//! When a desktop portal is available, capture goes through
//! `org.freedesktop.portal.Camera`: the portal asks the user for permission,
//! records the answer in the desktop's permission store and hands back a
//! PipeWire file descriptor. Frames are decoded with GStreamer.
//!
//! Native sessions without a camera portal fall back to V4L2, but a sandboxed
//! app never does: its camera access must stay visible to the portal.

use std::os::fd::{AsRawFd, OwnedFd};
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::sync::OnceLock;

use adw::prelude::*;
use gstreamer as gst;
use gstreamer::prelude::{
    DeviceExt, DeviceProviderExt, DeviceProviderExtManual, ElementExt, ElementExtManual,
    GstBinExtManual, PadExt,
};
use gtk::glib;
use imagiflume_core::paths;

use crate::{Ui, add_references, now_millis};

const BLOCKED_HINT: &str =
    "Camera access is blocked. Enable Imagiflume in Settings → Privacy & Security → Camera.";

static GST_INIT: OnceLock<Result<(), String>> = OnceLock::new();

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

/// Grab one frame and add it as an edit reference.
pub fn capture(ui: &Rc<Ui>) {
    if ui.camera_busy.replace(true) {
        return;
    }
    ui.camera_button.set_sensitive(false);
    ui.status.set_text("Taking a photo…");

    let directory = paths::cache_dir().join("camera");
    if let Err(error) = std::fs::create_dir_all(&directory) {
        finish(ui, Err(format!("Could not prepare the camera: {error}")));
        return;
    }

    let path = directory.join(format!("camera-{}.png", now_millis()));
    let ui = ui.clone();
    glib::spawn_future_local(async move {
        let outcome = capture_frame(&path).await;
        finish(&ui, outcome.map(|()| path));
    });
}

fn finish(ui: &Rc<Ui>, outcome: Result<PathBuf, String>) {
    ui.camera_busy.set(false);
    ui.camera_button.set_sensitive(true);
    match outcome {
        Ok(path) => add_references(ui, vec![path]),
        Err(error) => {
            let short = error.lines().last().unwrap_or(&error).to_string();
            ui.status.set_text("Camera capture failed");
            ui.toast
                .add_toast(adw::Toast::new(&format!("Could not take a photo: {short}")));
        }
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

async fn capture_frame(path: &Path) -> Result<(), String> {
    match portal_fd().await? {
        Some(fd) => {
            let path = path.to_owned();
            run_blocking(move || capture_pipewire(fd, &path)).await
        }
        None => {
            if in_flatpak() {
                return Err("Camera access needs the desktop camera portal.".into());
            }
            let device = v4l2_device().ok_or_else(|| "No camera found.".to_string())?;
            let path = path.to_owned();
            run_blocking(move || capture_v4l2(&device, &path)).await
        }
    }
}

/// Ask the portal for access and return a PipeWire remote.
///
/// `Ok(None)` is only returned when no camera portal exists; a denial is an
/// error so that sandboxed builds never fall back to the raw device.
async fn portal_fd() -> Result<Option<OwnedFd>, String> {
    let camera = match ashpd::desktop::camera::Camera::new().await {
        Ok(camera) => camera,
        Err(_) => return Ok(None),
    };
    let present = match camera.is_present().await {
        Ok(present) => present,
        // The portal frontend exists but its backend has no camera support.
        Err(_) => return Ok(None),
    };
    if !present {
        return Err("No camera found.".into());
    }

    let request = camera
        .request_access(Default::default())
        .await
        .map_err(portal_error)?;
    request.response().map_err(portal_error)?;
    let fd = camera
        .open_pipe_wire_remote(Default::default())
        .await
        .map_err(portal_error)?;
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

async fn run_blocking<F>(task: F) -> Result<(), String>
where
    F: FnOnce() -> Result<(), String> + Send + 'static,
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

fn capture_pipewire(fd: OwnedFd, path: &Path) -> Result<(), String> {
    ensure_gst()?;
    let provider =
        gst::DeviceProviderFactory::by_name("pipewiredeviceprovider").ok_or_else(|| {
            "The PipeWire GStreamer plugin is not installed. Install gst-plugin-pipewire."
                .to_string()
        })?;

    provider.set_property("fd", fd.as_raw_fd());
    provider
        .start()
        .map_err(|error| format!("Could not start the camera provider: {error}"))?;

    let result = (|| {
        let device = provider
            .devices()
            .into_iter()
            .find(|device| device.device_class().starts_with("Video"))
            .ok_or_else(|| "No camera found.".to_string())?;
        tracing::debug!(camera = %device.display_name(), "capturing a camera frame");
        let source = device
            .create_element(None)
            .map_err(|error| format!("Could not open the camera: {error}"))?;
        run_pipeline(source, path)
    })();

    provider.stop();
    provider.set_property("fd", -1);
    result
}

fn capture_v4l2(device: &Path, path: &Path) -> Result<(), String> {
    ensure_gst()?;
    let source = gst::ElementFactory::make("v4l2src")
        .build()
        .map_err(|error| format!("The GStreamer v4l2src plugin is not installed: {error}"))?;
    source.set_property("device", device.to_string_lossy().as_ref());
    run_pipeline(source, path)
}

fn run_pipeline(source: gst::Element, path: &Path) -> Result<(), String> {
    let pipeline = gst::Pipeline::new();
    let decode = gst::ElementFactory::make("decodebin")
        .build()
        .map_err(|error| format!("The GStreamer decodebin plugin is missing: {error}"))?;
    let convert = gst::ElementFactory::make("videoconvert")
        .build()
        .map_err(|error| format!("The GStreamer videoconvert plugin is missing: {error}"))?;
    let encode = gst::ElementFactory::make("pngenc")
        .build()
        .map_err(|error| format!("The GStreamer pngenc plugin is missing: {error}"))?;
    let sink = gst::ElementFactory::make("filesink")
        .build()
        .map_err(|error| format!("The GStreamer filesink plugin is missing: {error}"))?;

    sink.set_property("location", path.to_string_lossy().as_ref());
    if source.find_property("num-buffers").is_some() {
        source.set_property("num-buffers", 1);
    }

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
        // Camera nodes are video-only in practice, but never link an audio
        // pad into videoconvert if a device exposes one.
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

    pipeline
        .set_state(gst::State::Playing)
        .map_err(|error| format!("Could not start the camera: {error}"))?;

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

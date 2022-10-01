use std::thread::{spawn, JoinHandle};

use gst::{element_error, prelude::*};
use gstreamer as gst;
use gstreamer_app as gst_app;
use tokio::sync::mpsc::Sender;

#[derive(Clone)]
pub struct VideoConfig {
    device: Option<String>,
}

impl Default for VideoConfig {
    fn default() -> Self {
        Self {
            device: Some("/dev/video0".to_string()),
        }
    }
}

pub fn create_video(config: VideoConfig) -> Result<gst_app::AppSink, gst::glib::BoolError> {
    gst::init().ok();
    let source = gst::ElementFactory::make("v4l2src", Some("source"))?;
    let video_convert = gst::ElementFactory::make("videoconvert", Some("videoconvert"))?;
    let vp8enc = gst::ElementFactory::make("vp8enc", Some("vp8enc"))?;
    let rtp = gst::ElementFactory::make("rtpvp8pay", Some("rtp"))?;
    let app_sink = gst::ElementFactory::make("appsink", Some("udp sink"))?;

    if let Some(device) = config.device {
        source.set_property_from_str("device", &device);
    }

    let pipeline = gst::Pipeline::new(Some("live-pipe"));
    pipeline.add_many(&[&source, &video_convert, &vp8enc, &rtp, &app_sink])?;
    gst::Element::link_many(&[&source, &video_convert, &vp8enc, &rtp, &app_sink])?;

    let app_sink = app_sink.dynamic_cast::<gst_app::AppSink>().map_err(|_| {
        gst::glib::BoolError::new(
            "Sink element is expected to be an app sink!",
            "video.rs",
            "dynamic_cast",
            23,
        )
    })?;
    Ok(app_sink)
}

pub fn setup_listeners(
    app_sink: gst_app::AppSink,
    sender: Sender<Vec<u8>>,
) -> Option<JoinHandle<()>> {
    app_sink.set_callbacks(
        gst_app::AppSinkCallbacks::builder()
            .new_sample(move |app_sink| {
                let sample = app_sink.pull_sample().map_err(|_| gst::FlowError::Eos)?;
                let buffer = sample.buffer().ok_or_else(|| {
                    element_error!(
                        app_sink,
                        gst::ResourceError::Failed,
                        ("Failed to get buffer from appsink")
                    );

                    gst::FlowError::Error
                })?;
                let map = buffer.map_readable().map_err(|_| {
                    element_error!(
                        app_sink,
                        gst::ResourceError::Failed,
                        ("Failed to map buffer readable")
                    );

                    gst::FlowError::Error
                })?;
                sender.try_send(map.to_vec()).ok(); // todo: handle error when stream closes
                Ok(gst::FlowSuccess::Ok)
            })
            .build(),
    );

    if let Some(bus) = app_sink.bus() {
        let handle = spawn(move || {
            for msg in bus.iter_timed(gst::ClockTime::NONE) {
                use gst::MessageView;
                match msg.view() {
                    MessageView::Eos(eos) => {
                        println!("source exhausted! {eos:?}");
                        break;
                    }
                    MessageView::Error(err) => {
                        println!(
                            "error from element {:?} {}",
                            err.src().map(|s| s.path_string()),
                            err.error()
                        );
                        break;
                    }
                    MessageView::StateChanged(state) => {
                        if state.src().map(|s| s == app_sink).unwrap_or(false) {
                            println!(
                                "state changed from {:?} to {:?}",
                                state.old(),
                                state.current()
                            );
                        }
                    }
                    _ => (),
                }
            }
        });
        return Some(handle);
    }
    None
}

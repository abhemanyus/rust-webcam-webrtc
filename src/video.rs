use anyhow::{format_err, Result};
use serde::Deserialize;
use std::thread::spawn;

use gst::{element_error, prelude::*, Element, Pipeline, State};
use gstreamer as gst;
use gstreamer_app as gst_app;
use tokio::sync::mpsc::Sender;

#[derive(Deserialize, Clone)]
pub struct VideoConfig {
    device: Option<String>,
}

impl Default for VideoConfig {
    fn default() -> Self {
        Self { device: None }
    }
}

pub struct Video {
    pipeline: Pipeline,
    app_sink: Element,
}

impl Video {
    pub fn new(config: VideoConfig) -> Result<Self, gst::glib::BoolError> {
        gst::init().unwrap();
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

        if let Some(bus) = pipeline.bus() {
            let pipeline_clone = pipeline.clone();
            println!("Listening to pipeline");
            let _ = spawn(move || {
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
                            if state.src().map(|s| s == pipeline_clone).unwrap_or(false) {
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
                println!("Stopped listening to pipeline");
            });
        }
        Ok(Self { pipeline, app_sink })
    }

    pub fn setup_listeners(&self, sender: Sender<Vec<u8>>) -> Result<()> {
        // let app_sink = self.app_sink.take().ok_or(format_err!("app sink already taken!"))?;
        let app_sink = self
            .app_sink
            .clone()
            .dynamic_cast::<gst_app::AppSink>()
            .map_err(|_| format_err!("Cast to app sink failed"))?;
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
                    sender.blocking_send(map.to_vec()).ok(); // todo: handle error when stream closes
                    Ok(gst::FlowSuccess::Ok)
                })
                .build(),
        );
        Ok(())
    }

    pub fn start_video(&self) -> Result<()> {
        self.pipeline.set_state(State::Playing)?;
        Ok(())
    }
}

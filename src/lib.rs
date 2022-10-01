use std::{
    sync::Arc,
    thread::{sleep, spawn},
    time::Duration,
};

use anyhow::Result;
use gstreamer::{traits::ElementExt, State};
use tokio::{
    join,
    sync::{mpsc::channel, Notify},
};

mod socket;
mod video;
mod webrtc;

#[derive(Clone)]
pub struct Config {
    socket: socket::SocketConfig,
    video: video::VideoConfig,
    web_rtc: webrtc::IceConfig,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            socket: Default::default(),
            video: Default::default(),
            web_rtc: Default::default(),
        }
    }
}

pub async fn setup(config: Config) -> Result<()> {
    let (socket_sender, socket_receiver) = channel::<webrtc::Payload>(1);
    let (peer_sender, peer_receiver) = channel::<webrtc::Payload>(1);
    let (video_sender, video_receiver) = channel::<Vec<u8>>(1);
    let start_video = Arc::new(Notify::new());
    let video_start = start_video.clone();
    let start_socket = Arc::new(Notify::new());
    let socket_start = start_socket.clone();
    let _socket_handle = spawn(move || {
        let socket = loop {
            if let Ok(sock) = socket::create_socket(
                config.socket.clone(),
                peer_sender.clone(),
                start_video.clone(),
            ) {
                break sock;
            }
            println!("Socket reconnecting in 2 seconds...");
            sleep(Duration::from_secs(2));
        };
        let _socket_listener_handle = socket::setup_listeners(socket, socket_receiver);
        start_socket.notify_one();
    });
    socket_start.notified().await;
    let peer_connection = webrtc::create_peer_connection(config.web_rtc).await?;
    let peer_connection = Arc::new(peer_connection);
    webrtc::setup_callbacks(peer_connection.clone(), socket_sender.clone()).await;
    let webrtc_listener_handle =
        webrtc::setup_listeners(peer_connection.clone(), peer_receiver, socket_sender).await;
    let webrtc_stream_handle = webrtc::setup_stream(&peer_connection, video_receiver).await?;
    let (pipeline, app_sink) = video::create_video(config.video)?;
    let _video_listener = video::setup_listeners(app_sink, video_sender)?;
    video_start.notified().await;
    println!("Starting video");
    pipeline.set_state(State::Playing)?;
    let (_, _) = join!(webrtc_listener_handle, webrtc_stream_handle);
    Ok(())
}

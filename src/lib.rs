use std::{fs, sync::Arc, thread::{sleep, spawn}, time::Duration};

use anyhow::Result;
use serde::Deserialize;
use tokio::{
    join,
    sync::{mpsc::channel, Notify},
};

mod socket;
mod video;
mod wrtc;

use socket::Socket;
use video::Video;
use wrtc::WebRTC;

#[derive(Deserialize, Clone)]
pub struct Config {
    socket: socket::SocketConfig,
    video: video::VideoConfig,
    web_rtc: wrtc::IceConfig,
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

impl Config {
    pub fn parse(path: &str) -> Result<Self> {
        let config = fs::read(path)?;
        let config: Self = serde_json::from_slice(&config)?;
        Ok(config)
    }
}

pub async fn setup(config: Config) -> Result<()> {
    let (socket_sender, socket_receiver) = channel::<wrtc::Payload>(1);
    let (peer_sender, peer_receiver) = channel::<wrtc::Payload>(1);
    let (video_sender, video_receiver) = channel::<Vec<u8>>(1);
    let start_video = Arc::new(Notify::new());
    let video_start = start_video.clone();
    let _socket_handle = spawn(move || {
        let socket = loop {
            if let Ok(sock) = Socket::new(
                config.socket.clone(),
                peer_sender.clone(),
                start_video.clone(),
            ) {
                break sock;
            }
            println!("Socket reconnecting in 2 seconds...");
            sleep(Duration::from_secs(2));
        };
        let _socket_listener_handle = socket.setup_listeners(socket_receiver);
    });
    tokio::time::sleep(Duration::from_secs(2)).await;
    let peer_connection = WebRTC::new(config.web_rtc).await?;
    peer_connection.setup_callbacks(socket_sender.clone()).await;
    let wrtc_listener_handle = peer_connection
        .setup_listeners(peer_receiver, socket_sender)
        .await;
    let wrtc_stream_handle = peer_connection.setup_stream(video_receiver).await?;
    let video_pipe = Video::new(config.video)?;
    let _video_listener = video_pipe.setup_listeners(video_sender)?;
    video_start.notified().await;
    println!("Starting video");
    video_pipe.start_video()?;
    let (_, _) = join!(wrtc_listener_handle, wrtc_stream_handle);
    Ok(())
}

use std::{
    sync::Arc,
    thread::{spawn, JoinHandle},
};

use rust_socketio::{Client, ClientBuilder, Error};
use tokio::sync::{
    mpsc::{Receiver, Sender},
    Notify,
};

use crate::webrtc::Payload;

pub enum SocketEvent {
    BEGIN,
    HOLLER,
}

impl From<SocketEvent> for rust_socketio::Event {
    fn from(event: SocketEvent) -> Self {
        match event {
            SocketEvent::BEGIN => Self::Custom("begin".to_string()),
            SocketEvent::HOLLER => Self::Custom("holler".to_string()),
        }
    }
}

#[derive(Clone)]
pub struct SocketConfig {
    url: String,
}

impl Default for SocketConfig {
    fn default() -> Self {
        Self {
            url: env!("SOCK_URL").to_string(),
        }
    }
}

pub fn create_socket(
    config: SocketConfig,
    sender: Sender<Payload>,
    start_video: Arc<Notify>,
) -> Result<Client, Error> {
    let socket = ClientBuilder::new(config.url)
        .on(SocketEvent::BEGIN, move |_, _| {
            println!("Received begin event");
            start_video.notify_one();
        })
        .on(SocketEvent::HOLLER, move |payload, _| {
            if let rust_socketio::Payload::String(payload) = payload {
                match serde_json::from_str::<Payload>(&payload) {
                    Ok(payload) => {
                        println!("Socket received payload");
                        match sender.blocking_send(payload) {
                            Ok(_) => println!("Forwarded payload to channel"),
                            Err(err) => println!("Failed to forward payload: {err}"),
                        }
                    }
                    Err(err) => println!("Failed to parse payload {err}"),
                }
            } else {
                println!("Invalid payload");
            }
        })
        .connect()?;
    Ok(socket)
}

pub fn setup_listeners(socket: Client, mut receiver: Receiver<Payload>) -> JoinHandle<()> {
    spawn(move || {
        while let Some(payload) = receiver.blocking_recv() {
            if let Ok(payload) = serde_json::to_value(payload) {
                match socket.emit(SocketEvent::HOLLER, payload) {
                    Ok(_) => println!("Sent payload to remote"),
                    Err(err) => println!("Failed to send payload to remote: {err}"),
                }
            } else {
                println!("Failed to parse payload to json");
            }
        }
        println!("Socket stopped listening for events");
    })
}
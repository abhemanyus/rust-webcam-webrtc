use rust_socketio::{Client, ClientBuilder, Payload};
use serde_json::json;

fn start(payload: Payload, _socket: Client) {
    match payload {
        Payload::String(str) => println!("Received: {}", str),
        Payload::Binary(bin_data) => println!("Received bytes: {:#?}", bin_data),
    }
}

fn message(payload: Payload, _socket: Client) {
    match payload {
        Payload::String(str) => println!("Received: {}", str),
        Payload::Binary(bin_data) => println!("Received bytes: {:#?}", bin_data),
    }
}

fn main() {
    // get a socket that is connected to the admin namespace
    let socket = ClientBuilder::new("http://localhost:3000?uin=BOB&type=MavDrone")
        .on("start", start)
        .on("message", message)
        .connect()
        .expect("Connection failed");
}

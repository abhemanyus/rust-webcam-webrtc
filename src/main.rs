use anyhow::Result;
use rust_socketio::{Client, ClientBuilder, Event};
use serde::Deserialize;
use std::{sync::Arc, thread};
use tokio::{
    net::UdpSocket,
    sync::{
        mpsc::{self, Receiver, Sender},
        Mutex, Notify,
    },
};

use webrtc::{
    api::{
        interceptor_registry::register_default_interceptors,
        media_engine::{MediaEngine, MIME_TYPE_VP8},
        APIBuilder,
    },
    ice_transport::{
        ice_candidate::RTCIceCandidateInit, ice_credential_type::RTCIceCredentialType,
        ice_server::RTCIceServer,
    },
    interceptor::registry::Registry,
    peer_connection::{
        configuration::RTCConfiguration,
        peer_connection_state::RTCPeerConnectionState,
        sdp::{sdp_type::RTCSdpType, session_description::RTCSessionDescription},
        signaling_state::RTCSignalingState,
        RTCPeerConnection,
    },
    rtp_transceiver::rtp_codec::RTCRtpCodecCapability,
    track::track_local::{
        track_local_static_rtp::TrackLocalStaticRTP, TrackLocal, TrackLocalWriter,
    },
    Error,
};

use gst::prelude::*;
use gstreamer as gst;

const POLITE: bool = true;

#[derive(Debug, Clone, Copy)]
enum SocketEvent {
    HOLLER,
    BEGIN,
}

#[derive(Deserialize)]
struct MyPayload {
    candidate: Option<RTCIceCandidateInit>,
    sdp: Option<RTCSessionDescription>,
}

impl From<SocketEvent> for Event {
    fn from(e: SocketEvent) -> Self {
        match e {
            SocketEvent::BEGIN => Self::Custom("begin".to_string()),
            SocketEvent::HOLLER => Self::Custom("holler".to_string()),
        }
    }
}

async fn create_peer_connection(
    peer_send: Sender<(SocketEvent, serde_json::Value)>,
    mut socket_recv: Receiver<(SocketEvent, String, Client)>,
) -> Result<Arc<RTCPeerConnection>> {
    let mut m = MediaEngine::default();

    m.register_default_codecs()?;

    // Create a InterceptorRegistry. This is the user configurable RTP/RTCP Pipeline.
    // This provides NACKs, RTCP Reports and other features. If you use `webrtc.NewPeerConnection`
    // this is enabled by default. If you are manually managing You MUST create a InterceptorRegistry
    // for each PeerConnection.
    let mut registry = Registry::new();

    // Use the default set of Interceptors
    registry = register_default_interceptors(registry, &mut m)?;

    // Create the API object with the MediaEngine
    let api = APIBuilder::new()
        .with_media_engine(m)
        .with_interceptor_registry(registry)
        .build();

    // Prepare the configuration
    let config = RTCConfiguration {
        ice_servers: vec![RTCIceServer {
            urls: vec!["turn:localhost:5450".to_owned()],
            username: "test".to_owned(),
            credential: "test".to_owned(),
            credential_type: RTCIceCredentialType::Password,
        }],
        ..Default::default()
    };

    // Create a new RTCPeerConnection
    let peer_connection = Arc::new(api.new_peer_connection(config).await?);

    let notify_tx = Arc::new(Notify::new());
    let notify_video = notify_tx.clone();

    let (done_tx, _done_rx) = tokio::sync::mpsc::channel::<()>(1);
    let video_done_tx = done_tx.clone();

    let video_track = Arc::new(TrackLocalStaticRTP::new(
        RTCRtpCodecCapability {
            mime_type: MIME_TYPE_VP8.to_owned(),
            ..Default::default()
        },
        "video".to_owned(),
        "webrtc-rs".to_owned(),
    ));

    // Add this newly created track to the PeerConnection
    let rtp_sender = peer_connection
        .add_track(Arc::clone(&video_track) as Arc<dyn TrackLocal + Send + Sync>)
        .await?;

    // Read incoming RTCP packets
    // Before these packets are returned they are processed by interceptors. For things
    // like NACK this needs to be called.
    tokio::spawn(async move {
        let mut rtcp_buf = vec![0u8; 1500];
        while let Ok((_, _)) = rtp_sender.read(&mut rtcp_buf).await {}
        Result::<()>::Ok(())
    });

    tokio::spawn(async move {
        let _ = notify_video.notified().await;
        let listener = UdpSocket::bind("127.0.0.1:5004")
            .await
            .expect("failed to listen");
        let pipeline = start_stream().expect("failed to start stream");
        println!("play video from udp");
        let mut inbound_rtp_packet = vec![0u8; 1600]; // UDP MTU
        while let Ok((n, _)) = listener.recv_from(&mut inbound_rtp_packet).await {
            if let Err(err) = video_track.write(&inbound_rtp_packet[..n]).await {
                if Error::ErrClosedPipe == err {
                    println!("The peerConnection has been closed.");
                } else {
                    println!("video_track write err: {}", err);
                }
                let _ = done_tx.try_send(());
                return;
            }
        }

        let _ = video_done_tx.try_send(());
        pipeline.set_state(gst::State::Null).expect("stop pipeline");
    });

    let peer_send_clone = peer_send.clone();
    peer_connection
        .on_ice_candidate(Box::new(move |candidate| {
            let peer_send = peer_send_clone.clone();
            Box::pin(async move {
                if let Some(candidate) = candidate {
                    let candidate = candidate.to_json().await.expect("candidate to json");
                    peer_send
                        .send((
                            SocketEvent::HOLLER,
                            serde_json::json!({ "candidate": candidate }),
                        ))
                        .await
                        .expect("send candidate");
                    println!("send candidate!");
                }
            })
        }))
        .await;

    let peer_send_clone = peer_send.clone();
    let peer_connection_clone = peer_connection.clone();
    let making_offer = Arc::new(Mutex::new(false));
    let making_offer_clone = making_offer.clone();
    peer_connection
        .on_negotiation_needed(Box::new(move || {
            println!("negotiation required!");
            let peer_send = peer_send_clone.clone();
            let peer_connection = peer_connection_clone.clone();
            let making_offer = making_offer_clone.clone();
            Box::pin(async move {
                let mut making_offer = making_offer.lock().await;
                *making_offer = true;
                let offer = peer_connection
                    .create_offer(None)
                    .await
                    .expect("create offer");
                peer_connection
                    .set_local_description(offer.clone())
                    .await
                    .expect("set local desc");
                peer_send
                    .send((SocketEvent::HOLLER, serde_json::json!({ "sdp": offer })))
                    .await
                    .expect("send offer");
                println!("send offer to remote");
            })
        }))
        .await;

    peer_connection
        .on_peer_connection_state_change(Box::new(move |state| {
            if state == RTCPeerConnectionState::Connected {
                println!("Connected!");
                notify_tx.notify_waiters();
            }
            Box::pin(async {})
        }))
        .await;

    println!("peer created! listening to socket events");
    let mut ignore_offer = false;
    while let Some((event, payload, socket)) = socket_recv.recv().await {
        match event {
            SocketEvent::BEGIN => {
                println!("begin command received!");
            }
            SocketEvent::HOLLER => {
                let payload: MyPayload = serde_json::from_str(&payload).expect("parse payload");
                match payload {
                    MyPayload {
                        candidate: Some(candidate),
                        sdp: None,
                    } => {
                        println!("got candidate");
                        let candidate_result = peer_connection.add_ice_candidate(candidate).await;
                        // .expect("add candidate");
                        if !ignore_offer {
                            candidate_result.expect("add candidate")
                        }
                        println!("add candidate");
                    }
                    MyPayload {
                        candidate: None,
                        sdp: Some(sdp),
                    } => {
                        println!("got sdp");
                        let offer_collision = (sdp.sdp_type == RTCSdpType::Offer)
                            && (*making_offer.lock().await
                                || peer_connection.signaling_state() != RTCSignalingState::Stable);
                        ignore_offer = !POLITE && offer_collision;
                        if ignore_offer {
                            println!("ignoring offer");
                            continue;
                        }
                        let sdp_type = sdp.sdp_type;
                        peer_connection
                            .set_remote_description(sdp)
                            .await
                            .or_else(|f| {
                                println!("set remote desc {f}");
                                Ok::<(), ()>(())
                            })
                            .ok();
                        // .expect("set remote desc");
                        println!("set remote desc");
                        if sdp_type == RTCSdpType::Offer {
                            let answer = peer_connection
                                .create_answer(None)
                                .await
                                .expect("create answer");
                            println!("create answer");
                            socket
                                .emit(SocketEvent::HOLLER, serde_json::json!({ "sdp": answer }))
                                .expect("send answer");
                            println!("send answer");
                        }
                    }
                    _ => println!("invalid payload format"),
                }
            }
        }
    }
    Ok(peer_connection)
}

fn create_socket(
    socket_send: Sender<(SocketEvent, String, Client)>,
    mut peer_recv: Receiver<(SocketEvent, serde_json::Value)>,
) -> Result<Arc<Client>> {
    let socket_send_clone = socket_send.clone();
    let socket = ClientBuilder::new("http://localhost:3000?uin=BOB&type=MavDrone")
        .on(SocketEvent::HOLLER, move |payload, client| {
            // println!("socket message event");
            match payload {
                rust_socketio::Payload::Binary(_) => println!("message: got binary data!"),
                rust_socketio::Payload::String(payload) => {
                    // println!("payload data: {}", &payload);
                    socket_send_clone
                        .blocking_send((SocketEvent::HOLLER, payload, client))
                        .ok();
                }
            }
        })
        .on(SocketEvent::BEGIN, move |payload, client| {
            // println!("socket start event");
            match payload {
                rust_socketio::Payload::Binary(_) => println!("start: got binary data!"),
                rust_socketio::Payload::String(payload) => {
                    socket_send
                        .blocking_send((SocketEvent::BEGIN, payload, client))
                        .ok();
                }
            }
        })
        .connect()?;
    println!("socket created! listening to peer events.");
    while let Some((event, data)) = peer_recv.blocking_recv() {
        socket.emit(event, data)?;
        // println!("sent {event:?} to remote");
    }
    println!("no more peer events to listen for!");
    Ok(Arc::new(socket))
}

fn start_stream() -> Result<gst::Pipeline> {
    gst::init().unwrap();

    let source = gst::ElementFactory::make("v4l2src", Some("source")).expect("create source");
    let video_convert =
        gst::ElementFactory::make("videoconvert", Some("videoconvert")).expect("create converter");
    let vp8enc = gst::ElementFactory::make("vp8enc", Some("vp8enc")).expect("create encoder");
    let rtp = gst::ElementFactory::make("rtpvp8pay", Some("rtp")).expect("create rtp");
    let udp_sink = gst::ElementFactory::make("udpsink", Some("udp sink")).expect("create sink");

    udp_sink.set_property_from_str("host", "127.0.0.1");
    udp_sink.set_property_from_str("port", "5004");

    let pipeline = gst::Pipeline::new(Some("live-pipe"));

    pipeline
        .add_many(&[&source, &video_convert, &vp8enc, &rtp, &udp_sink])
        .expect("setup pipeline");
    gst::Element::link_many(&[&source, &video_convert, &vp8enc, &rtp, &udp_sink])
        .expect("link elements");

    pipeline.set_state(gst::State::Null).expect("stop pipeline");

    pipeline
        .set_state(gst::State::Playing)
        .expect("start pipeline");
    let pipeline_clone = pipeline.clone();
    thread::spawn(move || {
        let bus = pipeline_clone.bus().unwrap();
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
        pipeline_clone
            .set_state(gst::State::Null)
            .expect("stop pipeline");
    });

    Ok(pipeline)
}

#[tokio::main]
async fn main() -> Result<()> {
    let (socket_send, socket_recv) = mpsc::channel::<(SocketEvent, String, Client)>(4);
    let (peer_send, peer_recv) = mpsc::channel::<(SocketEvent, serde_json::Value)>(4);
    thread::spawn(move || {
        let _sc = create_socket(socket_send, peer_recv).unwrap();
    });
    let _pc = create_peer_connection(peer_send, socket_recv)
        .await
        .unwrap();
    Ok(())
}

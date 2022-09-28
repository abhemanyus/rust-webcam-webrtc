use anyhow::Result;
use rust_socketio::{Client, ClientBuilder, Event};
use serde::Deserialize;
use std::{fs::File, io::BufReader, sync::Arc, thread, time::Duration};
use tokio::{
    sync::{
        mpsc::{self, Receiver, Sender},
        Notify,
    },
    time::sleep,
};

use webrtc::{
    api::{
        interceptor_registry::register_default_interceptors,
        media_engine::{MediaEngine, MIME_TYPE_H264},
        APIBuilder,
    },
    ice_transport::{
        ice_candidate::RTCIceCandidateInit, ice_credential_type::RTCIceCredentialType,
        ice_server::RTCIceServer,
    },
    interceptor::registry::Registry,
    media::{io::h264_reader::H264Reader, Sample},
    peer_connection::{
        configuration::RTCConfiguration,
        sdp::{sdp_type::RTCSdpType, session_description::RTCSessionDescription},
        RTCPeerConnection,
    },
    rtp_transceiver::rtp_codec::RTCRtpCodecCapability,
    track::track_local::{track_local_static_sample::TrackLocalStaticSample, TrackLocal},
};

#[derive(Debug)]
enum SocketEvent {
    HOLLER,
    START,
}

#[derive(Deserialize)]
struct MyPayload {
    candidate: Option<RTCIceCandidateInit>,
    sdp: Option<RTCSessionDescription>,
}

impl From<SocketEvent> for Event {
    fn from(e: SocketEvent) -> Self {
        match e {
            SocketEvent::START => Self::Custom("start".to_string()),
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
    let _notify_audio = notify_tx.clone();

    let (done_tx, _done_rx) = tokio::sync::mpsc::channel::<()>(1);
    let video_done_tx = done_tx.clone();
    let _audio_done_tx = done_tx.clone();

    let video_track = Arc::new(TrackLocalStaticSample::new(
        RTCRtpCodecCapability {
            mime_type: MIME_TYPE_H264.to_owned(),
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

    let video_file_name = "./output.h264".to_owned();
    tokio::spawn(async move {
        // Open a H264 file and start reading using our H264Reader
        let file = File::open(&video_file_name)?;
        let reader = BufReader::new(file);
        let mut h264 = H264Reader::new(reader);

        println!("waiting to start video feed...");
        // Wait for connection established
        let _ = notify_video.notified().await;

        println!("play video from disk file {}", video_file_name);

        // It is important to use a time.Ticker instead of time.Sleep because
        // * avoids accumulating skew, just calling time.Sleep didn't compensate for the time spent parsing the data
        // * works around latency issues with Sleep
        let mut ticker = tokio::time::interval(Duration::from_millis(33));
        loop {
            let nal = match h264.next_nal() {
                Ok(nal) => nal,
                Err(err) => {
                    println!("All video frames parsed and sent: {}", err);
                    break;
                }
            };

            /*println!(
                "PictureOrderCount={}, ForbiddenZeroBit={}, RefIdc={}, UnitType={}, data={}",
                nal.picture_order_count,
                nal.forbidden_zero_bit,
                nal.ref_idc,
                nal.unit_type,
                nal.data.len()
            );*/

            video_track
                .write_sample(&Sample {
                    data: nal.data.freeze(),
                    duration: Duration::from_secs(1),
                    ..Default::default()
                })
                .await?;

            let _ = ticker.tick().await;
        }

        let _ = video_done_tx.try_send(());

        Result::<()>::Ok(())
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
                }
            })
        }))
        .await;

    let peer_send_clone = peer_send.clone();
    let peer_connection_clone = peer_connection.clone();
    peer_connection
        .on_negotiation_needed(Box::new(move || {
            let peer_send = peer_send_clone.clone();
            let peer_connection = peer_connection_clone.clone();
            Box::pin(async move {
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
            })
        }))
        .await;

    println!("peer created! listening to socket events");
    let peer_connection_clone = peer_connection.clone();
    while let Some((event, payload, socket)) = socket_recv.recv().await {
        match event {
            SocketEvent::START => {
                notify_tx.notify_waiters();
            }
            SocketEvent::HOLLER => {
                let payload: MyPayload = serde_json::from_str(&payload).expect("parse payload");
                match payload {
                    MyPayload {
                        candidate: Some(candidate),
                        sdp: None,
                    } => {
                        peer_connection_clone
                            .add_ice_candidate(candidate)
                            .await
                            .expect("add candidate");
                    }
                    MyPayload {
                        candidate: None,
                        sdp: Some(sdp),
                    } => {
                        let sdp_type = sdp.sdp_type;
                        peer_connection_clone
                            .set_remote_description(sdp)
                            .await
                            .expect("set remote desc");

                        if sdp_type == RTCSdpType::Offer {
                            let answer = peer_connection_clone
                                .create_answer(None)
                                .await
                                .expect("create answer");
                            socket
                                .emit(SocketEvent::HOLLER, serde_json::json!({ "sdp": answer }))
                                .expect("send answer");
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
            println!("socket message event");
            match payload {
                rust_socketio::Payload::Binary(_) => println!("message: got binary data!"),
                rust_socketio::Payload::String(payload) => {
                    socket_send_clone
                        .blocking_send((SocketEvent::HOLLER, payload, client))
                        .ok();
                }
            }
        })
        .on(SocketEvent::START, move |payload, client| {
            println!("socket start event");
            match payload {
                rust_socketio::Payload::Binary(_) => println!("start: got binary data!"),
                rust_socketio::Payload::String(payload) => {
                    socket_send
                        .blocking_send((SocketEvent::START, payload, client))
                        .ok();
                }
            }
        })
        .connect()?;
    println!("socket created! listening to peer events.");
    while let Some((event, data)) = peer_recv.blocking_recv() {
        socket.emit(event, data)?;
    }
    println!("no more peer events to listen for!");
    Ok(Arc::new(socket))
}

#[tokio::main]
async fn main() -> Result<()> {
    let (socket_send, socket_recv) = mpsc::channel::<(SocketEvent, String, Client)>(1);
    let (peer_send, peer_recv) = mpsc::channel::<(SocketEvent, serde_json::Value)>(1);
    thread::spawn(move || {
        let _sc = create_socket(socket_send, peer_recv).unwrap();
    });
    tokio::task::spawn(async move {
        let _pc = create_peer_connection(peer_send, socket_recv)
            .await
            .unwrap();
    });
    loop {
        sleep(Duration::from_secs(1)).await;
    }
}

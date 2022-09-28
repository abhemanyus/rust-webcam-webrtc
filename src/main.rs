use anyhow::Result;

use rust_socketio::{ClientBuilder, Payload};
use serde::{Deserialize, Serialize};
use std::fs::File;
use std::io::BufReader;
use std::path::Path;
use std::sync::Arc;
use tokio::sync::mpsc::{Receiver, Sender};
use tokio::sync::Notify;
use tokio::time::Duration;
use webrtc::api::interceptor_registry::register_default_interceptors;
use webrtc::api::media_engine::{MediaEngine, MIME_TYPE_H264};
use webrtc::api::APIBuilder;
use webrtc::ice_transport::ice_candidate::RTCIceCandidate;
use webrtc::ice_transport::ice_connection_state::RTCIceConnectionState;
use webrtc::ice_transport::ice_credential_type::RTCIceCredentialType;
use webrtc::ice_transport::ice_server::RTCIceServer;
use webrtc::interceptor::registry::Registry;
use webrtc::media::io::h264_reader::H264Reader;
use webrtc::media::Sample;
use webrtc::peer_connection::configuration::RTCConfiguration;
use webrtc::peer_connection::peer_connection_state::RTCPeerConnectionState;
use webrtc::peer_connection::sdp::sdp_type::RTCSdpType;
use webrtc::peer_connection::sdp::session_description::RTCSessionDescription;
use webrtc::rtp_transceiver::rtp_codec::RTCRtpCodecCapability;
use webrtc::track::track_local::track_local_static_sample::TrackLocalStaticSample;
use webrtc::track::track_local::TrackLocal;
use webrtc::Error;

const VIDEO_FILE: &str = "./video.mp4";

#[derive(Deserialize, Debug, Serialize)]
enum MyPayload {
    candidate(RTCIceCandidate),
    sdp(RTCSessionDescription),
}

#[derive(Debug)]
enum SocketEvent {
    Start,
    Message(MyPayload),
}

#[derive(Debug)]
enum PeerEvent {
    Message(MyPayload),
}

async fn socket_connect(
    socket_send: Sender<SocketEvent>,
    mut peer_recv: Receiver<PeerEvent>,
) -> Result<()> {
    let message = socket_send.clone();
    let socket = ClientBuilder::new("http://localhost:3000?uin=BOB&type=MavDrone")
        .on("message", move |payload, socket| {
            if let Payload::String(payload) = payload {
                let payload: MyPayload = serde_json::from_str(&payload).unwrap();
                message
                    .blocking_send(SocketEvent::Message(payload))
                    .unwrap();
            }
        })
        .on("start", move |payload, socket| {
            socket_send.blocking_send(SocketEvent::Start).unwrap();
        })
        .connect()
        .expect("unable to connect socket");
    tokio::spawn(async move {
        while let Some(peer_event) = peer_recv.recv().await {
            match peer_event {
                PeerEvent::Message(MyPayload::candidate(candidate)) => {
                    socket.emit("message", serde_json::json!({ "candidate": candidate }))
                }
                PeerEvent::Message(MyPayload::sdp(sdp)) => {
                    socket.emit("message", serde_json::json!({ "sdp": sdp }))
                }
            }
            .unwrap();
        }
    });
    Ok(())
}

async fn webrtc_connect(
    peer_send: Sender<PeerEvent>,
    mut socket_recv: Receiver<SocketEvent>,
) -> Result<()> {
    let video_file = VIDEO_FILE.to_owned();

    if !Path::new(&video_file).exists() {
        return Err(Error::new(format!("video file: '{}' not exist", video_file)).into());
    }
    // Everything below is the WebRTC-rs API! Thanks for using it ❤️.

    // Create a MediaEngine object to configure the supported codec
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

    let peer_send_clone = peer_send.clone();
    peer_connection
        .on_ice_candidate(Box::new(move |candidate| {
            if let Some(candidate) = candidate {
                peer_send_clone
                    .blocking_send(PeerEvent::Message(MyPayload::candidate(candidate)))
                    .unwrap();
            }
            Box::pin(async {})
        }))
        .await;

    let peer_send_clone = peer_send.clone();
    let peer_connection_clone = peer_connection.clone();
    tokio::spawn(async move {
        while let Some(payload) = socket_recv.recv().await {
            match payload {
                SocketEvent::Message(MyPayload::candidate(candidate)) => {
                    let candidate = candidate.to_json().await.unwrap();
                    peer_connection_clone
                        .add_ice_candidate(candidate)
                        .await
                        .unwrap();
                }
                SocketEvent::Message(MyPayload::sdp(sdp)) => {
                    let sdp_type = sdp.sdp_type;
                    peer_connection_clone
                        .set_remote_description(sdp)
                        .await
                        .unwrap();
                    if sdp_type == RTCSdpType::Offer {
                        let answer = peer_connection_clone.create_answer(None).await.unwrap();
                        peer_connection_clone
                            .set_local_description(answer.clone())
                            .await
                            .unwrap();
                        peer_send_clone
                            .send(PeerEvent::Message(MyPayload::sdp(answer)))
                            .await
                            .unwrap();
                    }
                }
                SocketEvent::Start => println!("Start streaming!"),
            };
        }
        Result::<()>::Ok(())
    });

    let notify_tx = Arc::new(Notify::new());
    let notify_video = notify_tx.clone();

    let (done_tx, mut done_rx) = tokio::sync::mpsc::channel::<()>(1);
    let video_done_tx = done_tx.clone();

    // Create a video track
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

    let video_file_name = video_file.to_owned();
    tokio::spawn(async move {
        // Open a H264 file and start reading using our H264Reader
        let file = File::open(&video_file_name)?;
        let reader = BufReader::new(file);
        let mut h264 = H264Reader::new(reader);

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

    // Set the handler for ICE connection state
    // This will notify you when the peer has connected/disconnected
    peer_connection
        .on_ice_connection_state_change(Box::new(move |connection_state: RTCIceConnectionState| {
            println!("Connection State has changed {}", connection_state);
            if connection_state == RTCIceConnectionState::Connected {
                notify_tx.notify_waiters();
            }
            Box::pin(async {})
        }))
        .await;

    // Set the handler for Peer connection state
    // This will notify you when the peer has connected/disconnected
    peer_connection
        .on_peer_connection_state_change(Box::new(move |s: RTCPeerConnectionState| {
            println!("Peer Connection State has changed: {}", s);

            if s == RTCPeerConnectionState::Failed {
                // Wait until PeerConnection has had no network activity for 30 seconds or another failure. It may be reconnected using an ICE Restart.
                // Use webrtc.PeerConnectionStateDisconnected if you are interested in detecting faster timeout.
                // Note that the PeerConnection may come back from PeerConnectionStateDisconnected.
                println!("Peer Connection has gone to failed exiting");
                let _ = done_tx.try_send(());
            }

            Box::pin(async {})
        }))
        .await;

    println!("Press ctrl-c to stop");
    tokio::select! {
        _ = done_rx.recv() => {
            println!("received done signal!");
        }
        _ = tokio::signal::ctrl_c() => {
            println!("");
        }
    };

    peer_connection.close().await?;
    Ok(())
}

#[tokio::main]
async fn main() -> Result<()> {
    let (socket_send, socket_recv) = tokio::sync::mpsc::channel::<SocketEvent>(1);
    let (peer_send, peer_recv) = tokio::sync::mpsc::channel::<PeerEvent>(1);
    socket_connect(socket_send, peer_recv).await.unwrap();
    webrtc_connect(peer_send, socket_recv).await.unwrap();
    Ok(())
}

use std::sync::Arc;

use serde::{Deserialize, Serialize};
use tokio::{
    sync::mpsc::{Receiver, Sender},
    task::JoinHandle,
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

#[derive(Deserialize, Clone)]
pub struct IceConfig {
    urls: Vec<String>,
    username: Option<String>,
    credential: Option<String>,
}

impl Default for IceConfig {
    fn default() -> Self {
        Self {
            urls: vec!["turn:localhost:5450".to_string()],
            username: Some("test".to_string()),
            credential: Some("test".to_string()),
        }
    }
}

#[derive(Serialize, Deserialize)]
pub struct Payload {
    candidate: Option<RTCIceCandidateInit>,
    sdp: Option<RTCSessionDescription>,
}

pub struct WebRTC {
    peer_connection: Arc<RTCPeerConnection>,
}

impl WebRTC {
    pub async fn new(config: IceConfig) -> Result<Self, Error> {
        let mut m = MediaEngine::default();
        m.register_default_codecs()?;
        let mut registry = Registry::new();
        registry = register_default_interceptors(registry, &mut m)?;
        let api = APIBuilder::new()
            .with_media_engine(m)
            .with_interceptor_registry(registry)
            .build();
        let config = RTCConfiguration {
            ice_servers: vec![RTCIceServer {
                urls: config.urls,
                username: config.username.unwrap_or("test".to_owned()),
                credential: config.credential.unwrap_or("test".to_owned()),
                credential_type: RTCIceCredentialType::Password,
            }],
            ..Default::default()
        };
        let pc = api.new_peer_connection(config).await?;
        Ok(Self {
            peer_connection: Arc::new(pc),
        })
    }

    pub async fn setup_listeners(
        &self,
        mut signaling_event: Receiver<Payload>,
        socket_sender: Sender<Payload>,
    ) -> JoinHandle<()> {
        let peer_connection = self.peer_connection.clone();
        tokio::task::spawn(async move {
            while let Some(payload) = signaling_event.recv().await {
                if let Some(response) = Self::handle_holler(&peer_connection, payload).await {
                    match socket_sender.send(response).await {
                        Ok(_) => println!("send response to socket"),
                        Err(err) => println!("Unable to send response to socket: {err}"),
                    }
                };
            }
            println!("Peer stopped listening for events");
        })
    }

    async fn handle_holler(
        peer_connection: &RTCPeerConnection,
        payload: Payload,
    ) -> Option<Payload> {
        if let Some(candidate) = payload.candidate {
            match peer_connection.add_ice_candidate(candidate).await {
                Ok(_) => println!("Added ice candidate"),
                Err(err) => println!("Failed to add ice candidate: {err}"),
            }
        }
        if let Some(sdp) = payload.sdp {
            match sdp.sdp_type {
                RTCSdpType::Offer => {
                    println!("Received offer");
                    if peer_connection.signaling_state() == RTCSignalingState::Stable {
                        match peer_connection.set_remote_description(sdp).await {
                            Ok(_) => {
                                println!("Remote description set");
                                match peer_connection.create_answer(None).await {
                                    Ok(answer) => {
                                        println!("Answer created");
                                        return Some(Payload {
                                            sdp: Some(answer),
                                            candidate: None,
                                        });
                                    }
                                    Err(err) => println!("Failed to create answer: {err}"),
                                };
                            }
                            Err(err) => println!("Failed to add remote description: {err}"),
                        };
                    } else {
                        println!(
                            "Offer received in invalid state: {}",
                            peer_connection.signaling_state()
                        );
                    }
                }
                RTCSdpType::Answer => {
                    println!("Received answer");
                    match peer_connection.set_remote_description(sdp).await {
                        Ok(_) => println!("Remote description set"),
                        Err(err) => println!("Failed to set remote description: {err}"),
                    }
                }
                _ => println!("Unknown sdp type received"),
            }
        }
        None
    }

    pub async fn setup_callbacks(&self, socket_sender: Sender<Payload>) {
        let sender = socket_sender.clone();
        self.peer_connection
            .on_ice_candidate(Box::new(move |candidate| {
                let socket_sender = sender.clone();
                Box::pin(async move {
                    if let Some(candidate) = candidate {
                        if let Ok(candidate) = candidate.to_json().await {
                            match socket_sender
                                .send(Payload {
                                    candidate: Some(candidate),
                                    sdp: None,
                                })
                                .await
                            {
                                Ok(_) => println!("Sent candidate to socket"),
                                Err(err) => println!("Failed to send candidate to socket: {err}"),
                            }
                        }
                    }
                })
            }))
            .await;

        let sender = socket_sender.clone();
        let peer_connection_clone = self.peer_connection.clone();
        self.peer_connection
            .on_negotiation_needed(Box::new(move || {
                let peer_connection = peer_connection_clone.clone();
                let socket_sender = sender.clone();
                Box::pin(async move {
                    let offer = match peer_connection.create_offer(None).await {
                        Ok(offer) => offer,
                        Err(err) => {
                            println!("Failed to create offer: {err}");
                            return;
                        }
                    };
                    match peer_connection.set_local_description(offer.clone()).await {
                        Ok(_) => println!("Set local description"),
                        Err(err) => println!("Failed to set local description: {err}"),
                    }
                    match socket_sender
                        .send(Payload {
                            candidate: None,
                            sdp: Some(offer),
                        })
                        .await
                    {
                        Ok(_) => println!("Send offer to socket"),
                        Err(err) => println!("Failed to send offer to socket {err}"),
                    };
                })
            }))
            .await;
    }

    pub async fn setup_stream(
        &self,
        mut stream: Receiver<Vec<u8>>,
    ) -> Result<JoinHandle<()>, webrtc::Error> {
        let video_track = Arc::new(TrackLocalStaticRTP::new(
            RTCRtpCodecCapability {
                mime_type: MIME_TYPE_VP8.to_owned(),
                ..Default::default()
            },
            "video".to_owned(),
            "webrtc-rs".to_owned(),
        ));
        let rtp_sender = self
            .peer_connection
            .add_track(Arc::clone(&video_track) as Arc<dyn TrackLocal + Send + Sync>)
            .await?;
        let _handle = tokio::spawn(async move {
            let mut rtcp_buf = vec![0u8; 1500];
            while let Ok((_, _)) = rtp_sender.read(&mut rtcp_buf).await {}
        });
        let handle = tokio::task::spawn(async move {
            while let Some(map) = stream.recv().await {
                if let Err(err) = video_track.write(&map).await {
                    if Error::ErrClosedPipe == err {
                        println!("The peerConnection has been closed.");
                    } else {
                        println!("video_track write err: {}", err);
                    }
                }
            }
        });
        Ok(handle)
    }
}

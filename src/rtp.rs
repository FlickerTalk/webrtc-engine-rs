use std::fmt;
use std::net::ToSocketAddrs;
use std::sync::Arc;
use std::time::Instant;

use bytes::Bytes;
use rtc::media::Sample;

use rtc::media_stream::MediaStreamTrack;
use rtc::peer_connection::configuration::media_engine::MIME_TYPE_OPUS;
use rtc::rtp_transceiver::rtp_sender::{
    RTCRtpCodec, RTCRtpCodecParameters, RTCRtpCodingParameters, RTCRtpEncodingParameters,
    RtpCodecKind,
};
use rtc::rtp_transceiver::{PayloadType, SSRC};
use webrtc::media_stream::track_local::static_sample::TrackLocalStaticSample;
use webrtc::media_stream::track_remote::{TrackRemote, TrackRemoteEvent};
use webrtc::peer_connection::{
    MediaEngine, PeerConnection, PeerConnectionBuilder, Registry, configure_rtcp_reports,
};
use webrtc::rtp_transceiver::RtpSender;

use crate::FRAME_DURATION;

#[derive(Debug)]
pub enum RtpError {
    WebRtc(webrtc::error::Error),
    OpusNotNegotiated,
}

impl fmt::Display for RtpError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::WebRtc(error) => write!(f, "webrtc: {error}"),
            Self::OpusNotNegotiated => f.write_str("opus is not negotiated on this connection"),
        }
    }
}

impl std::error::Error for RtpError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::WebRtc(error) => Some(error),
            Self::OpusNotNegotiated => None,
        }
    }
}

impl From<webrtc::error::Error> for RtpError {
    fn from(error: webrtc::error::Error) -> Self {
        Self::WebRtc(error)
    }
}

/// Two channels although calls are mono. RFC 7587 fixes the SDP at two and browsers reject
/// anything else.
pub fn opus_codec() -> RTCRtpCodec {
    RTCRtpCodec {
        mime_type: MIME_TYPE_OPUS.to_owned(),
        clock_rate: 48_000,
        channels: 2,
        sdp_fmtp_line: "minptime=10;useinbandfec=1".to_owned(),
        rtcp_feedback: vec![],
    }
}

/// What Chrome and Safari use. An answer takes the offerer's number instead.
pub const OPUS_PAYLOAD_TYPE: PayloadType = 111;

pub fn media_engine() -> Result<MediaEngine, RtpError> {
    let mut engine = MediaEngine::default();
    engine.register_codec(
        RTCRtpCodecParameters {
            rtp_codec: opus_codec(),
            payload_type: OPUS_PAYLOAD_TYPE,
        },
        RtpCodecKind::Audio,
    )?;
    crate::video::rtp::register_h264(&mut engine)?;
    Ok(engine)
}

/// The caller still adds the handler, the runtime, the ICE configuration and the addresses.
pub fn peer_connection_builder<A: ToSocketAddrs>() -> Result<PeerConnectionBuilder<A>, RtpError> {
    let mut engine = media_engine()?;
    let registry = configure_rtcp_reports(Registry::new());
    let registry = crate::video::rtp::configure_video(registry, &mut engine)?;
    Ok(PeerConnectionBuilder::new()
        .with_media_engine(engine)
        .with_interceptor_registry(registry))
}

// Not random on purpose. Only used inside a 1:1 call, and the SDP travels encrypted.
const STREAM_ID: &str = "call";
const TRACK_ID: &str = "voice";

pub struct AudioSender {
    track: Arc<TrackLocalStaticSample>,
    sender: Arc<dyn RtpSender>,
    ssrc: SSRC,
}

impl AudioSender {
    /// An empty packet sends nothing but still takes its 20 ms.
    /// Fails until audio is negotiated, and after the connection closes.
    pub async fn send(&self, packet: &[u8]) -> Result<(), RtpError> {
        let sample = Sample {
            data: Bytes::copy_from_slice(packet),
            duration: FRAME_DURATION,
            ..Sample::new(Instant::now())
        };
        let payload_type = self.payload_type().await?;
        self.track
            .write_sample(self.ssrc, payload_type, &sample, &[])
            .await?;
        Ok(())
    }

    /// Not cached since a renegotiation can change it. Reading it is only a message to the
    /// connection's task.
    async fn payload_type(&self) -> Result<PayloadType, RtpError> {
        let parameters = self.sender.get_parameters().await?;
        parameters
            .rtp_parameters
            .codecs
            .iter()
            .find(|codec| {
                codec
                    .rtp_codec
                    .mime_type
                    .eq_ignore_ascii_case(MIME_TYPE_OPUS)
            })
            .map(|codec| codec.payload_type)
            .ok_or(RtpError::OpusNotNegotiated)
    }

    pub fn rtp_sender(&self) -> &Arc<dyn RtpSender> {
        &self.sender
    }
}

/// Call before the offer or answer is created, so the SDP announces our audio.
pub async fn add_audio_track(connection: &dyn PeerConnection) -> Result<AudioSender, RtpError> {
    let ssrc: SSRC = rand::random();
    let track = MediaStreamTrack::new(
        STREAM_ID.to_owned(),
        TRACK_ID.to_owned(),
        TRACK_ID.to_owned(),
        RtpCodecKind::Audio,
        vec![RTCRtpEncodingParameters {
            rtp_coding_parameters: RTCRtpCodingParameters {
                ssrc: Some(ssrc),
                ..Default::default()
            },
            codec: opus_codec(),
            ..Default::default()
        }],
    );
    let track = Arc::new(TrackLocalStaticSample::new(Instant::now(), track)?);
    let sender = connection.add_track(track.clone()).await?;
    Ok(AudioSender {
        track,
        sender,
        ssrc,
    })
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AudioPacket {
    pub sequence: u16,
    /// 48 kHz clock.
    pub timestamp: u32,
    pub payload: Vec<u8>,
}

pub struct AudioReceiver {
    track: Arc<dyn TrackRemote>,
}

impl AudioReceiver {
    /// One reader per track. Each packet goes to a single `recv`.
    pub fn new(track: Arc<dyn TrackRemote>) -> Self {
        Self { track }
    }

    pub async fn recv(&self) -> Result<Option<(u16, Vec<u8>)>, RtpError> {
        Ok(self
            .recv_packet()
            .await?
            .map(|packet| (packet.sequence, packet.payload)))
    }

    pub async fn recv_packet(&self) -> Result<Option<AudioPacket>, RtpError> {
        while let Some(event) = self.track.poll().await {
            match event {
                TrackRemoteEvent::OnRtpPacket(packet) => {
                    return Ok(Some(AudioPacket {
                        sequence: packet.header.sequence_number,
                        timestamp: packet.header.timestamp,
                        payload: packet.payload.to_vec(),
                    }));
                }
                TrackRemoteEvent::OnEnded => return Ok(None),
                _ => {}
            }
        }
        Ok(None)
    }
}

#[cfg(test)]
mod tests {
    use webrtc::peer_connection::PeerConnectionEventHandler;

    use super::*;

    #[test]
    fn opus_is_announced_as_browsers_expect() {
        let codec = opus_codec();
        assert_eq!(codec.mime_type, MIME_TYPE_OPUS);
        assert_eq!(codec.clock_rate, 48_000);
        assert_eq!(codec.channels, 2);
        assert_eq!(codec.sdp_fmtp_line, "minptime=10;useinbandfec=1");
    }

    struct Quiet;
    impl PeerConnectionEventHandler for Quiet {}

    #[tokio::test(flavor = "multi_thread")]
    async fn an_audio_offer_carries_opus_with_its_fmtp() {
        let connection = peer_connection_builder()
            .expect("the builder is ready")
            .with_handler(Arc::new(Quiet))
            .with_udp_addrs(vec!["127.0.0.1:0".to_owned()])
            .build()
            .await
            .expect("the connection is built");
        connection
            .add_transceiver_from_kind(RtpCodecKind::Audio, None)
            .await
            .expect("an audio transceiver is added");

        let offer = connection
            .create_offer(None)
            .await
            .expect("an offer is created");

        assert!(
            offer.sdp.contains("m=audio "),
            "no audio section:\n{}",
            offer.sdp
        );
        assert!(
            offer.sdp.contains("a=rtpmap:111 opus/48000/2\r\n"),
            "no opus/48000/2:\n{}",
            offer.sdp
        );
        assert!(
            offer
                .sdp
                .contains("a=fmtp:111 minptime=10;useinbandfec=1\r\n"),
            "no Opus fmtp:\n{}",
            offer.sdp
        );
        connection.close().await.expect("the connection closes");
    }
}

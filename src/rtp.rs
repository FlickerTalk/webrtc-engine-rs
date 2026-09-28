//! The glue between Opus packets and webrtc-rs tracks: one 20 ms Opus packet is one RTP payload.
//!
//! The payload bytes are opaque here: encoding and decoding live in `codec`.

use std::fmt;
use std::net::ToSocketAddrs;
use std::sync::Arc;
use std::time::{Duration, Instant};

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

/// What can go wrong between Opus packets and the tracks.
#[derive(Debug)]
pub enum RtpError {
    /// webrtc-rs refused an operation.
    WebRtc(webrtc::error::Error),
    /// The connection has no Opus to send with: not negotiated yet, or the other side refused it.
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

/// The Opus codec as browsers announce it: `opus/48000/2`, with in-band FEC.
///
/// Two channels even though calls send mono: RFC 7587 fixes the SDP to two channels, and a
/// mono Opus stream is valid on it. Browsers reject an Opus line that says otherwise.
pub fn opus_codec() -> RTCRtpCodec {
    RTCRtpCodec {
        mime_type: MIME_TYPE_OPUS.to_owned(),
        clock_rate: 48_000,
        channels: 2,
        sdp_fmtp_line: "minptime=10;useinbandfec=1".to_owned(),
        rtcp_feedback: vec![],
    }
}

/// The payload type Chrome and Safari give Opus. Only what we offer: when we answer, the
/// offerer's number wins, so senders read the negotiated one instead of assuming this.
pub const OPUS_PAYLOAD_TYPE: PayloadType = 111;

/// A media engine that knows Opus and nothing else.
pub fn media_engine() -> Result<MediaEngine, RtpError> {
    let mut engine = MediaEngine::default();
    engine.register_codec(
        RTCRtpCodecParameters {
            rtp_codec: opus_codec(),
            payload_type: OPUS_PAYLOAD_TYPE,
        },
        RtpCodecKind::Audio,
    )?;
    Ok(engine)
}

/// A peer connection builder ready for audio calls. The caller adds the handler, the runtime,
/// the ICE configuration and the addresses to bind.
///
/// It sends and answers RTCP reports: the other side's Opus encoder turns its in-band FEC on
/// from the loss our receiver reports announce.
pub fn peer_connection_builder<A: ToSocketAddrs>() -> Result<PeerConnectionBuilder<A>, RtpError> {
    Ok(PeerConnectionBuilder::new()
        .with_media_engine(media_engine()?)
        .with_interceptor_registry(configure_rtcp_reports(Registry::new())))
}

/// One Opus packet carries 20 ms of audio: 960 samples at 48 kHz.
pub const FRAME_DURATION: Duration = Duration::from_millis(20);

// Fixed, not random: they only name the track inside a one-to-one call, and the description
// that carries them travels encrypted.
const STREAM_ID: &str = "call";
const TRACK_ID: &str = "voice";

/// Sends our voice: one 20 ms Opus packet per call to [`AudioSender::send`].
pub struct AudioSender {
    track: Arc<TrackLocalStaticSample>,
    sender: Arc<dyn RtpSender>,
    ssrc: SSRC,
}

impl AudioSender {
    /// Sends one 20 ms Opus packet. The track numbers it and stamps its time: 960 samples later
    /// than the previous one. An empty packet sends nothing but still takes its 20 ms.
    ///
    /// Fails until the connection has negotiated audio, and once it has closed.
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

    /// The payload type Opus was given in this negotiation. Read on every packet, not cached:
    /// the offerer chooses it, so a renegotiation may change it, and the read is a message to
    /// the connection's own task, not network I/O.
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

    /// The RTP sender, for statistics or to replace or remove the track.
    pub fn rtp_sender(&self) -> &Arc<dyn RtpSender> {
        &self.sender
    }
}

/// Adds our audio track to `connection`. Call it before creating the offer or the answer, so
/// the description announces that we send audio.
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

/// One received Opus packet, as it came in its RTP packet.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AudioPacket {
    /// RTP sequence number: consecutive packets differ by one, wrapping at `u16::MAX`.
    pub sequence: u16,
    /// RTP timestamp at 48 kHz: 960 more for every 20 ms of audio, gaps included.
    pub timestamp: u32,
    /// The Opus packet.
    pub payload: Vec<u8>,
}

/// Receives the other side's voice from the remote track that `on_track` hands over.
pub struct AudioReceiver {
    track: Arc<dyn TrackRemote>,
}

impl AudioReceiver {
    /// Reads `track`. Only one reader per track: every packet goes to a single `recv`.
    pub fn new(track: Arc<dyn TrackRemote>) -> Self {
        Self { track }
    }

    /// The next packet's sequence number and Opus payload, what the jitter buffer takes.
    /// `None` once the track has ended.
    pub async fn recv(&self) -> Result<Option<(u16, Vec<u8>)>, RtpError> {
        Ok(self
            .recv_packet()
            .await?
            .map(|packet| (packet.sequence, packet.payload)))
    }

    /// The next packet with its RTP timestamp too. `None` once the track has ended.
    ///
    /// Packets come as the network delivers them: lost, late or out of order. Putting them back
    /// in order is the jitter buffer's job.
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

    // WebRTC always announces Opus as `opus/48000/2`, even for mono (RFC 7587).
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

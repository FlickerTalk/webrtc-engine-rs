//! The glue between Opus packets and webrtc-rs tracks: one 20 ms Opus packet is one RTP payload.
//!
//! The payload bytes are opaque here: encoding and decoding live in `codec`.

use std::fmt;
use std::net::ToSocketAddrs;

use rtc::peer_connection::configuration::media_engine::MIME_TYPE_OPUS;
use rtc::rtp_transceiver::PayloadType;
use rtc::rtp_transceiver::rtp_sender::{RTCRtpCodec, RTCRtpCodecParameters, RtpCodecKind};
use webrtc::peer_connection::{
    MediaEngine, PeerConnectionBuilder, Registry, configure_rtcp_reports,
};

/// What can go wrong between Opus packets and the tracks.
#[derive(Debug)]
pub enum RtpError {
    /// webrtc-rs refused an operation.
    WebRtc(webrtc::error::Error),
}

impl fmt::Display for RtpError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::WebRtc(error) => write!(f, "webrtc: {error}"),
        }
    }
}

impl std::error::Error for RtpError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::WebRtc(error) => Some(error),
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

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use webrtc::peer_connection::{PeerConnection, PeerConnectionEventHandler};

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

//! The glue between Opus packets and webrtc-rs tracks: one 20 ms Opus packet is one RTP payload.
//!
//! The payload bytes are opaque here: encoding and decoding live in `codec`.

use rtc::peer_connection::configuration::media_engine::MIME_TYPE_OPUS;
use rtc::rtp_transceiver::rtp_sender::RTCRtpCodec;

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

#[cfg(test)]
mod tests {
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
}

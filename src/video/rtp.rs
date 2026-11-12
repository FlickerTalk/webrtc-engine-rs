use std::collections::{HashSet, VecDeque};
use std::fmt;
use std::sync::{Arc, Mutex, PoisonError};
use std::time::{Duration, Instant, SystemTime};

use bytes::Bytes;
use rtc::interceptor::{
    Attribute, AttributedPacket, Interceptor, Packet, Registry, Slot, StreamInfo, TaggedPacket,
};
use rtc::media_stream::MediaStreamTrack;
use rtc::peer_connection::configuration::interceptor_registry::{
    configure_nack, configure_twcc_receiver_only,
};
use rtc::peer_connection::configuration::media_engine::{MIME_TYPE_H264, MediaEngine};
use rtc::rtcp;
use rtc::rtcp::payload_feedbacks::full_intra_request::FullIntraRequest;
use rtc::rtcp::payload_feedbacks::picture_loss_indication::PictureLossIndication;
use rtc::rtcp::payload_feedbacks::receiver_estimated_maximum_bitrate::ReceiverEstimatedMaximumBitrate;
use rtc::rtcp::receiver_report::ReceiverReport;
use rtc::rtcp::sender_report::SenderReport;
use rtc::rtp;
use rtc::rtp_transceiver::rtp_sender::{
    RTCPFeedback, RTCRtpCodec, RTCRtpCodecParameters, RTCRtpCodingParameters,
    RTCRtpEncodingParameters, RTCRtpHeaderExtensionCapability, RtpCodecKind,
};
use rtc::rtp_transceiver::{PayloadType, SSRC};
use tokio::sync::oneshot;
use webrtc::media_stream::track_local::static_rtp::TrackLocalStaticRTP;
use webrtc::media_stream::track_local::{TrackLocal, TrackLocalEvent};
use webrtc::media_stream::track_remote::{TrackRemote, TrackRemoteEvent};
use webrtc::peer_connection::PeerConnection;
use webrtc::rtp_transceiver::RtpSender;

use super::call::{Feedback, FeedbackSource, FrameSink, RemoteVideo, VideoPacketSource};
use super::packet::{Packetizer, VIDEO_CLOCK_RATE, VideoPacket};
use super::{CVO_URI, EncodedFrame, Rotation};

#[derive(Debug)]
pub enum VideoRtpError {
    WebRtc(webrtc::error::Error),
    H264NotNegotiated,
}

impl fmt::Display for VideoRtpError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::WebRtc(error) => write!(f, "webrtc: {error}"),
            Self::H264NotNegotiated => f.write_str("h264 is not negotiated on this connection"),
        }
    }
}

impl std::error::Error for VideoRtpError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::WebRtc(error) => Some(error),
            Self::H264NotNegotiated => None,
        }
    }
}

impl From<webrtc::error::Error> for VideoRtpError {
    fn from(error: webrtc::error::Error) -> Self {
        Self::WebRtc(error)
    }
}

pub const H264_FMTP: &str =
    "level-asymmetry-allowed=1;packetization-mode=1;profile-level-id=42e01f";

/// Only what we offer. When we answer, the offerer's number wins.
pub const H264_PAYLOAD_TYPE: PayloadType = 102;

pub fn h264_codec() -> RTCRtpCodec {
    let feedback = |typ: &str, parameter: &str| RTCPFeedback {
        typ: typ.to_owned(),
        parameter: parameter.to_owned(),
    };
    RTCRtpCodec {
        mime_type: MIME_TYPE_H264.to_owned(),
        clock_rate: VIDEO_CLOCK_RATE,
        channels: 0,
        sdp_fmtp_line: H264_FMTP.to_owned(),
        rtcp_feedback: vec![
            feedback("goog-remb", ""),
            feedback("transport-cc", ""),
            feedback("ccm", "fir"),
            feedback("nack", ""),
            feedback("nack", "pli"),
        ],
    }
}

pub fn register_h264(engine: &mut MediaEngine) -> Result<(), webrtc::error::Error> {
    engine.register_codec(
        RTCRtpCodecParameters {
            rtp_codec: h264_codec(),
            payload_type: H264_PAYLOAD_TYPE,
        },
        RtpCodecKind::Video,
    )?;
    engine.register_header_extension(
        RTCRtpHeaderExtensionCapability {
            uri: CVO_URI.to_owned(),
        },
        RtpCodecKind::Video,
        None,
    )
}

pub fn configure_video(
    registry: Registry,
    engine: &mut MediaEngine,
) -> Result<Registry, webrtc::error::Error> {
    let registry = configure_nack(registry, engine);
    let registry = configure_twcc_receiver_only(registry, engine)?;
    Ok(registry.with(
        Slot::from(FEEDBACK_SLOT),
        VideoFeedbackInterceptor::default(),
    ))
}

/// After every built-in interceptor, so they all see the inbound RTCP before it picks from it.
pub const FEEDBACK_SLOT: usize = 14_000;

/// webrtc-rs keeps inbound RTCP for its interceptors, so this one hands the video feedback out.
#[derive(Default)]
pub struct VideoFeedbackInterceptor {
    video: HashSet<SSRC>,
    read_queue: VecDeque<TaggedPacket>,
    write_queue: VecDeque<TaggedPacket>,
}

/// One packet per stream, because webrtc-rs routes RTCP to a track by the first SSRC it names.
/// A receiver report whose first block is about our audio would never reach the video track.
pub fn video_feedback(
    packets: &[Box<dyn rtcp::Packet>],
    video: &HashSet<SSRC>,
) -> Vec<Box<dyn rtcp::Packet>> {
    let ours = |ssrc: &SSRC| video.contains(ssrc);
    let mut picked: Vec<Box<dyn rtcp::Packet>> = Vec::new();
    for packet in packets {
        let packet = packet.as_any();
        let (sender, blocks) = if let Some(report) = packet.downcast_ref::<ReceiverReport>() {
            (report.ssrc, report.reports.as_slice())
        } else if let Some(report) = packet.downcast_ref::<SenderReport>() {
            (report.ssrc, report.reports.as_slice())
        } else {
            (0, &[][..])
        };
        for block in blocks.iter().filter(|block| ours(&block.ssrc)) {
            picked.push(Box::new(ReceiverReport {
                ssrc: sender,
                reports: vec![block.clone()],
                ..Default::default()
            }));
        }
        if let Some(pli) = packet.downcast_ref::<PictureLossIndication>()
            && ours(&pli.media_ssrc)
        {
            picked.push(Box::new(pli.clone()));
        }
        if let Some(fir) = packet.downcast_ref::<FullIntraRequest>() {
            for entry in fir.fir.iter().filter(|entry| ours(&entry.ssrc)) {
                picked.push(Box::new(FullIntraRequest {
                    sender_ssrc: fir.sender_ssrc,
                    media_ssrc: fir.media_ssrc,
                    fir: vec![entry.clone()],
                }));
            }
        }
        if let Some(remb) = packet.downcast_ref::<ReceiverEstimatedMaximumBitrate>() {
            for &ssrc in remb.ssrcs.iter().filter(|ssrc| ours(ssrc)) {
                picked.push(Box::new(ReceiverEstimatedMaximumBitrate {
                    sender_ssrc: remb.sender_ssrc,
                    bitrate: remb.bitrate,
                    ssrcs: vec![ssrc],
                }));
            }
        }
    }
    picked
}

impl rtc::sansio::Protocol<TaggedPacket, TaggedPacket, ()> for VideoFeedbackInterceptor {
    type Rout = TaggedPacket;
    type Wout = TaggedPacket;
    type Eout = ();
    type Error = rtc::shared::error::Error;
    type Time = Instant;

    fn handle_read(&mut self, msg: TaggedPacket) -> Result<(), Self::Error> {
        let picked = match &msg.message.packet {
            Packet::Rtcp(packets) if !self.video.is_empty() => video_feedback(packets, &self.video),
            _ => Vec::new(),
        };
        let (now, transport) = (msg.now, msg.transport);
        self.read_queue.push_back(msg);
        for packet in picked {
            self.read_queue.push_back(TaggedPacket {
                now,
                transport,
                message: AttributedPacket::new(Packet::Rtcp(vec![packet]))
                    .with(Attribute::DeliverToApplication),
            });
        }
        Ok(())
    }

    fn poll_read(&mut self) -> Option<TaggedPacket> {
        self.read_queue.pop_front()
    }

    fn handle_write(&mut self, msg: TaggedPacket) -> Result<(), Self::Error> {
        self.write_queue.push_back(msg);
        Ok(())
    }

    fn poll_write(&mut self) -> Option<TaggedPacket> {
        self.write_queue.pop_front()
    }
}

impl Interceptor for VideoFeedbackInterceptor {
    fn bind_local_stream(&mut self, info: &StreamInfo) {
        if info.mime_type.to_ascii_lowercase().starts_with("video/") {
            self.video.insert(info.ssrc);
        }
    }
    fn unbind_local_stream(&mut self, info: &StreamInfo) {
        self.video.remove(&info.ssrc);
    }
    fn bind_remote_stream(&mut self, _info: &StreamInfo) {}
    fn unbind_remote_stream(&mut self, _info: &StreamInfo) {}
}

/// RFC 3550 section 6.4.1, in 1/65536 s. `None` before any sender report from us.
pub fn round_trip(
    arrival_ntp_middle: u32,
    last_sender_report: u32,
    delay: u32,
) -> Option<Duration> {
    if last_sender_report == 0 {
        return None;
    }
    let ticks = arrival_ntp_middle
        .wrapping_sub(last_sender_report)
        .wrapping_sub(delay);
    // Beyond half the 32-bit range it is a negative time, not a 9-hour round trip.
    (i32::try_from(ticks).is_ok())
        .then(|| Duration::from_micros(u64::from(ticks) * 1_000_000 / 65_536))
}

pub fn feedback_from_rtcp(
    packets: &[Box<dyn rtcp::Packet>],
    ssrc: SSRC,
    arrival_ntp_middle: u32,
) -> Vec<Feedback> {
    let mut feedback = Vec::new();
    for packet in packets {
        let packet = packet.as_any();
        let blocks = if let Some(report) = packet.downcast_ref::<ReceiverReport>() {
            report.reports.as_slice()
        } else if let Some(report) = packet.downcast_ref::<SenderReport>() {
            report.reports.as_slice()
        } else {
            &[]
        };
        feedback.extend(
            blocks
                .iter()
                .filter(|block| block.ssrc == ssrc)
                .map(|block| Feedback::Report {
                    fraction_lost: f64::from(block.fraction_lost) / 256.0,
                    round_trip: round_trip(
                        arrival_ntp_middle,
                        block.last_sender_report,
                        block.delay,
                    ),
                }),
        );
        if packet
            .downcast_ref::<PictureLossIndication>()
            .is_some_and(|pli| pli.media_ssrc == ssrc)
            || packet
                .downcast_ref::<FullIntraRequest>()
                .is_some_and(|fir| fir.fir.iter().any(|entry| entry.ssrc == ssrc))
        {
            feedback.push(Feedback::KeyframeRequest);
        }
        if let Some(remb) = packet.downcast_ref::<ReceiverEstimatedMaximumBitrate>()
            && remb.ssrcs.contains(&ssrc)
        {
            feedback.push(Feedback::Remb {
                bps: remb.bitrate as u64,
            });
        }
    }
    feedback
}

fn fmtp_parameter<'a>(fmtp: &'a str, key: &str) -> Option<&'a str> {
    fmtp.split(';')
        .filter_map(|pair| pair.split_once('='))
        .find(|(name, _)| name.trim().eq_ignore_ascii_case(key))
        .map(|(_, value)| value.trim())
}

/// Falls back to any profile in mode 1, since a Constrained Baseline stream is valid Baseline,
/// Main and High too.
pub fn h264_payload_type(codecs: &[RTCRtpCodecParameters]) -> Option<PayloadType> {
    let mode_1: Vec<&RTCRtpCodecParameters> = codecs
        .iter()
        .filter(|codec| {
            codec
                .rtp_codec
                .mime_type
                .eq_ignore_ascii_case(MIME_TYPE_H264)
        })
        .filter(|codec| {
            fmtp_parameter(&codec.rtp_codec.sdp_fmtp_line, "packetization-mode") == Some("1")
        })
        .collect();
    let constrained_baseline = |codec: &&&RTCRtpCodecParameters| {
        fmtp_parameter(&codec.rtp_codec.sdp_fmtp_line, "profile-level-id")
            .is_some_and(|profile| profile.to_ascii_lowercase().starts_with("42e0"))
    };
    mode_1
        .iter()
        .find(constrained_baseline)
        .or(mode_1.first())
        .map(|codec| codec.payload_type)
}

async fn cvo_id_of(sender: &Arc<dyn RtpSender>) -> Option<u8> {
    let parameters = sender.get_parameters().await.ok()?;
    parameters
        .rtp_parameters
        .header_extensions
        .iter()
        .find(|extension| extension.uri == CVO_URI)
        .and_then(|extension| u8::try_from(extension.id).ok())
}

// Fixed, not random. They only name the track inside a 1:1 call and the SDP travels encrypted.
const STREAM_ID: &str = "call";
const TRACK_ID: &str = "camera";

pub struct VideoSender {
    track: Arc<TrackLocalStaticRTP>,
    sender: Arc<dyn RtpSender>,
    ssrc: SSRC,
    packetizer: Mutex<Packetizer>,
}

impl VideoSender {
    /// Fails until H.264 is negotiated, and after the connection closes.
    pub async fn send(&self, frame: &EncodedFrame) -> Result<(), VideoRtpError> {
        // Not cached because a renegotiation may change the numbers. The read is a message to the
        // connection's task, not network I/O.
        let parameters = self.sender.get_parameters().await?;
        let payload_type = h264_payload_type(&parameters.rtp_parameters.codecs)
            .ok_or(VideoRtpError::H264NotNegotiated)?;
        let cvo_id = parameters
            .rtp_parameters
            .header_extensions
            .iter()
            .find(|extension| extension.uri == CVO_URI)
            .and_then(|extension| u8::try_from(extension.id).ok());
        let packets = lock(&self.packetizer).packetize(frame);
        for packet in packets {
            let mut header = rtp::header::Header {
                version: 2,
                marker: packet.marker,
                payload_type,
                sequence_number: packet.sequence,
                timestamp: packet.timestamp,
                ssrc: self.ssrc,
                ..Default::default()
            };
            if let (Some(rotation), Some(id)) = (packet.rotation, cvo_id) {
                // Only an id outside 1..=14 fails and webrtc-rs never negotiates one.
                let _ = header.set_extension(id, Bytes::copy_from_slice(&[rotation.to_cvo()]));
            }
            self.track
                .write_rtp(rtp::Packet {
                    header,
                    payload: Bytes::from(packet.payload),
                })
                .await?;
        }
        Ok(())
    }

    pub async fn cvo_extension_id(&self) -> Option<u8> {
        cvo_id_of(&self.sender).await
    }

    /// Read it from one place only.
    pub fn feedback(&self) -> VideoFeedback {
        VideoFeedback {
            track: self.track.clone(),
            ssrc: self.ssrc,
            ready: VecDeque::new(),
        }
    }

    pub fn ssrc(&self) -> SSRC {
        self.ssrc
    }

    pub fn rtp_sender(&self) -> &Arc<dyn RtpSender> {
        &self.sender
    }
}

impl FrameSink for VideoSender {
    type Error = VideoRtpError;
    fn send(
        &mut self,
        frame: &EncodedFrame,
    ) -> impl std::future::Future<Output = Result<(), VideoRtpError>> + Send {
        VideoSender::send(self, frame)
    }
}

/// Call it before creating the offer or the answer, or the SDP won't announce our video.
pub async fn add_video_track(
    connection: &dyn PeerConnection,
) -> Result<VideoSender, VideoRtpError> {
    let ssrc: SSRC = rand::random();
    let track = MediaStreamTrack::new(
        STREAM_ID.to_owned(),
        TRACK_ID.to_owned(),
        TRACK_ID.to_owned(),
        RtpCodecKind::Video,
        vec![RTCRtpEncodingParameters {
            rtp_coding_parameters: RTCRtpCodingParameters {
                ssrc: Some(ssrc),
                ..Default::default()
            },
            codec: h264_codec(),
            ..Default::default()
        }],
    );
    let track = Arc::new(TrackLocalStaticRTP::new(track));
    let sender = connection.add_track(track.clone()).await?;
    Ok(VideoSender {
        track,
        sender,
        ssrc,
        packetizer: Mutex::new(Packetizer::new(rand::random(), rand::random())),
    })
}

/// Polled: the track is unbound before negotiation, and a wait holds a lock that rebinding needs.
const FEEDBACK_POLL: Duration = Duration::from_millis(200);

/// Never ends on its own.
pub struct VideoFeedback {
    track: Arc<TrackLocalStaticRTP>,
    ssrc: SSRC,
    ready: VecDeque<Feedback>,
}

impl FeedbackSource for VideoFeedback {
    async fn recv(&mut self) -> Option<Feedback> {
        loop {
            if let Some(feedback) = self.ready.pop_front() {
                return Some(feedback);
            }
            match tokio::time::timeout(FEEDBACK_POLL, self.track.poll()).await {
                Ok(Some(TrackLocalEvent::OnRtcpPacket(packets))) => {
                    let arrival = ntp_middle_now();
                    self.ready
                        .extend(feedback_from_rtcp(&packets, self.ssrc, arrival));
                }
                Ok(Some(_)) | Err(_) => {}
                Ok(None) => tokio::time::sleep(FEEDBACK_POLL).await,
            }
        }
    }
}

/// Same wall clock as the NTP time in the sender reports webrtc-rs sends.
fn ntp_middle_now() -> u32 {
    const NTP_TO_UNIX_SECONDS: u64 = 2_208_988_800;
    let since_unix = SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .unwrap_or_default();
    let seconds = since_unix.as_secs() + NTP_TO_UNIX_SECONDS;
    let fraction = (u64::from(since_unix.subsec_nanos()) << 32) / 1_000_000_000;
    (((seconds & 0xFFFF) << 16) | (fraction >> 16)) as u32
}

pub type VideoReceiver = RemoteVideo<TrackVideoPackets>;

impl RemoteVideo<TrackVideoPackets> {
    pub fn from_track(track: Arc<dyn TrackRemote>, cvo_id: Option<u8>) -> Self {
        RemoteVideo::new(TrackVideoPackets {
            track: Some(track),
            pending: None,
            sender: None,
            cvo_id,
            media_ssrc: None,
            sender_ssrc: 0,
        })
    }

    /// `ours` must be the sender on the same transceiver. The CVO id and our SSRC come from it.
    pub fn pending(track: oneshot::Receiver<Arc<dyn TrackRemote>>, ours: &VideoSender) -> Self {
        RemoteVideo::new(TrackVideoPackets {
            track: None,
            pending: Some(track),
            sender: Some(ours.sender.clone()),
            cvo_id: None,
            media_ssrc: None,
            sender_ssrc: ours.ssrc,
        })
    }
}

pub struct TrackVideoPackets {
    track: Option<Arc<dyn TrackRemote>>,
    pending: Option<oneshot::Receiver<Arc<dyn TrackRemote>>>,
    sender: Option<Arc<dyn RtpSender>>,
    cvo_id: Option<u8>,
    media_ssrc: Option<SSRC>,
    sender_ssrc: SSRC,
}

impl TrackVideoPackets {
    async fn track(&mut self) -> Option<Arc<dyn TrackRemote>> {
        if self.track.is_none() {
            let pending = self.pending.as_mut()?;
            let arrived = pending.await;
            self.pending = None;
            self.track = Some(arrived.ok()?);
        }
        if let Some(sender) = self.sender.clone() {
            self.cvo_id = cvo_id_of(&sender).await;
            self.sender = None;
        }
        self.track.clone()
    }
}

impl VideoPacketSource for TrackVideoPackets {
    type Error = VideoRtpError;

    async fn recv(&mut self) -> Result<Option<VideoPacket>, VideoRtpError> {
        let Some(track) = self.track().await else {
            return Ok(None);
        };
        while let Some(event) = track.poll().await {
            match event {
                TrackRemoteEvent::OnRtpPacket(packet) => {
                    self.media_ssrc = Some(packet.header.ssrc);
                    return Ok(Some(video_packet(packet, self.cvo_id)));
                }
                TrackRemoteEvent::OnEnded => return Ok(None),
                _ => {}
            }
        }
        Ok(None)
    }

    async fn request_keyframe(&mut self) -> Result<(), VideoRtpError> {
        let Some(track) = self.track.clone() else {
            return Ok(());
        };
        let media_ssrc = match self.media_ssrc {
            Some(ssrc) => ssrc,
            None => match track.ssrcs().await.first() {
                Some(&ssrc) => ssrc,
                None => return Ok(()),
            },
        };
        track
            .write_rtcp(vec![Box::new(PictureLossIndication {
                sender_ssrc: self.sender_ssrc,
                media_ssrc,
            })])
            .await?;
        Ok(())
    }
}

fn video_packet(packet: rtp::Packet, cvo_id: Option<u8>) -> VideoPacket {
    let rotation = cvo_id
        .and_then(|id| packet.header.get_extension(id))
        .and_then(|payload| payload.first().copied())
        .map(Rotation::from_cvo);
    VideoPacket {
        sequence: packet.header.sequence_number,
        timestamp: packet.header.timestamp,
        marker: packet.header.marker,
        payload: packet.payload.to_vec(),
        rotation,
    }
}

fn lock<T>(mutex: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

#[cfg(test)]
mod tests {
    use super::*;
    use rtc::rtcp::payload_feedbacks::full_intra_request::FirEntry;
    use rtc::rtcp::reception_report::ReceptionReport;
    use rtc::sansio::Protocol;
    use rtc::shared::TransportContext;
    use webrtc::peer_connection::PeerConnectionEventHandler;

    const VIDEO: SSRC = 0x1111;
    const AUDIO: SSRC = 0x2222;

    #[test]
    fn h264_is_announced_as_browsers_expect() {
        let codec = h264_codec();
        assert_eq!(codec.mime_type, MIME_TYPE_H264);
        assert_eq!(codec.clock_rate, 90_000);
        assert_eq!(codec.channels, 0);
        assert_eq!(codec.sdp_fmtp_line, H264_FMTP);
        let feedback: Vec<(&str, &str)> = codec
            .rtcp_feedback
            .iter()
            .map(|feedback| (feedback.typ.as_str(), feedback.parameter.as_str()))
            .collect();
        for expected in [
            ("nack", ""),
            ("nack", "pli"),
            ("ccm", "fir"),
            ("goog-remb", ""),
            ("transport-cc", ""),
        ] {
            assert!(feedback.contains(&expected), "{expected:?} in {feedback:?}");
        }
    }

    struct Quiet;
    impl PeerConnectionEventHandler for Quiet {}

    #[tokio::test(flavor = "multi_thread")]
    async fn an_offer_carries_h264_with_its_feedback_and_cvo_next_to_opus() {
        let connection = crate::rtp::peer_connection_builder()
            .expect("the builder is ready")
            .with_handler(Arc::new(Quiet))
            .with_udp_addrs(vec!["127.0.0.1:0".to_owned()])
            .build()
            .await
            .expect("the connection is built");
        crate::rtp::add_audio_track(&connection)
            .await
            .expect("an audio track");
        add_video_track(&connection).await.expect("a video track");

        let offer = connection.create_offer(None).await.expect("an offer");
        let sdp = offer.sdp;
        let pt = H264_PAYLOAD_TYPE;
        for line in [
            "m=audio ".to_owned(),
            "a=rtpmap:111 opus/48000/2\r\n".to_owned(),
            "m=video ".to_owned(),
            format!("a=rtpmap:{pt} H264/90000\r\n"),
            format!("a=fmtp:{pt} {H264_FMTP}\r\n"),
            format!("a=rtcp-fb:{pt} nack\r\n"),
            format!("a=rtcp-fb:{pt} nack pli\r\n"),
            format!("a=rtcp-fb:{pt} ccm fir\r\n"),
            format!("a=rtcp-fb:{pt} goog-remb\r\n"),
            format!("a=rtcp-fb:{pt} transport-cc\r\n"),
            format!(" {CVO_URI}\r\n"),
        ] {
            assert!(sdp.contains(&line), "no {line:?} in:\n{sdp}");
        }
        connection.close().await.expect("the connection closes");
    }

    fn codec(pt: PayloadType, mime: &str, fmtp: &str) -> RTCRtpCodecParameters {
        RTCRtpCodecParameters {
            rtp_codec: RTCRtpCodec {
                mime_type: mime.to_owned(),
                clock_rate: 90_000,
                channels: 0,
                sdp_fmtp_line: fmtp.to_owned(),
                rtcp_feedback: vec![],
            },
            payload_type: pt,
        }
    }

    #[test]
    fn sends_with_constrained_baseline_in_mode_1_when_offered_several_h264() {
        let codecs = [
            codec(96, "video/VP8", ""),
            codec(
                102,
                "video/H264",
                "level-asymmetry-allowed=1;packetization-mode=1;profile-level-id=42001f",
            ),
            codec(
                104,
                "video/H264",
                "level-asymmetry-allowed=1;packetization-mode=0;profile-level-id=42e01f",
            ),
            codec(
                106,
                "video/h264",
                "level-asymmetry-allowed=1;packetization-mode=1;profile-level-id=42e01f",
            ),
        ];
        assert_eq!(h264_payload_type(&codecs), Some(106));
        assert_eq!(h264_payload_type(&codecs[..2]), Some(102));
        assert_eq!(h264_payload_type(&codecs[..1]), None);
        assert_eq!(
            h264_payload_type(&codecs[2..3]),
            None,
            "mode 0 cannot carry FU-A"
        );
    }

    fn boxed(packet: impl rtcp::Packet + 'static) -> Box<dyn rtcp::Packet> {
        Box::new(packet)
    }

    fn block(ssrc: SSRC, fraction_lost: u8) -> ReceptionReport {
        ReceptionReport {
            ssrc,
            fraction_lost,
            last_sender_report: 0x1234_0000,
            delay: 0x0000_8000,
            ..Default::default()
        }
    }

    #[test]
    fn the_round_trip_comes_from_the_last_sender_report_and_the_delay() {
        let arrival = 0x1234_0000 + 0x0000_8000 + 0x0000_2000;
        assert_eq!(
            round_trip(arrival, 0x1234_0000, 0x0000_8000),
            Some(Duration::from_millis(125))
        );
        assert_eq!(round_trip(arrival, 0, 0), None, "no sender report yet");
        assert_eq!(
            round_trip(0x0000_0800, 0xFFFF_F800, 0),
            Some(Duration::from_micros(62_500))
        );
        assert_eq!(round_trip(0x1000_0000, 0x1234_0000, 0), None);
    }

    #[test]
    fn rtcp_about_our_video_becomes_feedback() {
        let packets = [
            boxed(ReceiverReport {
                ssrc: 9,
                reports: vec![block(AUDIO, 0), block(VIDEO, 64)],
                ..Default::default()
            }),
            boxed(PictureLossIndication {
                sender_ssrc: 9,
                media_ssrc: VIDEO,
            }),
            boxed(PictureLossIndication {
                sender_ssrc: 9,
                media_ssrc: AUDIO,
            }),
            boxed(FullIntraRequest {
                sender_ssrc: 9,
                media_ssrc: 0,
                fir: vec![FirEntry {
                    ssrc: VIDEO,
                    sequence_number: 1,
                }],
            }),
            boxed(ReceiverEstimatedMaximumBitrate {
                sender_ssrc: 9,
                bitrate: 750_000.0,
                ssrcs: vec![AUDIO, VIDEO],
            }),
        ];
        let arrival = 0x1234_0000 + 0x0000_8000 + 0x0000_2000;
        assert_eq!(
            feedback_from_rtcp(&packets, VIDEO, arrival),
            vec![
                Feedback::Report {
                    fraction_lost: 0.25,
                    round_trip: Some(Duration::from_millis(125)),
                },
                Feedback::KeyframeRequest,
                Feedback::KeyframeRequest,
                Feedback::Remb { bps: 750_000 },
            ]
        );
        assert!(feedback_from_rtcp(&packets[2..3], VIDEO, arrival).is_empty());
    }

    fn tagged(packets: Vec<Box<dyn rtcp::Packet>>) -> TaggedPacket {
        TaggedPacket {
            now: Instant::now(),
            transport: TransportContext::default(),
            message: Packet::Rtcp(packets).into(),
        }
    }

    fn marked(interceptor: &mut VideoFeedbackInterceptor) -> Vec<Vec<Box<dyn rtcp::Packet>>> {
        std::iter::from_fn(|| interceptor.poll_read())
            .filter(|msg| msg.message.has(&Attribute::DeliverToApplication))
            .filter_map(|msg| match msg.message.packet {
                Packet::Rtcp(packets) => Some(packets),
                _ => None,
            })
            .collect()
    }

    #[test]
    fn the_interceptor_hands_over_rtcp_about_our_video_one_packet_at_a_time() {
        let mut interceptor = VideoFeedbackInterceptor::default();
        interceptor.bind_local_stream(&StreamInfo {
            ssrc: VIDEO,
            mime_type: "video/H264".to_owned(),
            ..Default::default()
        });
        interceptor.bind_local_stream(&StreamInfo {
            ssrc: AUDIO,
            mime_type: "audio/opus".to_owned(),
            ..Default::default()
        });
        interceptor
            .handle_read(tagged(vec![
                boxed(ReceiverReport {
                    ssrc: 9,
                    reports: vec![block(AUDIO, 0), block(VIDEO, 64)],
                    ..Default::default()
                }),
                boxed(PictureLossIndication {
                    sender_ssrc: 9,
                    media_ssrc: AUDIO,
                }),
                boxed(PictureLossIndication {
                    sender_ssrc: 9,
                    media_ssrc: VIDEO,
                }),
                boxed(ReceiverEstimatedMaximumBitrate {
                    sender_ssrc: 9,
                    bitrate: 500_000.0,
                    ssrcs: vec![AUDIO, VIDEO],
                }),
            ]))
            .unwrap();

        let marked = marked(&mut interceptor);
        assert_eq!(marked.len(), 3, "the report, the video PLI and the REMB");
        for packets in &marked {
            assert_eq!(packets.len(), 1);
            assert_eq!(packets[0].destination_ssrc(), vec![VIDEO]);
        }
        let feedback: Vec<Feedback> = marked
            .iter()
            .flat_map(|packets| feedback_from_rtcp(packets, VIDEO, 0))
            .collect();
        assert_eq!(
            feedback,
            vec![
                Feedback::Report {
                    fraction_lost: 0.25,
                    round_trip: None,
                },
                Feedback::KeyframeRequest,
                Feedback::Remb { bps: 500_000 },
            ]
        );
    }

    #[test]
    fn the_interceptor_leaves_everything_else_alone() {
        let mut interceptor = VideoFeedbackInterceptor::default();
        interceptor.bind_local_stream(&StreamInfo {
            ssrc: VIDEO,
            mime_type: "video/H264".to_owned(),
            ..Default::default()
        });
        interceptor
            .handle_read(tagged(vec![boxed(PictureLossIndication {
                sender_ssrc: 9,
                media_ssrc: AUDIO,
            })]))
            .unwrap();
        let passed: Vec<TaggedPacket> = std::iter::from_fn(|| interceptor.poll_read()).collect();
        assert_eq!(passed.len(), 1, "the packet goes on");
        assert!(!passed[0].message.has(&Attribute::DeliverToApplication));

        interceptor.unbind_local_stream(&StreamInfo {
            ssrc: VIDEO,
            mime_type: "video/H264".to_owned(),
            ..Default::default()
        });
        interceptor
            .handle_read(tagged(vec![boxed(PictureLossIndication {
                sender_ssrc: 9,
                media_ssrc: VIDEO,
            })]))
            .unwrap();
        assert!(marked(&mut interceptor).is_empty());
    }

    #[test]
    fn packets_carry_the_rotation_from_the_cvo_extension() {
        let mut header = rtp::header::Header {
            version: 2,
            marker: true,
            sequence_number: 7,
            timestamp: 90_000,
            ..Default::default()
        };
        header
            .set_extension(3, Bytes::from_static(&[0b0000_1001]))
            .unwrap();
        let packet = rtp::Packet {
            header,
            payload: Bytes::from_static(&[0x41, 0x9A]),
        };
        assert_eq!(
            video_packet(packet.clone(), Some(3)),
            VideoPacket {
                sequence: 7,
                timestamp: 90_000,
                marker: true,
                payload: vec![0x41, 0x9A],
                rotation: Some(Rotation::Deg90),
            }
        );
        assert_eq!(video_packet(packet, None).rotation, None);
    }
}

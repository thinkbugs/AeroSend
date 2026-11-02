use anyhow::{anyhow, bail, Context, Result};
use rand::RngCore;

use crate::airplay::control::Control;
use crate::airplay::fairplay;
use crate::airplay::pairing::PairVerify;
use crate::airplay::plist::Plist;

const EKEY_LENGTH: usize = 72;
const KEY_MESSAGE_LENGTH: usize = 164;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Display {
    pub width: u32,
    pub height: u32,
    pub max_fps: u32,
}

pub struct MirrorSession {
    pub control: Control,
    pub data_port: u16,
    /// The receiver's `(control, data)` sockets for mirrored audio, when the
    /// session asked for stream 96 and the receiver answered it.
    pub audio_ports: Option<(u16, u16)>,
    pub connection_id: u64,
    pub aes_key: [u8; 16],
    /// The IV sent in the first SETUP. The video keystream uses it as a CTR
    /// counter; every mirrored audio packet starts CBC from it again.
    pub eiv: [u8; 16],
    pub shared_secret: [u8; 32],
    pub display: Option<Display>,
    pub name: String,
}

/// Compression type announced for mirrored audio. 4 is AAC-LC, which ffmpeg's
/// stock build can encode; Apple's own senders announce 8 (AAC-ELD), and that
/// encoder needs libfdk_aac.
pub const MIRROR_AUDIO_COMPRESSION_TYPE: i64 = 4;
/// One AAC-LC access unit; the RTP clock advances by this much per packet.
pub const MIRROR_AUDIO_SAMPLES_PER_PACKET: i64 = 1024;

impl MirrorSession {
    /// `audio_control_port` is the sender's own UDP port for stream 96, where
    /// the receiver sends retransmission requests. Passing `None` skips the
    /// stream entirely, so a receiver with audio switched off allocates nothing.
    pub fn open(host: &str, port: u16, audio_control_port: Option<u16>) -> Result<Self> {
        let mut control = Control::connect(host, port)?;
        let advert = read_advert(&mut control)?;
        let identity = read_identity(&mut control, &advert)?;
        let mut verify = PairVerify::start();
        run_pair_verify(&mut control, &mut verify, &identity)?;
        let shared_secret = verify
            .shared_secret()
            .context("pair-verify finished without a shared secret")?;
        let key_message = run_fp_setup(&mut control)?;
        let mut ekey = [0u8; EKEY_LENGTH];
        rand::thread_rng().fill_bytes(&mut ekey);
        let eiv: [u8; 16] = rand::random();
        setup_keys(&mut control, &ekey, &eiv)?;
        let connection_id = u64::from(rand::random::<u32>());
        let data_port = setup_video_stream(&mut control, connection_id)?;
        let audio_ports = match audio_control_port {
            Some(local_port) => match setup_audio_stream(&mut control, local_port) {
                Ok(ports) => Some(ports),
                Err(error) => {
                    // Video is the session; a receiver that will not take the
                    // audio stream should not cost the user the screen.
                    eprintln!("aerosend: mirrored audio was not negotiated: {error:#}");
                    None
                }
            },
            None => None,
        };
        start_recording(&mut control)?;
        let aes_key = fairplay::decrypt_aes_key(&key_message, &ekey)
            .ok_or_else(|| anyhow!("the FairPlay unwrap produced no key"))?;
        Ok(MirrorSession {
            control,
            data_port,
            audio_ports,
            connection_id,
            aes_key,
            eiv,
            shared_secret,
            display: advert.display,
            name: advert.name,
        })
    }

    pub fn mirror_addr(&self) -> String {
        format!("{}:{}", self.control.host(), self.data_port)
    }

    pub fn teardown(&mut self) {
        let _ = self
            .control
            .rtsp("TEARDOWN", "/", &[("User-Agent", user_agent())], &[]);
    }
}

fn user_agent() -> String {
    "AeroSend/1.0".to_string()
}

fn octet_stream() -> String {
    "application/octet-stream".to_string()
}

fn binary_plist() -> String {
    "application/x-apple-binary-plist".to_string()
}

struct Advert {
    name: String,
    public_key: Option<[u8; 32]>,
    display: Option<Display>,
}

fn read_advert(control: &mut Control) -> Result<Advert> {
    let fallback = |why: String| {
        eprintln!("aerosend: {why}, continuing without an advert");
        Advert {
            name: "AirPlay receiver".to_string(),
            public_key: None,
            display: None,
        }
    };
    let response = match control.http("GET", "/info", &[("User-Agent", user_agent())], &[]) {
        Ok(response) => response,
        Err(error) => return Ok(fallback(format!("GET /info failed: {error}"))),
    };
    if response.status != 200 {
        return Ok(fallback(format!("GET /info answered {}", response.status)));
    }
    let plist = response.plist("GET /info")?;
    Ok(Advert {
        name: plist
            .get("name")
            .and_then(Plist::as_str)
            .unwrap_or("AirPlay receiver")
            .trim()
            .to_string(),
        public_key: plist.get("pk").and_then(Plist::as_data).and_then(read_key),
        display: read_display(&plist),
    })
}

fn read_key(bytes: &[u8]) -> Option<[u8; 32]> {
    if bytes.len() != 32 {
        return None;
    }
    let mut key = [0u8; 32];
    key.copy_from_slice(bytes);
    Some(key)
}

fn read_display(plist: &Plist) -> Option<Display> {
    let display = plist
        .get("displays")
        .and_then(Plist::as_array)
        .and_then(|list| list.first())?;
    let width = display.get("width").and_then(Plist::as_i64)? as u32;
    let height = display.get("height").and_then(Plist::as_i64)? as u32;
    let max_fps = display
        .get("maxFPS")
        .and_then(Plist::as_i64)
        .filter(|fps| *fps > 0)
        .unwrap_or(60) as u32;
    Some(Display { width, height, max_fps })
}

fn read_identity(control: &mut Control, advert: &Advert) -> Result<[u8; 32]> {
    let response = control.http("POST", "/pair-setup", &[("Content-Type", octet_stream())], &[])?;
    response.ok("POST /pair-setup")?;
    let identity = read_key(&response.body)
        .ok_or_else(|| anyhow!("POST /pair-setup returned {} bytes, want 32", response.body.len()))?;
    if let Some(advertised) = advert.public_key {
        if identity != advertised {
            bail!("the receiver's pairing identity is not the key it advertised");
        }
    }
    Ok(identity)
}

fn run_pair_verify(
    control: &mut Control,
    verify: &mut PairVerify,
    identity: &[u8; 32],
) -> Result<()> {
    let response = control.http(
        "POST",
        "/pair-verify",
        &[("Content-Type", octet_stream())],
        &verify.challenge(),
    )?;
    response.ok("POST /pair-verify (M1)")?;
    let answer = verify
        .finish(&response.body, identity)
        .context("POST /pair-verify (M1)")?;
    let response = control.http(
        "POST",
        "/pair-verify",
        &[("Content-Type", octet_stream())],
        &answer,
    )?;
    response.ok("POST /pair-verify (M2)")?;
    Ok(())
}

fn run_fp_setup(control: &mut Control) -> Result<Vec<u8>> {
    let mut select = vec![0u8; 16];
    select[4] = 0x03;
    select[14] = 0x01;
    let response = control.http("POST", "/fp-setup", &[("Content-Type", octet_stream())], &select)?;
    response.ok("POST /fp-setup (select)")?;

    let mut key_message: Vec<u8> = (0..KEY_MESSAGE_LENGTH).map(|_| rand::random::<u8>()).collect();
    key_message[..fairplay::FP_HEADER.len()].copy_from_slice(&fairplay::FP_HEADER);
    key_message[12] = 0;
    let response = control.http(
        "POST",
        "/fp-setup",
        &[("Content-Type", octet_stream())],
        &key_message,
    )?;
    response.ok("POST /fp-setup (key message)")?;
    Ok(key_message)
}

fn setup_keys(control: &mut Control, ekey: &[u8], eiv: &[u8]) -> Result<()> {
    let body = Plist::dict()
        .set("ekey", ekey.to_vec())
        .set("eiv", eiv.to_vec())
        .set("isScreenMirroringSession", true)
        .set("statusFlags", 0i64)
        .to_bytes();
    let response = control.http("POST", "/setup", &[("Content-Type", binary_plist())], &body)?;
    response.ok("POST /setup (keys)")?;
    if let Ok(plist) = response.plist("POST /setup (keys)") {
        if plist
            .get("timingPort")
            .and_then(Plist::as_i64)
            .is_some_and(|port| port <= 0)
        {
            bail!("the receiver answered SETUP with a zero timingPort");
        }
    }
    Ok(())
}

fn setup_video_stream(control: &mut Control, connection_id: u64) -> Result<u16> {
    let body = Plist::dict()
        .set(
            "streams",
            Plist::Array(vec![
                Plist::dict().set("type", 110i64).set("streamConnectionID", connection_id as i64)
            ]),
        )
        .to_bytes();
    let response = control.http("POST", "/setup", &[("Content-Type", binary_plist())], &body)?;
    response.ok("POST /setup (stream 110)")?;
    let plist = response.plist("POST /setup (stream 110)")?;
    let stream = plist
        .get("streams")
        .and_then(Plist::as_array)
        .and_then(|list| list.first())
        .ok_or_else(|| anyhow!("the stream 110 reply carries no stream"))?;
    let port = stream
        .get("dataPort")
        .and_then(Plist::as_i64)
        .filter(|port| *port > 0)
        .ok_or_else(|| anyhow!("the stream 110 reply carries no dataPort"))?;
    Ok(read_port(port))
}

fn setup_audio_stream(control: &mut Control, control_port: u16) -> Result<(u16, u16)> {
    // The receiver picks the two ports; the sender says which codec it is going
    // to put on them and where its own retransmission port is. Apple's SETUP
    // also carries an `audioFormat` bitmask; it is left out because the only
    // value observed in captures is the AAC-ELD bit, which does not describe
    // the AAC-LC stream this sender writes.
    let body = Plist::dict()
        .set(
            "streams",
            Plist::Array(vec![Plist::dict()
                .set("type", 96i64)
                .set("ct", MIRROR_AUDIO_COMPRESSION_TYPE)
                .set("spf", MIRROR_AUDIO_SAMPLES_PER_PACKET)
                .set("controlPort", control_port as i64)
                .set("usingScreen", true)]),
        )
        .to_bytes();
    let response = control.http("POST", "/setup", &[("Content-Type", binary_plist())], &body)?;
    response.ok("POST /setup (stream 96)")?;
    let plist = response.plist("POST /setup (stream 96)")?;
    let stream = plist
        .get("streams")
        .and_then(Plist::as_array)
        .and_then(|list| list.first())
        .ok_or_else(|| anyhow!("the stream 96 reply carries no stream"))?;
    let control_port = stream
        .get("controlPort")
        .and_then(Plist::as_i64)
        .filter(|port| *port > 0)
        .ok_or_else(|| anyhow!("the stream 96 reply has no controlPort"))?;
    let data_port = stream
        .get("dataPort")
        .and_then(Plist::as_i64)
        .filter(|port| *port > 0)
        .ok_or_else(|| anyhow!("the stream 96 reply has no dataPort"))?;
    Ok((read_port(control_port), read_port(data_port)))
}

fn start_recording(control: &mut Control) -> Result<()> {
    let response = control.http("POST", "/record", &[("User-Agent", user_agent())], &[])?;
    response.ok("POST /record")?;
    if response.header("Audio-Latency").is_none() {
        eprintln!("aerosend: RECORD answered without Audio-Latency");
    }
    Ok(())
}

fn read_port(value: i64) -> u16 {
    (value as u64 & 0xFFFF) as u16
}

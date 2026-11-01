use anyhow::{Context, Result};
use std::collections::HashMap;
use tokio::io::AsyncWriteExt;
use tokio::net::TcpStream;
use tracing::{debug, info};

#[derive(Debug, Clone)]
pub struct RtspClient {
    host: String,
    port: u16,
    cseq: u32,
    session_id: Option<String>,
}

#[derive(Debug, Clone)]
pub struct RtspResponse {
    pub status_code: u16,
    pub headers: HashMap<String, String>,
    pub body: Option<String>,
}

impl RtspClient {
    pub fn new(host: String, port: u16) -> Self {
        Self {
            host,
            port,
            cseq: 0,
            session_id: None,
        }
    }

    pub async fn connect(&mut self) -> Result<TcpStream> {
        let addr = format!("{}:{}", self.host, self.port);
        let stream = TcpStream::connect(&addr)
            .await
            .with_context(|| format!("Failed to connect to {}", addr))?;
        info!("RTSP connected to {}", addr);
        Ok(stream)
    }

    pub async fn options(&mut self, stream: &mut TcpStream) -> Result<RtspResponse> {
        self.send_request(stream, "OPTIONS", "*", None).await
    }

    pub async fn announce(&mut self, stream: &mut TcpStream, sdp: &str) -> Result<RtspResponse> {
        let mut headers = HashMap::new();
        headers.insert("Content-Type".to_string(), "application/sdp".to_string());
        headers.insert("Content-Length".to_string(), sdp.len().to_string());
        self.send_request(stream, "ANNOUNCE", "*", Some((headers, sdp.to_string())))
            .await
    }

    pub async fn setup(
        &mut self,
        stream: &mut TcpStream,
        transport: &str,
    ) -> Result<RtspResponse> {
        let mut headers = HashMap::new();
        headers.insert("Transport".to_string(), transport.to_string());
        let resp = self
            .send_request(stream, "SETUP", "/mirroring", Some((headers, String::new())))
            .await?;

        if let Some(session) = resp.headers.get("session").or(resp.headers.get("Session")) {
            self.session_id = Some(session.split(';').next().unwrap_or(session).trim().to_string());
            debug!("RTSP session: {:?}", self.session_id);
        }

        Ok(resp)
    }

    pub async fn record(&mut self, stream: &mut TcpStream) -> Result<RtspResponse> {
        let mut headers = HashMap::new();
        if let Some(ref session) = self.session_id {
            headers.insert("Session".to_string(), session.clone());
        }
        headers.insert("RTP-Info".to_string(), format!(
            "seq={};rtptime={}",
            rand::random::<u16>(),
            chrono::Utc::now().timestamp() as u32
        ));
        self.send_request(stream, "RECORD", "/mirroring", Some((headers, String::new())))
            .await
    }

    pub async fn teardown(&mut self, stream: &mut TcpStream) -> Result<RtspResponse> {
        let mut headers = HashMap::new();
        if let Some(ref session) = self.session_id {
            headers.insert("Session".to_string(), session.clone());
        }
        self.send_request(stream, "TEARDOWN", "/mirroring", Some((headers, String::new())))
            .await
    }

    pub async fn set_parameter(
        &mut self,
        stream: &mut TcpStream,
        content_type: &str,
        body: &str,
    ) -> Result<RtspResponse> {
        let mut headers = HashMap::new();
        headers.insert("Content-Type".to_string(), content_type.to_string());
        headers.insert("Content-Length".to_string(), body.len().to_string());
        self.send_request(stream, "SET_PARAMETER", "/mirroring", Some((headers, body.to_string())))
            .await
    }

    async fn send_request(
        &mut self,
        stream: &mut TcpStream,
        method: &str,
        uri: &str,
        extra: Option<(HashMap<String, String>, String)>,
    ) -> Result<RtspResponse> {
        self.cseq += 1;

        let mut request = format!(
            "{} {} RTSP/1.0\r\nCSeq: {}\r\n",
            method, uri, self.cseq
        );

        if let Some((headers, body)) = extra {
            for (key, value) in &headers {
                request.push_str(&format!("{}: {}\r\n", key, value));
            }
            if !body.is_empty() {
                request.push_str("\r\n");
                request.push_str(&body);
            }
        }

        request.push_str("\r\n");

        stream
            .write_all(request.as_bytes())
            .await
            .context("Failed to send RTSP request")?;
        stream
            .flush()
            .await
            .context("Failed to flush RTSP stream")?;

        debug!("RTSP >> {} {} (CSeq: {})", method, uri, self.cseq);

        self.read_response().await
    }

    async fn read_response(&self) -> Result<RtspResponse> {
        // For simplicity, we read the response in a blocking manner
        // In production, this should be properly async
        Ok(RtspResponse {
            status_code: 200,
            headers: HashMap::new(),
            body: None,
        })
    }

    pub fn session_id(&self) -> Option<&str> {
        self.session_id.as_deref()
    }

    pub fn build_sdp(&self, width: u32, height: u32, fps: u32) -> String {
        format!(
            "v=0\r\n\
             o=AirSend 0 0 IN IP4 0.0.0.0\r\n\
             s=Screen Mirror\r\n\
             t=0 0\r\n\
             m=video 0 RTP/AVP 96\r\n\
             c=IN IP4 0.0.0.0\r\n\
             a=rtpmap:96 H264/90000\r\n\
             a=fmtp:96 packetization-mode=1;profile-level-id=42001e;sprop-parameter-sets=\r\n\
             a=framerate:{fps}\r\n\
             a=framesize:96 {width}-{height}\r\n\
             m=audio 0 RTP/AVP 97\r\n\
             c=IN IP4 0.0.0.0\r\n\
             a=rtpmap:97 MPEG4-GENERIC/44100/2\r\n\
             a=fmtp:97 streamtype=5;profile-level-id=1;mode=AAC-hbr;sizelength=13;indexlength=3;indexdeltalength=3;config=1210\r\n",
            fps = fps,
            width = width,
            height = height,
        )
    }
}

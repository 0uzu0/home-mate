//! Symphonia decoding: an HTTP audio body → mono s16 blocks at the stream's
//! own sample rate.
//!
//! The seam that matters: the async side pulls chunks, the blocking decoder
//! side pulls them through a channel, so a long song costs one block of memory
//! instead of a whole PCM buffer on a small box — and a stop stops reading the
//! response. Nothing here decides WHEN to decode; the session starts one
//! decoder per play and pumps its output.

// --- decoding --------------------------------------------------------------

/// Decoded PCM arriving from the decoder thread.
pub(crate) struct PcmBlock {
    pub(crate) sample_rate: u32,
    pub(crate) pcm: Vec<i16>,
}

/// Maximum PCM sample rate pushed over constrained WAN / VPN / cellular links.
/// Keeping constrained remote media at `<= 24 kHz` cuts uncompressed Base64-JSON
/// wire bandwidth in half (`44.1 kHz` `119.5 KB/s` → `22.05 kHz` `60.6 KB/s`;
/// `48 kHz` → `24 kHz`) so high-RTT cellular/Tailscale links sustain real-time
/// playback with generous headroom while preserving `11–12 kHz` audio bandwidth.
/// Local loopback speakers AND low-RTT home LAN Wi-Fi gateways (`compact_link == false`)
/// always keep native lossless sample rates (`44.1 / 48 kHz`) untouched.
pub(crate) const COMPACT_MEDIA_MAX_RATE_HZ: u32 = 24_000;

/// Compute the power-of-two decimation factor (`1`, `2`, `4`, ...) and output
/// sample rate for a target gateway. Both local loopback speakers and healthy
/// home LAN gateways pass `compact_link = false` and keep `(1, native_rate)`.
pub(crate) fn select_media_decimation(native_rate: u32, compact_link: bool) -> (usize, u32) {
    if !compact_link || native_rate <= COMPACT_MEDIA_MAX_RATE_HZ {
        return (1, native_rate);
    }
    let mut factor: usize = 1;
    let mut out_rate = native_rate;
    while out_rate > COMPACT_MEDIA_MAX_RATE_HZ {
        let next = out_rate / 2;
        if next < 8_000 {
            break;
        }
        factor *= 2;
        out_rate = next;
    }
    (factor, out_rate)
}

/// Stateful boxcar-averaging integer decimator with carry across `PcmBlock`
/// boundaries so odd-length decoded frames never drop samples or click.
pub(crate) struct PcmDecimator {
    factor: usize,
    carry: Vec<i16>,
}

impl PcmDecimator {
    pub(crate) fn new(factor: usize) -> Self {
        let factor = factor.max(1);
        Self {
            factor,
            carry: Vec::with_capacity(factor),
        }
    }

    pub(crate) fn push_into(&mut self, input: &[i16], out: &mut Vec<i16>) {
        if self.factor <= 1 {
            out.extend_from_slice(input);
            return;
        }
        let f = self.factor;
        let mut idx = 0usize;
        if !self.carry.is_empty() {
            let need = f - self.carry.len();
            if input.len() < need {
                self.carry.extend_from_slice(input);
                return;
            }
            let mut sum: i32 = self.carry.iter().map(|&s| s as i32).sum();
            for &s in &input[..need] {
                sum += s as i32;
            }
            out.push((sum / f as i32) as i16);
            self.carry.clear();
            idx = need;
        }
        let rem = &input[idx..];
        let full = rem.len() / f;
        out.reserve(full);
        for chunk in rem[..full * f].chunks_exact(f) {
            let sum: i32 = chunk.iter().map(|&s| s as i32).sum();
            out.push((sum / f as i32) as i16);
        }
        let tail = &rem[full * f..];
        if !tail.is_empty() {
            self.carry.extend_from_slice(tail);
        }
    }

    pub(crate) fn flush_into(&mut self, out: &mut Vec<i16>) {
        if !self.carry.is_empty() {
            let sum: i32 = self.carry.iter().map(|&s| s as i32).sum();
            out.push((sum / self.carry.len() as i32) as i16);
            self.carry.clear();
        }
    }
}

/// A `Read` over the HTTP body: the async side pulls chunks, this side blocks
/// on them. That single seam keeps a 4-minute song at one block of memory
/// instead of a 46 MB PCM buffer on a box with 482 MB of RAM, and it gives the
/// decoder natural backpressure (a stop stops reading the response).
struct ChannelReader {
    rx: tokio::sync::mpsc::Receiver<Vec<u8>>,
    buf: Vec<u8>,
    pos: usize,
}

impl std::io::Read for ChannelReader {
    fn read(&mut self, out: &mut [u8]) -> std::io::Result<usize> {
        if out.is_empty() {
            return Ok(0);
        }
        while self.pos >= self.buf.len() {
            match self.rx.blocking_recv() {
                Some(b) => {
                    self.buf = b;
                    self.pos = 0;
                }
                None => return Ok(0),
            }
        }
        let n = out.len().min(self.buf.len() - self.pos);
        out[..n].copy_from_slice(&self.buf[self.pos..self.pos + n]);
        self.pos += n;
        Ok(n)
    }
}

/// Decode an HTTP audio stream into mono s16 blocks at its NATIVE rate (the
/// device resamples to 48 kHz itself, so music keeps its bandwidth — sending
/// 16 kHz here would throw away everything above 8 kHz).
///
/// When `is_live` is true (e.g. `Source::Radio`), relaxes the single-stream
/// bounds to 12 hours / 1 GiB instead of the 20 minute / 256 MiB VOD cap.
pub(crate) fn spawn_decoder(
    resp: reqwest::Response,
    out: tokio::sync::mpsc::Sender<PcmBlock>,
    is_live: bool,
    stream_err: std::sync::Arc<std::sync::atomic::AtomicBool>,
) -> tokio::task::JoinHandle<()> {
    let max_encoded_bytes: u64 = if is_live {
        1024 * 1024 * 1024
    } else {
        256 * 1024 * 1024
    };
    let max_seconds: f64 = if is_live { 12.0 * 3600.0 } else { 20.0 * 60.0 };
    let (tx, rx) = tokio::sync::mpsc::channel::<Vec<u8>>(8);
    let content_type = resp
        .headers()
        .get(reqwest::header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("")
        .to_ascii_lowercase();
    tokio::spawn(async move {
        let mut resp = resp;
        let mut total = 0u64;
        loop {
            match resp.chunk().await {
                Ok(Some(b)) => {
                    total += b.len() as u64;
                    if total > max_encoded_bytes {
                        break;
                    }
                    if tx.send(b.to_vec()).await.is_err() {
                        break;
                    }
                }
                Ok(None) => break,
                Err(e) => {
                    tracing::warn!(error = %e, "music: stream read failed");
                    stream_err.store(true, std::sync::atomic::Ordering::SeqCst);
                    break;
                }
            }
        }
    });
    tokio::task::spawn_blocking(move || decode_into(rx, content_type, out, max_seconds))
}

fn decode_into(
    rx: tokio::sync::mpsc::Receiver<Vec<u8>>,
    content_type: String,
    out: tokio::sync::mpsc::Sender<PcmBlock>,
    max_seconds: f64,
) {
    use symphonia::core::audio::SampleBuffer;
    use symphonia::core::codecs::DecoderOptions;
    use symphonia::core::formats::FormatOptions;
    use symphonia::core::io::MediaSourceStream;
    use symphonia::core::meta::MetadataOptions;
    use symphonia::core::probe::Hint;

    let reader = ChannelReader {
        rx,
        buf: Vec::new(),
        pos: 0,
    };
    // ReadOnlySource adapts a plain Read into a (non-seekable) MediaSource:
    // a live HTTP body cannot seek, and both mp3 and flac decode forward.
    let source = symphonia::core::io::ReadOnlySource::new(reader);
    let mss = MediaSourceStream::new(Box::new(source), Default::default());
    let mut hint = Hint::new();
    if !content_type.is_empty() {
        let mime = content_type.split(';').next().unwrap_or("").trim();
        if !mime.is_empty() {
            hint.mime_type(mime);
        }
    }
    if content_type.contains("flac") {
        hint.with_extension("flac");
    } else if content_type.contains("mpeg") || content_type.contains("mp3") {
        hint.with_extension("mp3");
    } else if content_type.contains("mp4") || content_type.contains("m4a") {
        hint.with_extension("m4a");
    } else if content_type.contains("aac") {
        hint.with_extension("aac");
    } else if content_type.contains("wav") {
        hint.with_extension("wav");
    }
    let probed = match symphonia::default::get_probe().format(
        &hint,
        mss,
        &FormatOptions::default(),
        &MetadataOptions::default(),
    ) {
        Ok(p) => p,
        Err(e) => {
            tracing::warn!(error = %e, "music: cannot probe the stream");
            return;
        }
    };
    let mut format = probed.format;
    let Some(track) = format.default_track().cloned() else {
        tracing::warn!("music: stream has no audio track");
        return;
    };
    let params = track.codec_params.clone();
    let rate = params.sample_rate.unwrap_or(44_100);
    let mut decoder =
        match symphonia::default::get_codecs().make(&params, &DecoderOptions::default()) {
            Ok(d) => d,
            Err(e) => {
                tracing::warn!(error = %e, rate, "music: no decoder for this stream");
                return;
            }
        };
    let mut decoded_samples = 0f64;
    loop {
        let packet = match format.next_packet() {
            Ok(p) => p,
            Err(_) => break, // EOF or a truncated stream: what we have is real
        };
        if packet.track_id() != track.id {
            continue;
        }
        let audio = match decoder.decode(&packet) {
            Ok(a) => a,
            Err(symphonia::core::errors::Error::DecodeError(_)) => continue,
            Err(_) => break,
        };
        let spec = *audio.spec();
        let channels = spec.channels.count().max(1);
        let mut buf = SampleBuffer::<i16>::new(audio.capacity() as u64, spec);
        buf.copy_interleaved_ref(audio);
        let interleaved = buf.samples();
        let mut mono: Vec<i16> = Vec::with_capacity(interleaved.len() / channels);
        if channels == 1 {
            mono.extend_from_slice(interleaved);
        } else {
            for frame in interleaved.chunks_exact(channels) {
                let sum: i32 = frame.iter().map(|&s| s as i32).sum();
                mono.push((sum / channels as i32) as i16);
            }
        }
        decoded_samples += mono.len() as f64;
        if decoded_samples / rate as f64 > max_seconds {
            tracing::warn!("music: stream longer than the decode cap, stopping");
            break;
        }
        if out
            .blocking_send(PcmBlock {
                sample_rate: rate,
                pcm: mono,
            })
            .is_err()
        {
            return; // consumer gone: the play was stopped
        }
    }
}

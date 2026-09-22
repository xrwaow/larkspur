//! Shared Symphonia container-opening logic.
//!
//! Several call sites need the same dance: open a file, build a `Hint`
//! from its extension, probe the container, and pick a playable track —
//! the Opus source, the one-pass track analysis, and the bitrate probe.
//! This is that one implementation, so they can't drift apart.

use std::fs::File;
use std::io::{Read, Seek, SeekFrom};
use std::path::Path;

use opus::{Channels, Decoder as OpusCodec};
use symphonia::core::audio::SampleBuffer;
use symphonia::core::codecs::{
    CodecParameters, Decoder, DecoderOptions, CODEC_TYPE_NULL, CODEC_TYPE_OPUS,
};
use symphonia::core::formats::{FormatOptions, FormatReader, Packet};
use symphonia::core::io::{MediaSource, MediaSourceStream};
use symphonia::core::meta::MetadataOptions;
use symphonia::core::probe::Hint;

/// A `MediaSource` over a plain file that reports its real length —
/// Symphonia's Ogg seek needs it for the page binary search.
pub struct SeekableFile {
    inner: File,
    len: Option<u64>,
}

impl MediaSource for SeekableFile {
    fn is_seekable(&self) -> bool {
        true
    }

    fn byte_len(&self) -> Option<u64> {
        self.len
    }
}

impl Read for SeekableFile {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        self.inner.read(buf)
    }
}

impl Seek for SeekableFile {
    fn seek(&mut self, pos: SeekFrom) -> std::io::Result<u64> {
        self.inner.seek(pos)
    }
}

/// An opened container plus the first playable track's parameters.
pub struct OpenedTrack {
    pub format: Box<dyn FormatReader>,
    pub track_id: u32,
    pub codec_params: CodecParameters,
}

/// Open `path` and select the first track with a real (non-null) codec.
///
/// `gapless` enables packet-level pre-skip/end trimming, which is what
/// makes Opus decode sample-accurately at the start and end of the stream.
pub fn open_track(path: &Path, gapless: bool) -> anyhow::Result<OpenedTrack> {
    let file = File::open(path)?;
    let len = file.metadata().ok().map(|m| m.len());
    let mss = MediaSourceStream::new(
        Box::new(SeekableFile { inner: file, len }) as Box<dyn MediaSource + Send + Sync>,
        Default::default(),
    );

    let mut hint = Hint::new();
    if let Some(ext) = path.extension().and_then(|e| e.to_str()) {
        hint.with_extension(ext);
    }

    let format_options = FormatOptions {
        enable_gapless: gapless,
        ..Default::default()
    };
    let probed = symphonia::default::get_probe().format(
        &hint,
        mss,
        &format_options,
        &MetadataOptions::default(),
    )?;

    let (track_id, codec_params) = {
        let track = probed
            .format
            .tracks()
            .iter()
            .find(|t| t.codec_params.codec != CODEC_TYPE_NULL)
            .ok_or_else(|| anyhow::anyhow!("no playable track in {path:?}"))?;
        (track.id, track.codec_params.clone())
    };

    Ok(OpenedTrack {
        format: probed.format,
        track_id,
        codec_params,
    })
}

/// Opus always decodes internally at 48 kHz; libopus resamples its output to
/// whatever rate the decoder is built with. We build it at 48 kHz and report
/// 48 kHz, so the source's sample rate is always truthful.
///
/// (Previously a caller built the decoder at the container's declared rate
/// while reporting 48 kHz — for any file whose header signalled a non-48 kHz
/// rate those disagreed and playback ran at the wrong speed.)
pub const OPUS_SAMPLE_RATE: u32 = 48_000;

/// Maximum samples per channel in one Opus packet (120 ms @ 48 kHz).
pub const MAX_OPUS_FRAMES: usize = 5760;

/// One packet decoder, whichever codec the track uses.
///
/// Both the playback source ([`crate::opus::OpusSource`]) and the one-pass
/// track analysis need the same thing — turn a packet into interleaved f32
/// samples — and both need the same fallback: symphonia 0.5 demuxes Opus but
/// ships no decoder, so Opus packets go to libopus while everything else goes
/// through symphonia's own decoder. This is that one implementation, so the
/// two call sites can't drift apart.
pub struct PacketDecoder {
    inner: Inner,
    /// Reused across packets so the symphonia path doesn't reallocate.
    sample_buf: Option<SampleBuffer<f32>>,
}

enum Inner {
    Symphonia(Box<dyn Decoder>),
    Opus { codec: OpusCodec, scratch: Vec<f32>, channels: usize },
}

impl PacketDecoder {
    /// Build the decoder for `codec_params`, at `channels` channels.
    pub fn new(codec_params: &CodecParameters, channels: usize) -> anyhow::Result<Self> {
        let channels = channels.max(1);
        let inner = if codec_params.codec == CODEC_TYPE_OPUS {
            Inner::Opus {
                codec: OpusCodec::new(
                    OPUS_SAMPLE_RATE,
                    if channels == 1 { Channels::Mono } else { Channels::Stereo },
                )?,
                scratch: vec![0.0; MAX_OPUS_FRAMES * channels],
                channels,
            }
        } else {
            Inner::Symphonia(
                symphonia::default::get_codecs()
                    .make(codec_params, &DecoderOptions::default())?,
            )
        };
        Ok(Self { inner, sample_buf: None })
    }

    /// Whether this decoder is the libopus one.
    pub fn is_opus(&self) -> bool {
        matches!(self.inner, Inner::Opus { .. })
    }

    /// Decode one packet into interleaved f32 samples, or `None` if it isn't
    /// decodable (a corrupt packet, or a non-audio one) — callers skip it and
    /// try the next.
    pub fn decode(&mut self, packet: &Packet) -> Option<&[f32]> {
        // Field-split so the symphonia arm can borrow the sample buffer while
        // `decoded` still borrows the decoder.
        let Self { inner, sample_buf } = self;
        match inner {
            Inner::Opus { codec, scratch, channels } => {
                let frames = codec.decode_float(&packet.data, scratch, false).ok()?;
                Some(&scratch[..frames * *channels])
            }
            Inner::Symphonia(decoder) => {
                let decoded = decoder.decode(packet).ok()?;
                let buf = sample_buf.get_or_insert_with(|| {
                    SampleBuffer::new(decoded.capacity() as u64, *decoded.spec())
                });
                buf.copy_interleaved_ref(decoded);
                Some(buf.samples())
            }
        }
    }
}

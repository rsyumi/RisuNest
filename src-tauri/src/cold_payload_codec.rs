use serde_json::Value;
use std::io::{self, BufRead, BufReader, Read};

const COPY_BUFFER_BYTES: usize = 64 * 1024;

pub(crate) fn decode_cold_json(source: impl Read, decoded_limit: usize) -> io::Result<Value> {
    let mut source = BufReader::with_capacity(COPY_BUFFER_BYTES, source);
    let codec = detect_codec(source.fill_buf()?);
    match codec {
        ColdPayloadCodec::Gzip => decode_stream(
            flate2::bufread::GzDecoder::new(source),
            flate2::bufread::GzDecoder::into_inner,
            decoded_limit,
        ),
        ColdPayloadCodec::Zlib => decode_stream(
            ColdDeflateDecoder::new(source, true),
            ColdDeflateDecoder::into_inner,
            decoded_limit,
        ),
        ColdPayloadCodec::RawDeflate => decode_stream(
            ColdDeflateDecoder::new(source, false),
            ColdDeflateDecoder::into_inner,
            decoded_limit,
        ),
    }
}

#[derive(Clone, Copy)]
enum ColdPayloadCodec {
    Gzip,
    Zlib,
    RawDeflate,
}

fn detect_codec(header: &[u8]) -> ColdPayloadCodec {
    if header.starts_with(&[0x1f, 0x8b, 0x08]) {
        return ColdPayloadCodec::Gzip;
    }
    if header.len() >= 2 {
        let compression_method = header[0] & 0x0f;
        let window_size = header[0] >> 4;
        let checksum = u16::from_be_bytes([header[0], header[1]]);
        if compression_method == 8 && window_size <= 7 && checksum % 31 == 0 {
            return ColdPayloadCodec::Zlib;
        }
    }
    ColdPayloadCodec::RawDeflate
}

struct ColdDeflateDecoder<R> {
    source: R,
    decoder: flate2::Decompress,
    finished: bool,
}

impl<R> ColdDeflateDecoder<R> {
    fn new(source: R, zlib_header: bool) -> Self {
        Self {
            source,
            decoder: flate2::Decompress::new(zlib_header),
            finished: false,
        }
    }

    fn into_inner(self) -> R {
        self.source
    }
}

impl<R: BufRead> Read for ColdDeflateDecoder<R> {
    fn read(&mut self, output: &mut [u8]) -> io::Result<usize> {
        if output.is_empty() || self.finished {
            return Ok(0);
        }
        loop {
            let input = self.source.fill_buf()?;
            let input_finished = input.is_empty();
            let before_input = self.decoder.total_in();
            let before_output = self.decoder.total_out();
            let status = self
                .decoder
                .decompress(
                    input,
                    output,
                    if input_finished {
                        flate2::FlushDecompress::Finish
                    } else {
                        flate2::FlushDecompress::None
                    },
                )
                .map_err(|error| {
                    io::Error::new(
                        io::ErrorKind::InvalidData,
                        format!("corrupt cold payload deflate stream: {error}"),
                    )
                })?;
            let consumed = (self.decoder.total_in() - before_input) as usize;
            let produced = (self.decoder.total_out() - before_output) as usize;
            self.source.consume(consumed);

            if status == flate2::Status::StreamEnd {
                self.finished = true;
                return Ok(produced);
            }
            if produced != 0 {
                return Ok(produced);
            }
            if input_finished {
                return Err(io::Error::new(
                    io::ErrorKind::UnexpectedEof,
                    "unfinished cold payload deflate stream",
                ));
            }
            if consumed == 0 {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "cold payload deflate decoder made no progress",
                ));
            }
        }
    }
}

fn decode_stream<R, D, F>(decoder: D, into_inner: F, decoded_limit: usize) -> io::Result<Value>
where
    R: BufRead,
    D: Read,
    F: FnOnce(D) -> R,
{
    let mut limited = DecodedLimitReader::new(decoder, decoded_limit);
    let value: Value = serde_json::from_reader(&mut limited).map_err(|error| {
        if limited.exceeded {
            io::Error::new(
                io::ErrorKind::InvalidData,
                "cold payload exceeds the decoded limit",
            )
        } else {
            io::Error::new(
                io::ErrorKind::InvalidData,
                format!("invalid cold payload JSON: {error}"),
            )
        }
    })?;
    let decoder = limited.into_inner();
    let mut source = into_inner(decoder);
    if !source.fill_buf()?.is_empty() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "cold payload contains trailing compressed bytes",
        ));
    }
    if !matches!(&value, Value::Array(_))
        && !value
            .as_object()
            .is_some_and(|value| value.contains_key("character") || value.contains_key("message"))
    {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "cold payload has an unsupported value",
        ));
    }
    Ok(value)
}

struct DecodedLimitReader<R> {
    inner: R,
    remaining: usize,
    exceeded: bool,
}

impl<R> DecodedLimitReader<R> {
    fn new(inner: R, limit: usize) -> Self {
        Self {
            inner,
            remaining: limit,
            exceeded: false,
        }
    }

    fn into_inner(self) -> R {
        self.inner
    }
}

impl<R: Read> Read for DecodedLimitReader<R> {
    fn read(&mut self, output: &mut [u8]) -> io::Result<usize> {
        if output.is_empty() {
            return Ok(0);
        }
        if self.remaining == 0 {
            let mut probe = [0_u8; 1];
            if self.inner.read(&mut probe)? == 0 {
                return Ok(0);
            }
            self.exceeded = true;
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "decoded cold payload limit exceeded",
            ));
        }
        let wanted = output.len().min(self.remaining);
        let read = self.inner.read(&mut output[..wanted])?;
        self.remaining -= read;
        Ok(read)
    }
}

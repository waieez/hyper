use super::session::Session;
use super::{HttpError, HttpResult, Request};
use super::frame::{
    Frame,
    RawFrame,
    DataFrame,
    DataFlag,
    HeadersFrame,
    HeadersFlag,
    SettingsFrame,
    HttpSetting,
    unpack_header,
};
use hpack::{Encoder, Decoder};
use hyper::net::{NetworkStream};

/// An enum representing all frame variants that can be returned by an
/// `HttpConnection`.
///
/// The variants wrap the appropriate `Frame` implementation.
#[derive(PartialEq)]
#[derive(Debug)]
pub enum HttpFrame {
    DataFrame(DataFrame),
    HeadersFrame(HeadersFrame),
    SettingsFrame(SettingsFrame),
}

/// Sends the given frame to the peer.
///
/// # Returns
///
/// Any IO errors raised by the underlying transport layer are wrapped in a
/// `HttpError::IoError` variant and propagated upwards.
///
/// If the frame is successfully written, returns a unit Ok (`Ok(())`).
pub fn send_frame<F: Frame>(stream: &mut NetworkStream, frame: F) -> HttpResult<()> {
    // debug!("Sending frame ... {:?}", frame.get_header());
    try!(stream.write_all(&frame.serialize()));
    Ok(())
}

/// Reads a new frame from the transport layer.
///
/// # Returns
///
/// Any IO errors raised by the underlying transport layer are wrapped in a
/// `HttpError::IoError` variant and propagated upwards.
///
/// If the frame type is unknown the `HttpError::UnknownFrameType` variant
/// is returned.
///
/// If the frame type is recognized, but the frame cannot be successfully
/// decoded, the `HttpError::InvalidFrame` variant is returned. For now,
/// invalid frames are not further handled by informing the peer (e.g.
/// sending PROTOCOL_ERROR) nor can the exact reason behind failing to
/// decode the frame be extracted.
///
/// If a frame is successfully read and parsed, returns the frame wrapped
/// in the appropriate variant of the `HttpFrame` enum.
pub fn recv_frame(stream: &mut NetworkStream) -> HttpResult<HttpFrame> {
    let header = unpack_header(&try!(read_header_bytes(stream)));
    // debug!("Received frame header {:?}", header);

    let payload = try!(read_payload(stream, header.0));
    let raw_frame = RawFrame::with_payload(header, payload);

    // TODO: The reason behind being unable to decode the frame should be
    //       extracted and an appropriate connection-level action taken
    //       (e.g. responding with a PROTOCOL_ERROR).
    let frame = match header.1 {
        0x0 => HttpFrame::DataFrame(try!(parse_frame(raw_frame))),
        0x1 => HttpFrame::HeadersFrame(try!(parse_frame(raw_frame))),
        0x4 => HttpFrame::SettingsFrame(try!(parse_frame(raw_frame))),
        _ => return Err(HttpError::UnknownFrameType),
    };

    Ok(frame)
}

/// Reads the header bytes of the next frame from the underlying stream.
///
/// # Returns
///
/// Since each frame header is exactly 9 octets long, returns an array of
/// 9 bytes if the frame header is successfully read.
///
/// Any IO errors raised by the underlying transport layer are wrapped in a
/// `HttpError::IoError` variant and propagated upwards.
fn read_header_bytes(stream: &mut NetworkStream) -> HttpResult<[u8; 9]> {
    let mut buf = [0; 9];
    try!(stream.read_exact(&mut buf));

    Ok(buf)
}

/// Reads the payload of an HTTP/2 frame with the given length.
///
/// # Returns
///
/// A newly allocated buffer containing the entire payload of the frame.
///
/// Any IO errors raised by the underlying transport layer are wrapped in a
/// `HttpError::IoError` variant and propagated upwards.
fn read_payload(stream: &mut NetworkStream, len: u32) -> HttpResult<Vec<u8>> {
    // debug!("Trying to read {} bytes of frame payload", len);
    let length = len as usize;
    let mut buf: Vec<u8> = Vec::with_capacity(length);
    // This is completely safe since we *just* allocated the vector with
    // the same capacity.
    unsafe { buf.set_len(length); }
    try!(stream.read_exact(&mut buf));

    Ok(buf)
}

/// A helper method that parses the given `RawFrame` into the given `Frame`
/// implementation.
///
/// # Returns
///
/// Failing to decode the given `Frame` from the `raw_frame`, an
/// `HttpError::InvalidFrame` error is returned.
#[inline]
fn parse_frame<F: Frame>(raw_frame: RawFrame) -> HttpResult<F> {
    Frame::from_raw(raw_frame).ok_or(HttpError::InvalidFrame)
}


/// Writes the client preface to the underlying HTTP/2 connection.
///
/// According to the HTTP/2 spec, a client preface is first a specific
/// sequence of octets, followed by a settings frame.
///
/// # Returns
/// Any error raised by the underlying connection is propagated.
fn write_preface(stream: &mut NetworkStream) -> HttpResult<()> {
    // The first part of the client preface is always this sequence of 24
    // raw octets.
    let preface = b"PRI * HTTP/2.0\r\n\r\nSM\r\n\r\n";
    try!(stream.write(preface));

    // It is followed by the client's settings.
    let settings = {
        let mut frame = SettingsFrame::new();
        frame.add_setting(HttpSetting::EnablePush(0));
        frame
    };
    try!(send_frame(stream, settings));
    // debug!("Sent client preface");

    Ok(())
}

/// Reads and handles the server preface from the underlying HTTP/2
/// connection.
///
/// According to the HTTP/2 spec, a server preface consists of a single
/// settings frame.
///
/// # Returns
///
/// Any error raised by the underlying connection is propagated.
///
/// Additionally, if it is not possible to decode the server preface,
/// it returns the `HttpError::UnableToConnect` variant.
fn read_preface(stream: &mut NetworkStream) -> HttpResult<()> {
    match recv_frame(stream) {
        Ok(HttpFrame::SettingsFrame(settings)) => {
            // debug!("Correctly received a SETTINGS frame from the server");
            try!(handle_settings_frame(stream, settings));
        },
        // Wrong frame received...
        Ok(_) => return Err(HttpError::UnableToConnect),
        // Already an error -- propagate that.
        Err(e) => return Err(e),
    }
    Ok(())
}

/// A method that sends the given `Request` to the server.
///
/// The method blocks until the entire request has been sent.
///
/// All errors are propagated.
///
/// # Note
///
/// Request body is ignored for now.
pub fn send_request(stream: &mut NetworkStream, encoder: &mut Encoder, req: Request) -> HttpResult<()> {
    let headers_fragment = encoder.encode(&req.headers);
    // For now, sending header fragments larger than 16kB is not supported
    // (i.e. the encoded representation cannot be split into CONTINUATION
    // frames).
    let mut frame = HeadersFrame::new(headers_fragment, req.stream_id);
    frame.set_flag(HeadersFlag::EndHeaders);
    // Since we are not supporting methods which require request bodies to
    // be sent, we end the stream from this side already.
    // TODO: Support bodies!
    frame.set_flag(HeadersFlag::EndStream);

    // Sending this HEADER frame opens the new stream and is equivalent to
    // sending the given request to the server.
    try!(send_frame(stream, frame));

    Ok(())
}

/// Fully handle the next incoming frame, blocking to read it from the
/// underlying transport stream if not available yet.
///
/// All communication errors are propagated.
pub fn handle_next_frame(stream: &mut NetworkStream, session: &mut Session, decoder: &mut Decoder) -> HttpResult<()> {
    // debug!("Waiting for frame...");
    let frame = match recv_frame(stream) {
        Ok(frame) => frame,
        Err(HttpError::UnknownFrameType) => {
            // debug!("Ignoring unknown frame type");
            return Ok(())
        },
        Err(e) => {
            // debug!("Encountered an HTTP/2 error, stopping.");
            return Err(e);
        },
    };

    handle_frame(stream, session, decoder, frame)
}

/// Private helper method that actually handles a received frame.
fn handle_frame(stream: &mut NetworkStream, session: &mut Session, decoder: &mut Decoder, frame: HttpFrame) -> HttpResult<()> {
    match frame {
        HttpFrame::DataFrame(frame) => {
            // debug!("Data frame received");
            handle_data_frame(session, frame)
        },
        HttpFrame::HeadersFrame(frame) => {
            // debug!("Headers frame received");
            handle_headers_frame(session, decoder, frame)
        },
        HttpFrame::SettingsFrame(frame) => {
            // debug!("Settings frame received");
            handle_settings_frame(stream, frame)
        }
    }
}

/// Private helper method that handles a received `DataFrame`.
fn handle_data_frame(session: &mut Session, frame: DataFrame) -> HttpResult<()> {
    session.new_data_chunk(frame.get_stream_id(), &frame.data);

    if frame.is_set(DataFlag::EndStream) {
        // debug!("End of stream {}", frame.get_stream_id());
        session.end_of_stream(frame.get_stream_id())
    }

    Ok(())
}

/// Private helper method that handles a received `HeadersFrame`.
fn handle_headers_frame(session: &mut Session, decoder: &mut Decoder, frame: HeadersFrame) -> HttpResult<()> {
    let headers = try!(decoder.decode(&frame.header_fragment)
                                   .map_err(|e| HttpError::CompressionError(e)));
    session.new_headers(frame.get_stream_id(), headers);

    if frame.is_end_of_stream() {
        // debug!("End of stream {}", frame.get_stream_id());
        session.end_of_stream(frame.get_stream_id());
    }

    Ok(())
}

/// Private helper method that handles a received `SettingsFrame`.
fn handle_settings_frame(stream: &mut NetworkStream, frame: SettingsFrame) -> HttpResult<()> {
    if !frame.is_ack() {
        // TODO: Actually handle the settings change before
        //       sending out the ACK.
        // debug!("Sending a SETTINGS ack");
        try!(send_frame(stream, SettingsFrame::new_ack()));
    }

    Ok(())
}
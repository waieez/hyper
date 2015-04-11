use http2::session::{Session, DefaultSession, Stream};
use http2::{Config, StreamId}; 

// perhaps should live in http2

/// Internal helper method that initializes a new stream and returns its
/// ID once done.
pub fn new_stream(config: &mut Config) -> StreamId {
    //should probably be in session.rs
    let stream_id = get_next_stream_id(config);
    //defaultsession?
    config.session.new_stream(stream_id);

    stream_id
}

/// Internal helper method that gets the next valid stream ID number.
pub fn get_next_stream_id(config: &mut Config) -> StreamId {
    let ret = config.next_stream_id;
    config.next_stream_id += 2;
    
    ret
}
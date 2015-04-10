//! Client Requests
use std::marker::PhantomData;
use std::io::{self, Write, BufWriter};

use url::Url;

use method::{self, Method};
use header::Headers;
use header::{self, Host};
use net::{NetworkStream, NetworkConnector, HttpConnector, Fresh, Streaming};
use http::{HttpWriter, LINE_ENDING};
use http::HttpWriter::{ThroughWriter, ChunkedWriter, SizedWriter, EmptyWriter};
use version;
use HttpResult;
use client::{Response, get_host_and_port};


/// A client request to a remote server.
pub struct Request<W> {
    /// The target URI for this request.
    pub url: Url,

    /// The HTTP version of this request.
    pub version: version::HttpVersion,

    body: HttpWriter<BufWriter<Box<NetworkStream + Send>>>,
    headers: Headers,
    method: method::Method,

    _marker: PhantomData<W>,
}

impl<W> Request<W> {
    /// Read the Request headers.
    #[inline]
    pub fn headers(&self) -> &Headers { &self.headers }

    /// Read the Request method.
    #[inline]
    pub fn method(&self) -> method::Method { self.method.clone() }
}

impl Request<Fresh> {
    /// Create a new client request.
    pub fn new(method: method::Method, url: Url) -> HttpResult<Request<Fresh>> {
        let mut conn = HttpConnector(None);
        let mut version = version::HttpVersion::Http11;
        Request::with_connector(method, url, &mut conn, &mut version) //http2:flag for http11
    }

    /// Create a new client request with a specific underlying NetworkStream.
    pub fn with_connector<C, S>(method: method::Method, url: Url, connector: &mut C, version: &mut version::HttpVersion)
        -> HttpResult<Request<Fresh>> where
        C: NetworkConnector<Stream=S>,
        S: Into<Box<NetworkStream + Send>> {
        debug!("{} {}", method, url);
        let (host, port) = try!(get_host_and_port(&url));

        let stream = try!(connector.connect(&*host, port, &*url.scheme)).into();
        let stream = ThroughWriter(BufWriter::new(stream));

        let mut headers = Headers::new();
        headers.set(Host {
            hostname: host,
            port: Some(port),
        });

        //http2: note
        let protocol = version.clone(); //copy version to request

        Ok(Request {
            method: method,
            headers: headers,
            url: url,
            version: protocol, //change to use protocol var
            body: stream,
            _marker: PhantomData,
        })
    }

    /// Consume a Fresh Request, writing the headers and method,
    /// returning a Streaming Request.
    pub fn start(mut self) -> HttpResult<Request<Streaming>> {
        let mut uri = self.url.serialize_path().unwrap();
        //TODO: this needs a test
        if let Some(ref q) = self.url.query {
            uri.push('?');
            uri.push_str(&q[..]);
        }

        debug!("writing head: {:?} {:?} {:?}", self.method, uri, self.version);
        try!(write!(&mut self.body, "{} {} {}{}",
                    self.method, uri, self.version, LINE_ENDING));


        let stream = match self.method {
            Method::Get | Method::Head => {
                debug!("headers [\n{:?}]", self.headers);
                try!(write!(&mut self.body, "{}{}", self.headers, LINE_ENDING));
                EmptyWriter(self.body.into_inner())
            },
            _ => {
                let mut chunked = true;
                let mut len = 0;

                match self.headers.get::<header::ContentLength>() {
                    Some(cl) => {
                        chunked = false;
                        len = **cl;
                    },
                    None => ()
                };

                // cant do in match above, thanks borrowck
                if chunked {
                    let encodings = match self.headers.get_mut::<header::TransferEncoding>() {
                        Some(&mut header::TransferEncoding(ref mut encodings)) => {
                            //TODO: check if chunked is already in encodings. use HashSet?
                            encodings.push(header::Encoding::Chunked);
                            false
                        },
                        None => true
                    };

                    if encodings {
                        self.headers.set::<header::TransferEncoding>(
                            header::TransferEncoding(vec![header::Encoding::Chunked]))
                    }
                }

                debug!("headers [\n{:?}]", self.headers);
                try!(write!(&mut self.body, "{}{}", self.headers, LINE_ENDING));

                if chunked {
                    ChunkedWriter(self.body.into_inner())
                } else {
                    SizedWriter(self.body.into_inner(), len)
                }
            }
        };

        Ok(Request {
            method: self.method,
            headers: self.headers,
            url: self.url,
            version: self.version,
            body: stream,
            _marker: PhantomData,
        })
    }

    /// Get a mutable reference to the Request headers.
    #[inline]
    pub fn headers_mut(&mut self) -> &mut Headers { &mut self.headers }
}

impl Request<Streaming> {
    /// Completes writing the request, and returns a response to read from.
    ///
    /// Consumes the Request.
    pub fn send(self) -> HttpResult<Response> {
        //http2: check version if http2, call sendhttp2 privatefunc. raw is the stream
        
        let raw = try!(self.body.end()).into_inner().unwrap(); // end() already flushes
        Response::new(raw)
    }

    // http2: note client::send is called to get response
    // fn sendhttp2 (&mut self) -> HttpStream? {
        // init
        //write_preface(stream: &mut NetworkStream);
        //read_preface(stream: &mut NetworkStream);

        //if success
            // send_request
            // first destructure Fresh Request
            // let scheme = b"http".to_vec();
            // let host = self.host.clone();

            // let stream_id = self.new_stream();

            // let mut headers: Vec<Header> = vec![
            //     (b":method".to_vec(), method.to_vec()),
            //     (b":path".to_vec(), path.to_vec()),
            //     (b":authority".to_vec(), host),
            //     (b":scheme".to_vec(), scheme),
            // ];
            // where does the body go?

            //recreate request object
            //extract stream from request
            //send_request(stream: &mut NetworkStream, encoder: &mut Encoder, req: Request)
            // try!(self.conn.send_request(Request {
            //     stream_id: stream_id,
            //     headers: headers,
            //     body: Vec::new(),
            // }));
        //else
            //send as http1
    // }
}

impl Write for Request<Streaming> {
    #[inline]
    fn write(&mut self, msg: &[u8]) -> io::Result<usize> {
        self.body.write(msg)
    }

    #[inline]
    fn flush(&mut self) -> io::Result<()> {
        self.body.flush()
    }
}

#[cfg(test)]
mod tests {
    use std::str::from_utf8;
    use url::Url;
    use method::Method::{Get, Head};
    use mock::{MockStream, MockConnector};
    use super::Request;
    use version::HttpVersion::{Http11}; //http2: TODO: add tests for http2 splits

    #[test]
    fn test_get_empty_body() {
        let req = Request::with_connector(
            Get, Url::parse("http://example.dom").unwrap(), &mut MockConnector, &mut Http11
        ).unwrap();
        let req = req.start().unwrap();
        let stream = *req.body.end().unwrap()
            .into_inner().unwrap().downcast::<MockStream>().ok().unwrap();
        let bytes = stream.write;
        let s = from_utf8(&bytes[..]).unwrap();
        assert!(!s.contains("Content-Length:"));
        assert!(!s.contains("Transfer-Encoding:"));
    }

    #[test]
    fn test_head_empty_body() {
        let req = Request::with_connector(
            Head, Url::parse("http://example.dom").unwrap(), &mut MockConnector, &mut Http11
        ).unwrap();
        let req = req.start().unwrap();
        let stream = *req.body.end().unwrap()
            .into_inner().unwrap().downcast::<MockStream>().ok().unwrap();
        let bytes = stream.write;
        let s = from_utf8(&bytes[..]).unwrap();
        assert!(!s.contains("Content-Length:"));
        assert!(!s.contains("Transfer-Encoding:"));
    }
}

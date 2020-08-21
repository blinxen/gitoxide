use crate::fixture_bytes;
use bstr::ByteVec;
use std::io::{Read, Write};
use std::net::SocketAddr;
use std::time::Duration;

struct MockServer {
    addr: SocketAddr,
    thread: Option<std::thread::JoinHandle<Vec<u8>>>,
}

impl MockServer {
    pub fn new(fixture: Vec<u8>) -> Self {
        let ports = (15411..).take(10);
        let listener = std::net::TcpListener::bind(
            ports
                .map(|port| SocketAddr::from(([127, 0, 0, 1], port)))
                .collect::<Vec<_>>()
                .as_slice(),
        )
        .expect("one of these ports to be free");
        let addr = listener.local_addr().expect("a local address");
        MockServer {
            addr,
            thread: Some(std::thread::spawn(move || {
                let (mut stream, _) = listener.accept().expect("accept to always work");
                stream
                    .set_read_timeout(Some(Duration::from_millis(50)))
                    .expect("timeout to always work");
                stream
                    .set_write_timeout(Some(Duration::from_millis(50)))
                    .expect("timeout to always work");
                let mut out = Vec::new();
                stream.read_to_end(&mut out).expect("reading to always work");
                stream.write_all(&fixture).expect("write to always work");
                out
            })),
        }
    }

    pub fn received(&mut self) -> Vec<u8> {
        self.thread
            .take()
            .and_then(|h| h.join().ok())
            .expect("join to be called only once")
    }

    pub fn addr(&self) -> &SocketAddr {
        &self.addr
    }

    pub fn received_as_string(&mut self) -> String {
        self.received().into_string().expect("utf8 only")
    }
}

fn serve_once(name: &str) -> MockServer {
    MockServer::new(fixture_bytes(name))
}

mod upload_pack {
    use crate::client::http::serve_once;
    use git_transport::{client::TransportSketch, Protocol, Service};

    #[test]
    #[ignore]
    fn clone_v1() -> crate::Result {
        let mut server = serve_once("v1/http-handshake.response");
        let mut c = git_transport::client::http::connect(
            &format!(
                "http://{}/path/not/important/due/to/mock",
                &server.addr().ip().to_string()
            ),
            Protocol::V1,
        )?;
        let _response = c.set_service(Service::UploadPack)?;
        assert_eq!(&server.received_as_string(), "hello");
        Ok(())
    }
}

use std::net::UdpSocket;

pub struct PbxHarness {
    pub port: u16,
    _socket: UdpSocket,
}

impl PbxHarness {
    pub fn new() -> Self {
        let socket = UdpSocket::bind("127.0.0.1:0").expect("failed to bind PBX socket");
        let port = socket.local_addr().unwrap().port();
        Self {
            port,
            _socket: socket,
        }
    }
}

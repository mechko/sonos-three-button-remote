use crate::sonos::{location, TextBuffer, SEARCH};
use crate::wifi::current_millis;
use core::fmt::Write;
use smoltcp::{
    iface::{Interface, SocketHandle, SocketSet},
    phy::Device,
    socket::{dhcpv4, tcp, udp, Socket},
    time::{Duration, Instant},
    wire::{IpAddress, IpCidr, Ipv4Address},
};

#[derive(Clone, Copy)]
pub struct Candidate {
    pub ip: Ipv4Address,
    pub port: u16,
    path: [u8; 160],
    path_len: usize,
}

#[derive(Debug)]
pub enum ControlError {
    Network(&'static str),
    Rejected { status: u16, code: Option<u16> },
}

impl core::fmt::Display for ControlError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::Network(error) => f.write_str(error),
            Self::Rejected {
                status,
                code: Some(code),
            } => write!(f, "Sonos error {code}\nHTTP {status}"),
            Self::Rejected { status, code: None } => write!(f, "HTTP {status}"),
        }
    }
}

pub struct Network<'a, D> {
    pub iface: Interface,
    pub device: D,
    pub sockets: SocketSet<'a>,
    pub udp: SocketHandle,
    pub tcp: SocketHandle,
    pub next_port: u16,
}

impl<D: Device> Network<'_, D> {
    pub fn poll(&mut self) {
        self.iface.poll(
            Instant::from_millis(current_millis() as i64),
            &mut self.device,
            &mut self.sockets,
        );
        for (_, socket) in self.sockets.iter_mut() {
            if let Socket::Dhcpv4(socket) = socket {
                match socket.poll() {
                    Some(dhcpv4::Event::Configured(config)) => {
                        self.iface.update_ip_addrs(|addresses| {
                            addresses.clear();
                            let _ = addresses.push(IpCidr::Ipv4(config.address));
                        });
                        self.iface.routes_mut().remove_default_ipv4_route();
                        if let Some(router) = config.router {
                            let _ = self.iface.routes_mut().add_default_ipv4_route(router);
                        }
                    }
                    Some(dhcpv4::Event::Deconfigured) => {
                        self.iface.update_ip_addrs(|addresses| addresses.clear());
                        self.iface.routes_mut().remove_default_ipv4_route();
                    }
                    None => {}
                }
            }
        }
    }
    pub fn ip(&self) -> Option<Ipv4Address> {
        self.iface
            .ip_addrs()
            .iter()
            .find_map(|address| match address {
                IpCidr::Ipv4(cidr) if !cidr.address().is_unspecified() => Some(cidr.address()),
                _ => None,
            })
    }
    pub fn reset_dhcp(&mut self) {
        for (_, socket) in self.sockets.iter_mut() {
            if let Socket::Dhcpv4(socket) = socket {
                socket.reset();
            }
        }
        self.iface.update_ip_addrs(|addresses| addresses.clear());
        self.iface.routes_mut().remove_default_ipv4_route();
    }
    #[allow(dead_code)] // Used by the standalone discovery example.
    pub fn wait(&mut self, milliseconds: u64) {
        self.wait_with_tick(milliseconds, || {});
    }
    pub fn wait_with_tick(&mut self, milliseconds: u64, mut tick: impl FnMut()) {
        let end = current_millis() + milliseconds;
        while current_millis() < end {
            self.poll();
            tick();
        }
    }
    #[allow(dead_code)] // Used by the standalone discovery example.
    pub fn discover(&mut self) -> ([Option<Candidate>; 16], usize) {
        self.discover_with_tick(|| {})
    }
    pub fn discover_with_tick(
        &mut self,
        mut tick: impl FnMut(),
    ) -> ([Option<Candidate>; 16], usize) {
        let mut candidates = [None; 16];
        let mut count = 0;
        let socket = self.sockets.get_mut::<udp::Socket>(self.udp);
        while socket.can_recv() {
            let _ = socket.recv();
        }
        let end = current_millis() + 8000;
        let mut next_send = 0;
        while current_millis() < end {
            self.poll();
            tick();
            let socket = self.sockets.get_mut::<udp::Socket>(self.udp);
            if current_millis() >= next_send && socket.can_send() {
                let _ = socket.send_slice(SEARCH, (Ipv4Address::new(239, 255, 255, 250), 1900));
                next_send = current_millis() + 2000;
            }
            while let Ok((bytes, metadata)) = socket.recv() {
                let Ok(response) = core::str::from_utf8(bytes) else {
                    continue;
                };
                let Some(url) = location(response) else {
                    continue;
                };
                let ip = Ipv4Address::from(url.ip);
                if metadata.endpoint.addr != IpAddress::Ipv4(ip) {
                    continue;
                }
                if count == candidates.len()
                    || candidates[..count]
                        .iter()
                        .flatten()
                        .any(|c: &Candidate| c.ip == ip && c.port == url.port)
                {
                    continue;
                }
                let mut path = [0; 160];
                path[..url.path.len()].copy_from_slice(url.path.as_bytes());
                candidates[count] = Some(Candidate {
                    ip,
                    port: url.port,
                    path,
                    path_len: url.path.len(),
                });
                count += 1;
            }
        }
        (candidates, count)
    }
    #[allow(dead_code)] // Used by the standalone discovery example.
    pub fn fetch<'b>(
        &mut self,
        candidate: Candidate,
        response: &'b mut [u8],
    ) -> Result<&'b str, &'static str> {
        self.fetch_with_tick(candidate, response, || {})
    }
    pub fn fetch_with_tick<'b>(
        &mut self,
        candidate: Candidate,
        response: &'b mut [u8],
        tick: impl FnMut(),
    ) -> Result<&'b str, &'static str> {
        let mut request = TextBuffer::<384>::new();
        let path =
            core::str::from_utf8(&candidate.path[..candidate.path_len]).map_err(|_| "Bad URL")?;
        write!(
            request,
            "GET {path} HTTP/1.0\r\nHost: {}:{}\r\nConnection: close\r\n\r\n",
            candidate.ip, candidate.port
        )
        .map_err(|_| "URL too long")?;
        let received = self.exchange(candidate, request.as_str(), response, tick)?;
        crate::sonos::http_body(&mut response[..received])
    }

    pub fn soap_with_tick<'b>(
        &mut self,
        candidate: Candidate,
        action: crate::sonos::Action,
        response: &'b mut [u8],
        tick: impl FnMut(),
    ) -> Result<&'b str, ControlError> {
        let mut host = TextBuffer::<64>::new();
        write!(host, "{}:{}", candidate.ip, candidate.port)
            .map_err(|_| ControlError::Network("Host too long"))?;
        let url = match action {
            crate::sonos::Action::SetRadio(station) => station.url(),
            _ => crate::sonos::STATIONS[0].stream_url,
        };
        let request = crate::sonos::soap_request(action, host.as_str(), url)
            .map_err(|_| ControlError::Network("Request too large"))?;
        let received = self
            .exchange(candidate, request.as_str(), response, tick)
            .map_err(ControlError::Network)?;
        let decoded =
            crate::sonos::decode_http(&mut response[..received]).map_err(ControlError::Network)?;
        let code = crate::sonos::xml_value(decoded.body, "errorCode").and_then(|v| v.parse().ok());
        if decoded.status != 200 || code.is_some() {
            return Err(ControlError::Rejected {
                status: decoded.status,
                code,
            });
        }
        let mut expected = TextBuffer::<64>::new();
        let _ = write!(expected, "{}Response", action.name());
        if !decoded.body.contains(expected.as_str()) {
            return Err(ControlError::Network("Invalid SOAP reply"));
        }
        Ok(decoded.body)
    }

    fn exchange(
        &mut self,
        candidate: Candidate,
        request: &str,
        response: &mut [u8],
        mut tick: impl FnMut(),
    ) -> Result<usize, &'static str> {
        let socket = self.sockets.get_mut::<tcp::Socket>(self.tcp);
        socket.abort();
        socket.set_timeout(Some(Duration::from_secs(5)));
        let port = self.next_port;
        self.next_port = if port == 65535 { 49153 } else { port + 1 };
        socket
            .connect(self.iface.context(), (candidate.ip, candidate.port), port)
            .map_err(|_| "TCP connect")?;
        let deadline = current_millis() + 6000;
        let mut sent = 0;
        let mut received = 0;
        let result = (|| loop {
            self.poll();
            tick();
            let socket = self.sockets.get_mut::<tcp::Socket>(self.tcp);
            if socket.can_send() && sent < request.len() {
                sent += socket
                    .send_slice(&request.as_bytes()[sent..])
                    .map_err(|_| "HTTP send")?;
            }
            if socket.can_recv() {
                if received == response.len() {
                    break Err("Reply too large");
                }
                received += socket
                    .recv_slice(&mut response[received..])
                    .map_err(|_| "HTTP receive")?;
            }
            if received > 0 && !socket.may_recv() && !socket.can_recv() {
                break Ok(());
            }
            if socket.state() == tcp::State::Closed {
                break Err("TCP closed");
            }
            if current_millis() >= deadline {
                break Err("HTTP timeout");
            }
        })();
        self.sockets.get_mut::<tcp::Socket>(self.tcp).abort();
        result?;
        Ok(received)
    }
}

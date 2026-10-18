//! Host regression checks against the firmware's actual network module.
#![cfg(test)]
#![allow(dead_code)]

#[path = "../../src/button.rs"]
mod button;
#[path = "../../src/led.rs"]
mod led;
#[path = "../../src/network.rs"]
mod network;
#[path = "../../src/sonos.rs"]
mod sonos;

mod wifi {
    use std::cell::Cell;
    thread_local! { pub static NOW: Cell<u64> = const { Cell::new(0) }; }
    pub fn current_millis() -> u64 {
        NOW.with(Cell::get)
    }
}

use smoltcp::{
    iface::{Config, Interface, SocketSet, SocketStorage},
    phy::{ChecksumCapabilities, Device, DeviceCapabilities, Medium, RxToken, TxToken},
    socket::{dhcpv4, tcp, udp},
    time::Instant,
    wire::{
        DhcpMessageType, DhcpPacket, DhcpRepr, EthernetAddress, EthernetFrame, EthernetProtocol,
        EthernetRepr, IpProtocol, Ipv4Address, Ipv4Packet, Ipv4Repr, UdpPacket, UdpRepr,
    },
};
use std::collections::VecDeque;

#[derive(Default)]
struct Wire {
    rx: VecDeque<Vec<u8>>,
    tx: VecDeque<Vec<u8>>,
}
struct Rx(Vec<u8>);
struct Tx<'a>(&'a mut VecDeque<Vec<u8>>);
impl Device for Wire {
    type RxToken<'a> = Rx;
    type TxToken<'a> = Tx<'a>;
    fn receive(&mut self, _: Instant) -> Option<(Rx, Tx<'_>)> {
        self.rx
            .pop_front()
            .map(|packet| (Rx(packet), Tx(&mut self.tx)))
    }
    fn transmit(&mut self, _: Instant) -> Option<Tx<'_>> {
        Some(Tx(&mut self.tx))
    }
    fn capabilities(&self) -> DeviceCapabilities {
        let mut caps = DeviceCapabilities::default();
        caps.medium = Medium::Ethernet;
        caps.max_transmission_unit = 1492;
        caps
    }
}
impl RxToken for Rx {
    fn consume<R, F: FnOnce(&[u8]) -> R>(self, f: F) -> R {
        f(&self.0)
    }
}
impl TxToken for Tx<'_> {
    fn consume<R, F: FnOnce(&mut [u8]) -> R>(self, len: usize, f: F) -> R {
        let mut packet = vec![0; len];
        let result = f(&mut packet);
        self.0.push_back(packet);
        result
    }
}

fn dhcp_payload(frame: &[u8]) -> &[u8] {
    // These assertions also catch malformed framing in client-generated requests.
    let ethernet = EthernetFrame::new_checked(frame).unwrap();
    assert_eq!(ethernet.ethertype(), EthernetProtocol::Ipv4);
    let ip = Ipv4Packet::new_checked(ethernet.payload()).unwrap();
    assert!(ip.verify_checksum());
    let udp = UdpPacket::new_checked(ip.payload()).unwrap();
    assert!(udp.verify_checksum(&ip.src_addr().into(), &ip.dst_addr().into()));
    // All packets emitted in this fixture use an ordinary 20-byte IPv4 header.
    &frame[14 + ip.header_len() as usize + 8..14 + ip.total_len() as usize]
}

fn message(frame: &[u8]) -> (DhcpMessageType, u32) {
    let packet = DhcpPacket::new_checked(dhcp_payload(frame)).unwrap();
    let repr = DhcpRepr::parse(&packet).unwrap();
    (repr.message_type, repr.transaction_id)
}

fn server_reply(request: &[u8], kind: DhcpMessageType) -> Vec<u8> {
    let packet = DhcpPacket::new_checked(dhcp_payload(request)).unwrap();
    let mut dhcp = DhcpRepr::parse(&packet).unwrap();
    dhcp.message_type = kind;
    dhcp.your_ip = Ipv4Address::new(192, 0, 2, 42);
    dhcp.server_identifier = Some(Ipv4Address::new(192, 0, 2, 1));
    dhcp.subnet_mask = Some(Ipv4Address::new(255, 255, 255, 0));
    dhcp.router = dhcp.server_identifier;
    dhcp.lease_duration = Some(3600);
    dhcp.requested_ip = None;
    dhcp.parameter_request_list = None;
    dhcp.client_identifier = None;
    dhcp.max_size = None;
    let ip = Ipv4Repr {
        src_addr: dhcp.server_identifier.unwrap(),
        dst_addr: Ipv4Address::BROADCAST,
        next_header: IpProtocol::Udp,
        payload_len: 8 + dhcp.buffer_len(),
        hop_limit: 64,
    };
    let mut reply = vec![0; 14 + 20 + ip.payload_len];
    let mut ethernet = EthernetFrame::new_unchecked(&mut reply[..]);
    EthernetRepr {
        src_addr: EthernetAddress([2, 0, 0, 0, 0, 1]),
        dst_addr: EthernetAddress::BROADCAST,
        ethertype: EthernetProtocol::Ipv4,
    }
    .emit(&mut ethernet);
    let mut ipv4 = Ipv4Packet::new_unchecked(ethernet.payload_mut());
    ip.emit(&mut ipv4, &ChecksumCapabilities::default());
    UdpRepr {
        src_port: 67,
        dst_port: 68,
    }
    .emit(
        &mut UdpPacket::new_unchecked(ipv4.payload_mut()),
        &ip.src_addr.into(),
        &ip.dst_addr.into(),
        dhcp.buffer_len(),
        |payload| dhcp.emit(&mut DhcpPacket::new_unchecked(payload)).unwrap(),
        &ChecksumCapabilities::default(),
    );
    reply
}

#[test]
fn delayed_dhcp_keeps_transaction_id_assigns_address_and_resets_on_link_loss() {
    wifi::NOW.with(|now| now.set(0));
    let mut device = Wire::default();
    let mut config = Config::new(EthernetAddress([2, 0, 0, 0, 0, 2]).into());
    config.random_seed = 12345;
    let iface = Interface::new(config, &mut device, Instant::from_millis(0));
    let mut storage: [SocketStorage; 3] = Default::default();
    let mut sockets = SocketSet::new(&mut storage[..]);
    sockets.add(dhcpv4::Socket::new());
    let mut rx_meta = [udp::PacketMetadata::EMPTY; 1];
    let mut tx_meta = [udp::PacketMetadata::EMPTY; 1];
    let mut udp_rx = [0; 512];
    let mut udp_tx = [0; 512];
    let mut tcp_rx = [0; 512];
    let mut tcp_tx = [0; 512];
    let udp = sockets.add(udp::Socket::new(
        udp::PacketBuffer::new(&mut rx_meta[..], &mut udp_rx[..]),
        udp::PacketBuffer::new(&mut tx_meta[..], &mut udp_tx[..]),
    ));
    let tcp = sockets.add(tcp::Socket::new(
        tcp::SocketBuffer::new(&mut tcp_rx[..]),
        tcp::SocketBuffer::new(&mut tcp_tx[..]),
    ));
    let mut network = network::Network {
        iface,
        device,
        sockets,
        udp,
        tcp,
        next_port: 49153,
    };
    network.poll();
    assert_eq!(
        message(&network.device.tx.pop_front().unwrap()).0,
        DhcpMessageType::Discover
    );

    // The server is silent for longer than the former 20-second Wi-Fi cutoff.
    wifi::NOW.with(|now| now.set(25_000));
    network.poll();
    let discover = network.device.tx.pop_front().unwrap();
    let (_, xid) = message(&discover);
    network
        .device
        .rx
        .push_back(server_reply(&discover, DhcpMessageType::Offer));
    network.poll();
    let request = network.device.tx.pop_front().unwrap();
    assert_eq!(message(&request), (DhcpMessageType::Request, xid));

    // A delayed ACK requires a REQUEST retry with the very same transaction ID.
    wifi::NOW.with(|now| now.set(50_000));
    network.poll();
    let retry = network.device.tx.pop_front().unwrap();
    assert_eq!(message(&retry), (DhcpMessageType::Request, xid));
    network
        .device
        .rx
        .push_back(server_reply(&retry, DhcpMessageType::Ack));
    network.poll();
    assert_eq!(network.ip(), Some(Ipv4Address::new(192, 0, 2, 42)));

    network.reset_dhcp();
    assert_eq!(network.ip(), None);
    network.poll();
    assert_eq!(
        message(&network.device.tx.pop_front().unwrap()).0,
        DhcpMessageType::Discover
    );
}

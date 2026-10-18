#!/usr/bin/env python3
"""Discover Sonos speakers on the Mac's current LAN using SSDP."""

import argparse
import concurrent.futures
import ipaddress
import re
import socket
import subprocess
import sys
import time
import unicodedata
import urllib.error
import urllib.parse
import urllib.request
import xml.etree.ElementTree as ET


def normalized(value):
    return unicodedata.normalize("NFC", value).casefold()


def mac_lan():
    """Find an active Ethernet/Wi-Fi interface, excluding VPN interfaces."""
    try:
        output = subprocess.check_output(["/sbin/ifconfig"], text=True, stderr=subprocess.DEVNULL)
    except (OSError, subprocess.CalledProcessError):
        return None, None
    for block in re.split(r"(?=^\S+: flags=)", output, flags=re.MULTILINE):
        if not re.match(r"en\d+:", block) or "status: active" not in block:
            continue
        match = re.search(r"\binet ([\d.]+) netmask (0x[0-9a-fA-F]+|[\d.]+)", block)
        if match:
            address, mask = match.groups()
            if mask.startswith("0x"):
                mask = str(ipaddress.IPv4Address(int(mask, 16)))
            return address, ipaddress.IPv4Network(f"{address}/{mask}", strict=False)
    return None, None


def scan_http(network):
    def probe(host):
        ip = str(host)
        try:
            with socket.create_connection((ip, 1400), timeout=0.5):
                return ip, f"http://{ip}:1400/xml/device_description.xml"
        except OSError:
            return None
    with concurrent.futures.ThreadPoolExecutor(max_workers=32) as pool:
        return dict(result for result in pool.map(probe, network.hosts()) if result)


def discover(timeout, interface=None):
    message = (
        'M-SEARCH * HTTP/1.1\r\nHOST: 239.255.255.250:1900\r\n'
        'MAN: "ssdp:discover"\r\nMX: 2\r\n'
        'ST: urn:schemas-upnp-org:device:ZonePlayer:1\r\n\r\n'
    ).encode("ascii")
    locations = {}
    with socket.socket(socket.AF_INET, socket.SOCK_DGRAM) as sock:
        sock.bind((interface or "", 0))
        if interface:
            sock.setsockopt(socket.IPPROTO_IP, socket.IP_MULTICAST_IF, socket.inet_aton(interface))
        sock.setsockopt(socket.IPPROTO_IP, socket.IP_MULTICAST_TTL, 2)
        deadline = time.monotonic() + timeout
        next_send = 0
        while time.monotonic() < deadline:
            now = time.monotonic()
            if now >= next_send:
                sock.sendto(message, ("239.255.255.250", 1900))
                next_send = now + 2
            sock.settimeout(min(0.5, max(0.001, deadline - now)))
            try:
                data, peer = sock.recvfrom(65535)
            except socket.timeout:
                continue
            lines = data.decode("utf-8", errors="replace").splitlines()
            if not lines or not lines[0].startswith("HTTP/1.1 200"):
                continue
            headers = {}
            for line in lines[1:]:
                key, separator, value = line.partition(":")
                if separator:
                    headers[key.strip().lower()] = value.strip()
            location = headers.get("location", "")
            url = urllib.parse.urlsplit(location)
            # Fetch only HTTP device descriptions hosted by the responding peer.
            if url.scheme == "http" and url.hostname == peer[0]:
                locations[peer[0]] = location
    return locations


def speaker_info(ip, location):
    # Local traffic should bypass any configured HTTP proxy.
    opener = urllib.request.build_opener(urllib.request.ProxyHandler({}))
    with opener.open(location, timeout=4) as response:
        root = ET.fromstring(response.read(1024 * 1024))
    fields = {}
    for element in root.iter():
        tag = element.tag.rsplit("}", 1)[-1]
        if element.text:
            fields.setdefault(tag, element.text.strip())
    return {
        "ip": ip,
        "name": fields.get("roomName", fields.get("friendlyName", "Unknown")),
        "model": fields.get("modelName", "Unknown"),
        "uid": fields.get("UDN", ""),
        "location": location,
    }


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--name", default="Sonos Küche")
    parser.add_argument("--timeout", type=float, default=8)
    parser.add_argument("--interface", help="Mac's LAN IPv4 address for multicast discovery")
    parser.add_argument("--subnet", help="LAN subnet for HTTP fallback, e.g. 192.168.1.0/24")
    args = parser.parse_args()
    if args.timeout <= 0:
        parser.error("--timeout must be positive")
    interface, network = mac_lan()
    try:
        if args.interface:
            interface = str(ipaddress.IPv4Address(args.interface))
            if network and ipaddress.IPv4Address(interface) not in network:
                network = None
        if args.subnet:
            network = ipaddress.IPv4Network(args.subnet, strict=False)
    except ValueError as error:
        parser.error(str(error))
    print(f"Searching the current LAN for {args.name!r}…", flush=True)
    if interface:
        print(f"Using LAN address {interface}", flush=True)
    try:
        locations = discover(args.timeout, interface)
    except OSError as error:
        print(f"Multicast discovery failed: {error}", file=sys.stderr)
        locations = {}
        if error.errno in (1, 13):
            return 1
    if not locations and network:
        if network.num_addresses > 1024:
            print("LAN is too large for automatic scanning. Supply a smaller --subnet.", file=sys.stderr)
            return 1
        print(f"Trying Sonos HTTP discovery on {network}…", flush=True)
        locations = scan_http(network)
    elif not locations and not network:
        print("Could not determine the LAN subnet. Supply --interface and --subnet.", file=sys.stderr)
    found = False
    for ip, location in sorted(locations.items()):
        try:
            speaker = speaker_info(ip, location)
        except (OSError, ValueError, urllib.error.URLError, ET.ParseError) as error:
            print(f"Could not inspect {ip}: {error}", file=sys.stderr)
            continue
        match = normalized(speaker["name"]) == normalized(args.name)
        print(f"{'MATCH' if match else 'Speaker'}: {speaker['name']} | "
              f"{speaker['model']} | {ip}")
        if match:
            found = True
            print(f"  Device description: {location}")
            print(f"  Device ID: {speaker['uid']}")
    if not found:
        print("Target not found. Check that the Mac and speaker are on the same "
              "LAN and multicast discovery is allowed.")
    return 0 if found else 2


if __name__ == "__main__":
    sys.exit(main())

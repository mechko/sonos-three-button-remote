use core::fmt;

/// One compile-time button preset. Public URLs are examples, not credentials.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Station {
    pub name: &'static str,
    pub stream_url: &'static str,
}

// Standalone host tests always use public example settings.
#[cfg(test)]
include!("../remote.example.rs");
#[cfg(not(test))]
include!(concat!(env!("OUT_DIR"), "/remote.rs"));

impl Station {
    pub const ALL: [Self; 3] = STATIONS;
    pub fn title(self) -> &'static str {
        self.name
    }
    pub fn url(self) -> &'static str {
        self.stream_url
    }
}

pub const SEARCH: &[u8] = b"M-SEARCH * HTTP/1.1\r\nHOST: 239.255.255.250:1900\r\nMAN: \"ssdp:discover\"\r\nMX: 2\r\nST: urn:schemas-upnp-org:device:ZonePlayer:1\r\n\r\n";

pub struct TextBuffer<const N: usize> {
    bytes: [u8; N],
    len: usize,
}

impl<const N: usize> TextBuffer<N> {
    pub const fn new() -> Self {
        Self {
            bytes: [0; N],
            len: 0,
        }
    }
    pub fn as_str(&self) -> &str {
        core::str::from_utf8(&self.bytes[..self.len]).unwrap()
    }
}

impl<const N: usize> fmt::Write for TextBuffer<N> {
    fn write_str(&mut self, value: &str) -> fmt::Result {
        if self.len + value.len() > N {
            return Err(fmt::Error);
        }
        self.bytes[self.len..self.len + value.len()].copy_from_slice(value.as_bytes());
        self.len += value.len();
        Ok(())
    }
}

#[derive(Debug, PartialEq)]
pub struct Location<'a> {
    pub ip: [u8; 4],
    pub port: u16,
    pub path: &'a str,
}

pub fn location(response: &str) -> Option<Location<'_>> {
    if !response.lines().next()?.starts_with("HTTP/1.1 200") {
        return None;
    }
    let url = response.lines().skip(1).find_map(|line| {
        let (key, value) = line.split_once(':')?;
        key.trim()
            .eq_ignore_ascii_case("location")
            .then_some(value.trim())
    })?;
    let url = url.strip_prefix("http://")?;
    let (host, path) = url.split_once('/')?;
    let (host, port) = host.split_once(':').unwrap_or((host, "80"));
    let mut ip = [0; 4];
    let mut parts = host.split('.');
    for octet in &mut ip {
        *octet = parts.next()?.parse().ok()?;
    }
    if parts.next().is_some() || path.len() > 150 || path.contains(['\r', '\n', ' ']) {
        return None;
    }
    let port = port.parse().ok()?;
    if port == 0 {
        return None;
    }
    // Include the slash without allocating.
    let path = &url[url.find('/')?..];
    Some(Location { ip, port, path })
}

pub fn xml_value<'a>(xml: &'a str, tag: &str) -> Option<&'a str> {
    let mut opening = TextBuffer::<64>::new();
    let mut closing = TextBuffer::<64>::new();
    use fmt::Write;
    write!(opening, "<{tag}>").ok()?;
    write!(closing, "</{tag}>").ok()?;
    let start = xml.find(opening.as_str())? + opening.as_str().len();
    let rest = &xml[start..];
    Some(rest[..rest.find(closing.as_str())?].trim())
}

pub fn xml_text_equals(mut value: &str, expected: &str) -> bool {
    let mut chars = expected.chars();
    while !value.is_empty() {
        let next = if let Some(entity) = value.strip_prefix('&') {
            let Some(end) = entity.find(';') else {
                return false;
            };
            let token = &entity[..end];
            value = &entity[end + 1..];
            match token {
                "amp" => Some('&'),
                "lt" => Some('<'),
                "gt" => Some('>'),
                "quot" => Some('"'),
                "apos" => Some('\''),
                _ => {
                    let number = if let Some(hex) = token.strip_prefix("#x") {
                        u32::from_str_radix(hex, 16).ok()
                    } else {
                        token.strip_prefix('#').and_then(|v| v.parse().ok())
                    };
                    number.and_then(char::from_u32)
                }
            }
        } else {
            let ch = value.chars().next().unwrap();
            value = &value[ch.len_utf8()..];
            Some(ch)
        };
        if next.is_none() || chars.next() != next {
            return false;
        }
    }
    chars.next().is_none()
}

pub fn is_target(xml: &str) -> bool {
    let name = xml_value(xml, "roomName").unwrap_or("");
    let model = xml_value(xml, "modelName").unwrap_or("");
    let manufacturer = xml_value(xml, "manufacturer").unwrap_or("");
    xml_text_equals(name, TARGET_NAME)
        && xml_text_equals(model.strip_prefix("Sonos ").unwrap_or(model), TARGET_MODEL)
        && manufacturer.starts_with("Sonos")
}

// Compact the HTTP body in place so chunk boundaries may split XML tags or
// even individual UTF-8 characters without affecting the XML parser.
fn decode_chunks(body: &mut [u8]) -> Result<usize, &'static str> {
    let mut read = 0;
    let mut written = 0;
    loop {
        let line_len = body[read..]
            .windows(2)
            .position(|w| w == b"\r\n")
            .ok_or("Incomplete chunk size")?;
        let size_line =
            core::str::from_utf8(&body[read..read + line_len]).map_err(|_| "Invalid chunk size")?;
        let size = usize::from_str_radix(size_line.split(';').next().unwrap().trim(), 16)
            .map_err(|_| "Invalid chunk size")?;
        read += line_len + 2;
        if size == 0 {
            let trailers = &body[read..];
            if trailers.starts_with(b"\r\n") || trailers.windows(4).any(|w| w == b"\r\n\r\n") {
                return Ok(written);
            }
            return Err("Incomplete chunk trailers");
        }
        let end = read.checked_add(size).ok_or("Chunk size overflow")?;
        let next = end.checked_add(2).ok_or("Chunk size overflow")?;
        if next > body.len() {
            return Err("Incomplete chunk data");
        }
        if &body[end..next] != b"\r\n" {
            return Err("Invalid chunk separator");
        }
        body.copy_within(read..end, written);
        written += size;
        read = next;
    }
}

pub struct HttpResponse<'a> {
    pub status: u16,
    pub body: &'a str,
}

pub fn decode_http(response: &mut [u8]) -> Result<HttpResponse<'_>, &'static str> {
    let header_len = response
        .windows(4)
        .position(|w| w == b"\r\n\r\n")
        .ok_or("Invalid HTTP")?;
    let (headers, rest) = response.split_at_mut(header_len);
    let headers = core::str::from_utf8(headers).map_err(|_| "Invalid HTTP headers")?;
    let status = headers
        .lines()
        .next()
        .and_then(|line| line.split_whitespace().nth(1))
        .and_then(|value| value.parse().ok())
        .ok_or("Invalid HTTP status")?;
    let mut chunked = false;
    let mut length = None;
    for line in headers.lines().skip(1) {
        let Some((key, value)) = line.split_once(':') else {
            continue;
        };
        if key.trim().eq_ignore_ascii_case("transfer-encoding") {
            for encoding in value.split(',').map(str::trim) {
                if !encoding.eq_ignore_ascii_case("chunked") {
                    return Err("Unsupported transfer encoding");
                }
                chunked = true;
            }
        }
        if key.trim().eq_ignore_ascii_case("content-length") {
            length = Some(
                value
                    .trim()
                    .parse::<usize>()
                    .map_err(|_| "Invalid content length")?,
            );
        }
    }
    let body = &mut rest[4..];
    let length = if chunked {
        decode_chunks(body)?
    } else {
        let length = length.unwrap_or(body.len());
        if length > body.len() {
            return Err("Incomplete HTTP body");
        }
        length
    };
    let body = core::str::from_utf8(&body[..length]).map_err(|_| "Invalid UTF-8")?;
    Ok(HttpResponse { status, body })
}

pub fn http_body(response: &mut [u8]) -> Result<&str, &'static str> {
    let response = decode_http(response)?;
    if response.status != 200 {
        return Err("HTTP not 200");
    }
    if !response.body.contains("</root>") {
        return Err("Incomplete XML");
    }
    Ok(response.body)
}

/// Radio URI schemes and broadcast metadata reported by GetMediaInfo.
pub fn is_radio_media(xml: &str) -> bool {
    let uri = xml_value(xml, "CurrentURI").unwrap_or("");
    Station::ALL
        .into_iter()
        .any(|station| xml_text_equals(uri, station.url()))
        || uri.starts_with("x-rincon-mp3radio:")
        || uri.starts_with("x-sonosapi-stream:")
        || uri.starts_with("x-sonosapi-radio:")
        || xml_value(xml, "CurrentURIMetaData")
            .map(|metadata| metadata.contains("object.item.audioItem.audioBroadcast"))
            .unwrap_or(false)
}

#[derive(Clone, Copy)]
pub enum Action {
    SetRadio(Station),
    Play,
    GetTransportInfo,
    GetMediaInfo,
}

impl Action {
    pub fn name(self) -> &'static str {
        match self {
            Self::SetRadio(_) => "SetAVTransportURI",
            Self::Play => "Play",
            Self::GetTransportInfo => "GetTransportInfo",
            Self::GetMediaInfo => "GetMediaInfo",
        }
    }
}

fn escape_xml(output: &mut impl fmt::Write, value: &str) -> fmt::Result {
    for ch in value.chars() {
        match ch {
            '&' => output.write_str("&amp;")?,
            '<' => output.write_str("&lt;")?,
            '>' => output.write_str("&gt;")?,
            '"' => output.write_str("&quot;")?,
            '\'' => output.write_str("&apos;")?,
            _ => output.write_char(ch)?,
        }
    }
    Ok(())
}

pub fn soap_request(action: Action, host: &str, url: &str) -> Result<TextBuffer<3072>, fmt::Error> {
    use fmt::Write;
    let name = action.name();
    let mut body = TextBuffer::<2048>::new();
    write!(body, "<?xml version=\"1.0\"?><s:Envelope xmlns:s=\"http://schemas.xmlsoap.org/soap/envelope/\" s:encodingStyle=\"http://schemas.xmlsoap.org/soap/encoding/\"><s:Body><u:{name} xmlns:u=\"urn:schemas-upnp-org:service:AVTransport:1\"><InstanceID>0</InstanceID>")?;
    match action {
        Action::SetRadio(station) => {
            body.write_str("<CurrentURI>")?;
            escape_xml(&mut body, url)?;
            body.write_str("</CurrentURI><CurrentURIMetaData>")?;
            let mut metadata = TextBuffer::<768>::new();
            let mut title = TextBuffer::<384>::new();
            escape_xml(&mut title, station.title())?;
            let title = title.as_str();
            write!(metadata, "<DIDL-Lite xmlns:dc=\"http://purl.org/dc/elements/1.1/\" xmlns:upnp=\"urn:schemas-upnp-org:metadata-1-0/upnp/\" xmlns=\"urn:schemas-upnp-org:metadata-1-0/DIDL-Lite/\"><item id=\"R:0/0/0\" parentID=\"R:0/0\" restricted=\"true\"><dc:title>{title}</dc:title><upnp:class>object.item.audioItem.audioBroadcast</upnp:class><desc id=\"cdudn\" nameSpace=\"urn:schemas-rinconnetworks-com:metadata-1-0/\">SA_RINCON65031_</desc></item></DIDL-Lite>")?;
            escape_xml(&mut body, metadata.as_str())?;
            body.write_str("</CurrentURIMetaData>")?;
        }
        Action::Play => body.write_str("<Speed>1</Speed>")?,
        Action::GetTransportInfo | Action::GetMediaInfo => {}
    }
    write!(body, "</u:{name}></s:Body></s:Envelope>")?;
    let mut request = TextBuffer::<3072>::new();
    write!(request, "POST /MediaRenderer/AVTransport/Control HTTP/1.0\r\nHost: {host}\r\nContent-Type: text/xml; charset=\"utf-8\"\r\nSOAPAction: \"urn:schemas-upnp-org:service:AVTransport:1#{name}\"\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}", body.as_str().len(), body.as_str())?;
    Ok(request)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn custom_station_names_are_escaped_inside_didl_and_soap() {
        let station = Station {
            name: "Rock & <Jazz>",
            stream_url: "https://example.com/live?a=1&b=2",
        };
        let request =
            soap_request(Action::SetRadio(station), "192.0.2.1:1400", station.url()).unwrap();
        assert!(request
            .as_str()
            .contains("Rock &amp;amp; &amp;lt;Jazz&amp;gt;"));
        assert!(request
            .as_str()
            .contains("https://example.com/live?a=1&amp;b=2"));
    }

    #[test]
    fn radioeins_uses_explicit_sonos_radio_transport() {
        let station = STATIONS[1];
        let request =
            soap_request(Action::SetRadio(station), "192.0.2.1:1400", station.url()).unwrap();
        assert!(request.as_str().contains("<CurrentURI>x-rincon-mp3radio://dispatcher.rndfnk.com/rbb/radioeins/live/mp3/mid</CurrentURI>"));
        assert!(STATIONS[0].url().starts_with("https://"));
        assert!(STATIONS[2].url().starts_with("https://"));
    }

    #[test]
    fn all_station_requests_use_their_own_stream_and_title() {
        for station in Station::ALL {
            let request =
                soap_request(Action::SetRadio(station), "192.0.2.1:1400", station.url()).unwrap();
            assert!(request.as_str().contains(station.title()));
            assert!(request.as_str().contains(station.url()));
            assert!(request.as_str().contains("audioBroadcast"));
            let media = format!("<CurrentURI>{}</CurrentURI>", station.url());
            assert!(is_radio_media(&media));
        }
        assert_ne!(STATIONS[1].url(), STATIONS[2].url());
    }

    #[test]
    fn recognizes_radio_without_treating_every_http_song_as_radio() {
        assert!(is_radio_media(&format!(
            "<CurrentURI>{}</CurrentURI>",
            STATIONS[0].url()
        )));
        assert!(is_radio_media(
            "<CurrentURI>x-sonosapi-stream:s123</CurrentURI>"
        ));
        assert!(is_radio_media("<CurrentURI>https://radio.example/live</CurrentURI><CurrentURIMetaData>&lt;upnp:class&gt;object.item.audioItem.audioBroadcast&lt;/upnp:class&gt;</CurrentURIMetaData>"));
        assert!(!is_radio_media(
            "<CurrentURI>https://example.com/song.mp3</CurrentURI>"
        ));
        assert!(!is_radio_media(
            "<CurrentURI>x-rincon-queue:RINCON_123#0</CurrentURI>"
        ));
        let request = soap_request(
            Action::GetMediaInfo,
            "192.0.2.1:1400",
            STATIONS[0].stream_url,
        )
        .unwrap();
        assert!(request.as_str().contains("AVTransport:1#GetMediaInfo"));
    }

    #[test]
    fn parses_ssdp_location_and_rejects_malformed_hosts() {
        let response = "HTTP/1.1 200 OK\r\nLOCATION: http://192.168.1.20:1400/xml/device_description.xml\r\n\r\n";
        assert_eq!(
            location(response),
            Some(Location {
                ip: [192, 168, 1, 20],
                port: 1400,
                path: "/xml/device_description.xml"
            })
        );
        assert!(location(&response.replace("192.168.1.20", "192.168.1.999")).is_none());
        assert!(location(&response.replace("http://", "https://")).is_none());
    }

    #[test]
    fn matches_name_with_utf8_or_xml_entities_and_verifies_model() {
        for name in ["Sonos Küche", "Sonos K&#252;che", "Sonos K&#xFC;che"] {
            let xml = std::format!("<root><roomName>{name}</roomName><modelName>Sonos One SL</modelName><manufacturer>Sonos, Inc.</manufacturer></root>");
            assert!(is_target(&xml));
            assert!(!is_target(&xml.replace("One SL", "One")));
            assert!(!is_target(&xml.replace("Sonos K", "Other K")));
        }
        assert!(!xml_text_equals("Sonos K&bad;che", TARGET_NAME));
    }

    #[test]
    fn decodes_chunks_split_inside_tags_and_utf8_before_matching() {
        let xml = "<root><roomName>Sonos Küche</roomName><modelName>Sonos One SL</modelName><manufacturer>Sonos, Inc.</manufacturer></root>";
        let mut response = b"HTTP/1.1 200 OK\r\ntRaNsFeR-EnCoDiNg: Chunked\r\n\r\n".to_vec();
        for byte in xml.as_bytes() {
            response.extend_from_slice(b"1;test=yes\r\n");
            response.push(*byte);
            response.extend_from_slice(b"\r\n");
        }
        response.extend_from_slice(b"0\r\nX-Trailer: done\r\n\r\n");
        let decoded = http_body(&mut response).unwrap();
        assert_eq!(decoded, xml);
        assert!(is_target(decoded));
    }

    #[test]
    fn handles_plain_http_and_rejects_incomplete_or_invalid_chunks() {
        let mut plain = b"HTTP/1.0 200 OK\r\nContent-Length: 13\r\n\r\n<root></root>".to_vec();
        assert_eq!(http_body(&mut plain).unwrap(), "<root></root>");
        for body in ["5\r\nabc", "3\r\nabcXX0\r\n\r\n", "xyz\r\n", "0\r\n"] {
            let mut response =
                std::format!("HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n{body}")
                    .into_bytes();
            assert!(http_body(&mut response).is_err());
        }
    }

    #[test]
    fn soap_sets_the_supplied_https_stream_and_escapes_xml() {
        let url = "https://example.com/live?station=1&test=1";
        let request =
            soap_request(Action::SetRadio(STATIONS[0]), "192.168.178.50:1400", url).unwrap();
        let (headers, body) = request.as_str().split_once("\r\n\r\n").unwrap();
        assert!(body.contains("https://example.com/live?station=1&amp;test=1"));
        assert!(body.contains("&lt;DIDL-Lite"));
        assert!(body.contains("Deutschlandfunk"));
        assert!(headers.contains("#SetAVTransportURI\""));
        assert!(headers.contains(&std::format!("Content-Length: {}", body.len())));
        let play =
            soap_request(Action::Play, "192.168.178.50:1400", STATIONS[0].stream_url).unwrap();
        assert!(play.as_str().contains("#Play\""));
        assert!(play.as_str().contains("<Speed>1</Speed>"));
        let info = soap_request(
            Action::GetTransportInfo,
            "192.168.178.50:1400",
            STATIONS[0].stream_url,
        )
        .unwrap();
        assert!(info.as_str().contains("#GetTransportInfo\""));
    }

    #[test]
    fn retains_soap_fault_details_on_http_500() {
        let body = "<s:Envelope><s:Body><s:Fault><errorCode>701</errorCode><errorDescription>Transition not available</errorDescription></s:Fault></s:Body></s:Envelope>";
        let mut wire = std::format!(
            "HTTP/1.1 500 Internal Server Error\r\nContent-Length: {}\r\n\r\n{body}",
            body.len()
        )
        .into_bytes();
        let response = decode_http(&mut wire).unwrap();
        assert_eq!(response.status, 500);
        assert_eq!(xml_value(response.body, "errorCode"), Some("701"));
    }
}

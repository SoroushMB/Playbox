// ==============================================================================
//  UPnP / DLNA Discovery & SOAP ContentDirectory Service
// ==============================================================================

use std::net::UdpSocket;
use std::path::Path;
use std::thread;
use std::time::Duration;
use crate::ServerConfig;
use crate::scanner::VideoItem;
use crate::http::{escape_xml, get_mime_type, url_encode};

pub fn ssdp_responder(config: ServerConfig) {
    let ip = config.local_ip.clone();
    let port = config.port;
    let location = format!("http://{}:{}/description.xml", ip, port);
    let usn = format!("uuid:playbox-{}::urn:schemas-upnp-org:device:MediaServer:1", ip);
    let server_hdr = "Playbox/1.0 UPnP/1.0";

    // Notifier thread
    let loc2 = location.clone();
    let usn2 = usn.clone();
    thread::spawn(move || {
        loop {
            // send NOTIFY alive
            if let Ok(sock) = UdpSocket::bind("0.0.0.0:0") {
                let _ = sock.set_multicast_ttl_v4(4);
                let msg = format!(
                    "NOTIFY * HTTP/1.1\r\nHOST: 239.255.255.250:1900\r\nCACHE-CONTROL: max-age=1800\r\nLOCATION: {}\r\nNT: urn:schemas-upnp-org:device:MediaServer:1\r\nNTS: ssdp:alive\r\nSERVER: {}\r\nUSN: {}\r\n\r\n",
                    loc2, server_hdr, usn2
                );
                let _ = sock.send_to(msg.as_bytes(), "239.255.255.250:1900");
            }
            thread::sleep(Duration::from_secs(30));
        }
    });

    // Responder socket
    let sock = match UdpSocket::bind("0.0.0.0:1900") {
        Ok(s) => s,
        Err(e) => {
            eprintln!("[!] SSDP bind 1900 failed: {}", e);
            return;
        }
    };
    let _ = sock.set_multicast_loop_v4(true);
    let multi: std::net::Ipv4Addr = "239.255.255.250".parse().unwrap();
    let iface: std::net::Ipv4Addr = config.local_ip.parse().unwrap_or("0.0.0.0".parse().unwrap());
    let _ = sock.join_multicast_v4(&multi, &iface);
    let _ = sock.set_read_timeout(Some(Duration::from_secs(1)));
    let mut buf = [0u8; 2048];
    loop {
        match sock.recv_from(&mut buf) {
            Ok((n, src)) => {
                let req = String::from_utf8_lossy(&buf[..n]);
                let lower = req.to_lowercase();
                if lower.contains("m-search") {
                    let st_req = if lower.contains("ssdp:all") {
                        "ssdp:all"
                    } else if lower.contains("mediaserver:1") {
                        "urn:schemas-upnp-org:device:MediaServer:1"
                    } else {
                        continue;
                    };
                    // determine ST to respond
                    let st = if st_req == "ssdp:all" {
                        "upnp:rootdevice"
                    } else {
                        "urn:schemas-upnp-org:device:MediaServer:1"
                    };
                    let resp = format!(
                        "HTTP/1.1 200 OK\r\nCACHE-CONTROL: max-age=1800\r\nDATE: \r\nEXT: \r\nLOCATION: {}\r\nSERVER: {}\r\nST: {}\r\nUSN: {}\r\nContent-Length: 0\r\n\r\n",
                        location, server_hdr, st, usn
                    );
                    let _ = sock.send_to(resp.as_bytes(), src);
                    let _ = st_req;
                }
            }
            Err(_) => {}
        }
    }
}

pub fn generate_device_description(local_ip: &str, port: u16) -> String {
    format!(
        r#"<?xml version="1.0"?>
<root xmlns="urn:schemas-upnp-org:device-1-0">
  <specVersion><major>1</major><minor>0</minor></specVersion>
  <device>
    <deviceType>urn:schemas-upnp-org:device:MediaServer:1</deviceType>
    <friendlyName>Playbox</friendlyName>
    <manufacturer>Playbox</manufacturer>
    <modelName>Playbox</modelName>
    <UDN>uuid:playbox-{}-{}</UDN>
    <serviceList>
      <service>
        <serviceType>urn:schemas-upnp-org:service:ContentDirectory:1</serviceType>
        <serviceId>urn:upnp-org:serviceId:ContentDirectory</serviceId>
        <SCPDURL>/ContentDirectory/SCPD</SCPDURL>
        <controlURL>/ContentDirectory/Control</controlURL>
        <eventSubURL>/ContentDirectory/Event</eventSubURL>
      </service>
      <service>
        <serviceType>urn:schemas-upnp-org:service:ConnectionManager:1</serviceType>
        <serviceId>urn:upnp-org:serviceId:ConnectionManager</serviceId>
        <SCPDURL>/ConnectionManager/SCPD</SCPDURL>
        <controlURL>/ConnectionManager/Control</controlURL>
        <eventSubURL>/ConnectionManager/Event</eventSubURL>
      </service>
    </serviceList>
  </device>
</root>"#,
        local_ip, port
    )
}

pub fn generate_scpd_content_directory() -> &'static str {
    r#"<?xml version="1.0"?><scpd xmlns="urn:schemas-upnp-org:service-1-0"><specVersion><major>1</major><minor>0</minor></specVersion><actionList><action><name>Browse</name><argumentList><argument><name>ObjectID</name><direction>in</direction><relatedStateVariable>A_ARG_TYPE_ObjectID</relatedStateVariable></argument><argument><name>BrowseFlag</name><direction>in</direction><relatedStateVariable>A_ARG_TYPE_BrowseFlag</relatedStateVariable></argument><argument><name>Filter</name><direction>in</direction><relatedStateVariable>A_ARG_TYPE_Filter</relatedStateVariable></argument><argument><name>StartingIndex</name><direction>in</direction><relatedStateVariable>A_ARG_TYPE_Index</relatedStateVariable></argument><argument><name>RequestedCount</name><direction>in</direction><relatedStateVariable>A_ARG_TYPE_Count</relatedStateVariable></argument><argument><name>SortCriteria</name><direction>in</direction><relatedStateVariable>A_ARG_TYPE_SortCriteria</relatedStateVariable></argument><argument><name>Result</name><direction>out</direction><relatedStateVariable>A_ARG_TYPE_Result</relatedStateVariable></argument><argument><name>NumberReturned</name><direction>out</direction><relatedStateVariable>A_ARG_TYPE_Count</relatedStateVariable></argument><argument><name>TotalMatches</name><direction>out</direction><relatedStateVariable>A_ARG_TYPE_Count</relatedStateVariable></argument><argument><name>UpdateID</name><direction>out</direction><relatedStateVariable>A_ARG_TYPE_UpdateID</relatedStateVariable></argument></argumentList></action></actionList><serviceStateTable><stateVariable sendEvents="no"><name>A_ARG_TYPE_ObjectID</name><dataType>string</dataType></stateVariable><stateVariable sendEvents="no"><name>A_ARG_TYPE_BrowseFlag</name><dataType>string</dataType></stateVariable><stateVariable sendEvents="no"><name>A_ARG_TYPE_Filter</name><dataType>string</dataType></stateVariable><stateVariable sendEvents="no"><name>A_ARG_TYPE_Index</name><dataType>ui4</dataType></stateVariable><stateVariable sendEvents="no"><name>A_ARG_TYPE_Count</name><dataType>ui4</dataType></stateVariable><stateVariable sendEvents="no"><name>A_ARG_TYPE_SortCriteria</name><dataType>string</dataType></stateVariable><stateVariable sendEvents="no"><name>A_ARG_TYPE_Result</name><dataType>string</dataType></stateVariable><stateVariable sendEvents="no"><name>A_ARG_TYPE_UpdateID</name><dataType>ui4</dataType></stateVariable></serviceStateTable></scpd>"#
}

pub fn generate_scpd_connection_manager() -> &'static str {
    r#"<?xml version="1.0"?><scpd xmlns="urn:schemas-upnp-org:service-1-0"><specVersion><major>1</major><minor>0</minor></specVersion><actionList><action><name>GetProtocolInfo</name><argumentList><argument><name>Source</name><direction>out</direction><relatedStateVariable>SourceProtocolInfo</relatedStateVariable></argument><argument><name>Sink</name><direction>out</direction><relatedStateVariable>SinkProtocolInfo</relatedStateVariable></argument></argumentList></action></actionList><serviceStateTable><stateVariable sendEvents="no"><name>SourceProtocolInfo</name><dataType>string</dataType></stateVariable><stateVariable sendEvents="no"><name>SinkProtocolInfo</name><dataType>string</dataType></stateVariable></serviceStateTable></scpd>"#
}

pub fn generate_protocol_info_response() -> &'static str {
    r#"<?xml version="1.0"?><s:Envelope xmlns:s="http://schemas.xmlsoap.org/soap/envelope/"><s:Body><u:GetProtocolInfoResponse xmlns:u="urn:schemas-upnp-org:service:ConnectionManager:1"><Source>http-get:*:video/mp4:*,http-get:*:video/*:*</Source><Sink></Sink></u:GetProtocolInfoResponse></s:Body></s:Envelope>"#
}

pub fn generate_browse_response(videos: &[VideoItem], local_ip: &str, port: u16) -> String {
    let mut items = String::new();
    for v in videos {
        let url = format!("http://{}:{}/api/stream?path={}", local_ip, port, url_encode(&v.rel_path));
        let mime = get_mime_type(Path::new(&v.name));
        items.push_str(&format!(
            "<item id=\"{}\" parentID=\"0\" restricted=\"1\"><dc:title xmlns:dc=\"http://purl.org/dc/elements/1.1/\">{}</dc:title><res protocolInfo=\"http-get:*:{}:*\">{}</res></item>",
            escape_xml(&v.rel_path), escape_xml(&v.name), mime, url
        ));
    }
    let didl = format!(
        "<DIDL-Lite xmlns=\"urn:schemas-upnp-org:metadata-1-0/DIDL-Lite/\" xmlns:dc=\"http://purl.org/dc/elements/1.1/\" xmlns:upnp=\"urn:schemas-upnp-org:metadata-1-0/upnp/\"><container id=\"0\" parentID=\"-1\" restricted=\"1\"><dc:title>Root</dc:title></container>{}</DIDL-Lite>",
        items
    );
    format!(
        "<?xml version=\"1.0\"?><s:Envelope xmlns:s=\"http://schemas.xmlsoap.org/soap/envelope/\"><s:Body><u:BrowseResponse xmlns:u=\"urn:schemas-upnp-org:service:ContentDirectory:1\"><Result>{}</Result><NumberReturned>{}</NumberReturned><TotalMatches>{}</TotalMatches><UpdateID>1</UpdateID></u:BrowseResponse></s:Body></s:Envelope>",
        escape_xml(&didl), videos.len(), videos.len()
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_device_description() {
        let xml = generate_device_description("192.168.1.5", 8080);
        assert!(xml.contains("uuid:playbox-192.168.1.5-8080"));
        assert!(xml.contains("urn:schemas-upnp-org:device:MediaServer:1"));
    }
}

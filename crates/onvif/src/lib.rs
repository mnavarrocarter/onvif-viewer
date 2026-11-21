//! Asynchronous ONVIF device and media operations. Dropping a future cancels it.
//!
//! Credentials are held only in memory and are sent as a WS-Security UsernameToken
//! password digest. Use HTTPS when available: the digest does not encrypt traffic.

use base64::{Engine as _, engine::general_purpose::STANDARD};
use chrono::Utc;
use rand::{RngCore, rngs::OsRng};
use reqwest::{Client, redirect::Policy};
use roxmltree::{Document, Node};
use sha1::{Digest, Sha1};
use std::{fmt, time::Duration};
use url::Url;

const DEVICE: &str = "http://www.onvif.org/ver10/device/wsdl";
const MEDIA: &str = "http://www.onvif.org/ver10/media/wsdl";
const MEDIA2: &str = "http://www.onvif.org/ver20/media/wsdl";
const SOAP: &str = "http://www.w3.org/2003/05/soap-envelope";
const MAX_RESPONSE: u64 = 2 * 1024 * 1024;

/// A sanitized failure suitable for showing to the user.
#[derive(Debug)]
pub struct Error(String);

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for Error {}

#[derive(Clone, Debug)]
pub struct DeviceInformation {
    pub manufacturer: String,
    pub model: String,
    pub firmware_version: String,
    pub serial_number: String,
}

#[derive(Clone, Debug)]
pub struct Profile {
    pub token: String,
    pub name: String,
    pub width: Option<u32>,
    pub height: Option<u32>,
    pub codec: Option<String>,
    pub audio_codec: Option<String>,
}

/// A connected camera. This type deliberately does not implement Debug.
pub struct Camera {
    pub information: DeviceInformation,
    pub profiles: Vec<Profile>,
    client: Client,
    media_url: Url,
    media_namespace: &'static str,
    username: String,
    password: String,
}

/// Connect to an ONVIF device using a host, base URL, or device-service URL.
/// Network requests have bounded connect and overall timeouts.
pub async fn connect(address: &str, username: &str, password: &str) -> Result<Camera, Error> {
    let device_url = device_url(address)?;
    let client = Client::builder()
        .connect_timeout(Duration::from_secs(5))
        .timeout(Duration::from_secs(15))
        .redirect(Policy::none())
        .build()
        .map_err(|_| error("Could not initialize the camera HTTP client"))?;

    let information_xml = request(
        &client,
        &device_url,
        DEVICE,
        "GetDeviceInformation",
        "<tds:GetDeviceInformation/>",
        username,
        password,
    )
    .await?;
    let information = parse_information(&information_xml)?;
    let services_xml = request(
        &client,
        &device_url,
        DEVICE,
        "GetServices",
        "<tds:GetServices><tds:IncludeCapability>false</tds:IncludeCapability></tds:GetServices>",
        username,
        password,
    )
    .await?;
    let (media_url, media_namespace) = parse_services(&services_xml, &device_url)?;
    let (prefix, namespace) = if media_namespace == MEDIA {
        ("trt", MEDIA)
    } else {
        ("tr2", MEDIA2)
    };
    let profiles_body = format!("<{prefix}:GetProfiles/>");
    let profiles_xml = request(
        &client,
        &media_url,
        namespace,
        "GetProfiles",
        &profiles_body,
        username,
        password,
    )
    .await?;
    let profiles = parse_profiles(&profiles_xml, namespace)?;

    Ok(Camera {
        information,
        profiles,
        client,
        media_url,
        media_namespace,
        username: username.to_owned(),
        password: password.to_owned(),
    })
}

impl Camera {
    /// Request an RTSP URI for a profile. The returned URI may contain credentials;
    /// callers must not log it or persist it.
    pub async fn stream_uri(&self, profile_token: &str) -> Result<String, Error> {
        let token = escape(profile_token);
        let body = if self.media_namespace == MEDIA {
            format!(
                "<trt:GetStreamUri><trt:StreamSetup><tt:Stream>RTP-Unicast</tt:Stream><tt:Transport><tt:Protocol>RTSP</tt:Protocol></tt:Transport></trt:StreamSetup><trt:ProfileToken>{token}</trt:ProfileToken></trt:GetStreamUri>"
            )
        } else {
            format!(
                "<tr2:GetStreamUri><tr2:Protocol>RTSP</tr2:Protocol><tr2:ProfileToken>{token}</tr2:ProfileToken></tr2:GetStreamUri>"
            )
        };
        let xml = request(
            &self.client,
            &self.media_url,
            self.media_namespace,
            "GetStreamUri",
            &body,
            &self.username,
            &self.password,
        )
        .await?;
        let doc = parse_document(&xml)?;
        let response = response_node(&doc, self.media_namespace, "GetStreamUriResponse")?;
        let uri = descendant_text(response, "Uri")
            .ok_or_else(|| error("Camera returned no stream URI"))?;
        let parsed =
            Url::parse(&uri).map_err(|_| error("Camera returned an invalid stream URI"))?;
        if parsed.scheme() != "rtsp" && parsed.scheme() != "rtsps" {
            return Err(error("Camera returned a stream URI that is not RTSP"));
        }
        Ok(uri)
    }
}

fn error(message: &str) -> Error {
    Error(message.to_owned())
}

fn device_url(address: &str) -> Result<Url, Error> {
    let address = address.trim();
    if address.is_empty() {
        return Err(error("Enter the camera address"));
    }
    let input = if address.contains("://") {
        address.to_owned()
    } else {
        format!("http://{address}")
    };
    let mut url = Url::parse(&input)
        .map_err(|_| error("Invalid camera address; enter a host or HTTP(S) URL"))?;
    validate_service_url(&url)?;
    if url.path() == "/" {
        url.set_path("/onvif/device_service");
    }
    Ok(url)
}

fn validate_service_url(url: &Url) -> Result<(), Error> {
    if !matches!(url.scheme(), "http" | "https") || url.host_str().is_none() {
        return Err(error("Camera service address must be an HTTP(S) URL"));
    }
    if !url.username().is_empty() || url.password().is_some() {
        return Err(error(
            "Do not include credentials in the camera service address",
        ));
    }
    Ok(())
}

fn escape(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    for c in value.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&apos;"),
            _ => out.push(c),
        }
    }
    out
}

fn envelope(body: &str, username: &str, password: &str, nonce: &[u8], created: &str) -> String {
    let mut hash = Sha1::new();
    hash.update(nonce);
    hash.update(created.as_bytes());
    hash.update(password.as_bytes());
    let digest = STANDARD.encode(hash.finalize());
    let nonce = STANDARD.encode(nonce);
    let username = escape(username);
    format!(
        r#"<?xml version="1.0" encoding="UTF-8"?><s:Envelope xmlns:s="http://www.w3.org/2003/05/soap-envelope" xmlns:tds="http://www.onvif.org/ver10/device/wsdl" xmlns:trt="http://www.onvif.org/ver10/media/wsdl" xmlns:tr2="http://www.onvif.org/ver20/media/wsdl" xmlns:tt="http://www.onvif.org/ver10/schema" xmlns:wsse="http://docs.oasis-open.org/wss/2004/01/oasis-200401-wss-wssecurity-secext-1.0.xsd" xmlns:wsu="http://docs.oasis-open.org/wss/2004/01/oasis-200401-wss-wssecurity-utility-1.0.xsd"><s:Header><wsse:Security s:mustUnderstand="1"><wsse:UsernameToken><wsse:Username>{username}</wsse:Username><wsse:Password Type="http://docs.oasis-open.org/wss/2004/01/oasis-200401-wss-username-token-profile-1.0#PasswordDigest">{digest}</wsse:Password><wsse:Nonce EncodingType="http://docs.oasis-open.org/wss/2004/01/oasis-200401-wss-soap-message-security-1.0#Base64Binary">{nonce}</wsse:Nonce><wsu:Created>{created}</wsu:Created></wsse:UsernameToken></wsse:Security></s:Header><s:Body>{body}</s:Body></s:Envelope>"#
    )
}

async fn request(
    client: &Client,
    url: &Url,
    namespace: &str,
    operation: &str,
    body: &str,
    username: &str,
    password: &str,
) -> Result<String, Error> {
    let mut nonce = [0u8; 20];
    OsRng
        .try_fill_bytes(&mut nonce)
        .map_err(|_| error("Could not generate a secure authentication nonce"))?;
    let created = Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Secs, true);
    let xml = envelope(body, username, password, &nonce, &created);
    let content_type =
        format!("application/soap+xml; charset=utf-8; action=\"{namespace}/{operation}\"");
    let mut response = client
        .post(url.clone())
        .header(reqwest::header::CONTENT_TYPE, content_type)
        .body(xml)
        .send()
        .await
        .map_err(|e| {
            if e.is_timeout() {
                error("Camera request timed out; check its address and network connection")
            } else {
                error("Could not reach the camera; check its address and network connection")
            }
        })?;
    let status = response.status();
    if status == reqwest::StatusCode::UNAUTHORIZED || status == reqwest::StatusCode::FORBIDDEN {
        return Err(error(
            "Camera rejected authentication; check the username, password, and camera clock",
        ));
    }
    let mut bytes = Vec::new();
    while let Some(chunk) = response
        .chunk()
        .await
        .map_err(|_| error("Could not read the camera response"))?
    {
        if chunk.len() as u64 > MAX_RESPONSE - bytes.len() as u64 {
            return Err(error("Camera response is too large"));
        }
        bytes.extend_from_slice(&chunk);
    }
    let text = String::from_utf8(bytes)
        .map_err(|_| error("Camera returned a response that is not UTF-8 XML"))?;
    if let Ok(doc) = Document::parse(&text)
        && doc.descendants().any(|n| n.has_tag_name((SOAP, "Fault")))
    {
        return Err(error(
            "Camera returned an ONVIF fault; check credentials, camera clock, and service support",
        ));
    }
    if !status.is_success() {
        return Err(Error(format!(
            "Camera returned HTTP {status} during {operation}; check the service address and camera settings"
        )));
    }
    Ok(text)
}

fn parse_document(xml: &str) -> Result<Document<'_>, Error> {
    Document::parse(xml).map_err(|_| error("Camera returned malformed XML"))
}

fn response_node<'a, 'input>(
    doc: &'a Document<'input>,
    namespace: &str,
    name: &str,
) -> Result<Node<'a, 'input>, Error> {
    let envelope = doc.root_element();
    if !envelope.has_tag_name((SOAP, "Envelope")) {
        return Err(error("Camera returned an invalid SOAP envelope"));
    }
    let body = envelope
        .children()
        .find(|n| n.has_tag_name((SOAP, "Body")))
        .ok_or_else(|| error("Camera returned an invalid SOAP body"))?;
    body.children()
        .find(|n| n.has_tag_name((namespace, name)))
        .ok_or_else(|| error("Camera returned an unexpected ONVIF response"))
}

fn descendant_text(node: Node<'_, '_>, name: &str) -> Option<String> {
    node.descendants()
        .find(|n| n.is_element() && n.tag_name().name() == name)
        .and_then(|n| n.text())
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_owned)
}

fn parse_information(xml: &str) -> Result<DeviceInformation, Error> {
    let doc = parse_document(xml)?;
    let node = response_node(&doc, DEVICE, "GetDeviceInformationResponse")?;
    let field = |name| {
        node.descendants()
            .find(|n| n.is_element() && n.tag_name().name() == name)
            .map(|n| n.text().unwrap_or("").trim().to_owned())
            .ok_or_else(|| error("Camera returned incomplete device information"))
    };
    Ok(DeviceInformation {
        manufacturer: field("Manufacturer")?,
        model: field("Model")?,
        firmware_version: field("FirmwareVersion")?,
        serial_number: field("SerialNumber")?,
    })
}

fn parse_services(xml: &str, base: &Url) -> Result<(Url, &'static str), Error> {
    let doc = parse_document(xml)?;
    let response = response_node(&doc, DEVICE, "GetServicesResponse")?;
    let mut media2 = None;
    for service in response
        .children()
        .filter(|n| n.is_element() && n.tag_name().name() == "Service")
    {
        let Some(namespace) = descendant_text(service, "Namespace") else {
            continue;
        };
        if namespace != MEDIA && namespace != MEDIA2 {
            continue;
        }
        let Some(address) = descendant_text(service, "XAddr") else {
            continue;
        };
        let Ok(url) = base.join(&address) else {
            continue;
        };
        if validate_service_url(&url).is_err() {
            continue;
        }
        if namespace == MEDIA {
            return Ok((url, MEDIA));
        }
        media2 = Some((url, MEDIA2));
    }
    media2.ok_or_else(|| error("Camera did not advertise a usable ONVIF media service"))
}

fn parse_profiles(xml: &str, namespace: &str) -> Result<Vec<Profile>, Error> {
    let doc = parse_document(xml)?;
    let response = response_node(&doc, namespace, "GetProfilesResponse")?;
    response
        .children()
        .filter(|n| n.is_element() && n.tag_name().name() == "Profiles")
        .map(|profile| {
            let token = profile
                .attribute("token")
                .filter(|s| !s.is_empty())
                .ok_or_else(|| error("Camera returned a profile without a token"))?
                .to_owned();
            let name = descendant_text(profile, "Name")
                .ok_or_else(|| error("Camera returned a profile without a name"))?;
            let encoder = profile.descendants().find(|n| {
                n.is_element()
                    && matches!(
                        n.tag_name().name(),
                        "VideoEncoderConfiguration" | "VideoEncoder"
                    )
            });
            let resolution = encoder.and_then(|n| {
                n.descendants()
                    .find(|n| n.is_element() && n.tag_name().name() == "Resolution")
            });
            let width = resolution
                .and_then(|n| descendant_text(n, "Width"))
                .and_then(|s| s.parse().ok());
            let height = resolution
                .and_then(|n| descendant_text(n, "Height"))
                .and_then(|s| s.parse().ok());
            let codec = encoder.and_then(|n| descendant_text(n, "Encoding"));
            let audio_encoder = profile.descendants().find(|n| {
                n.is_element()
                    && matches!(
                        n.tag_name().name(),
                        "AudioEncoderConfiguration" | "AudioEncoder"
                    )
            });
            let audio_codec = audio_encoder.and_then(|n| descendant_text(n, "Encoding"));
            Ok(Profile {
                token,
                name,
                width,
                height,
                codec,
                audio_codec,
            })
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn soap(body: &str) -> String {
        format!(
            "<s:Envelope xmlns:s='{SOAP}' xmlns:d='{DEVICE}' xmlns:m='{MEDIA}' xmlns:t='http://www.onvif.org/ver10/schema'><s:Body>{body}</s:Body></s:Envelope>"
        )
    }

    #[test]
    fn digest_and_escaping_follow_username_token_specification() {
        let xml = envelope(
            "<x/>",
            "a<&\"'",
            "verySecret",
            b"nonce",
            "2020-01-01T00:00:00Z",
        );
        let doc = Document::parse(&xml).unwrap();
        let token = doc
            .descendants()
            .find(|n| n.tag_name().name() == "UsernameToken")
            .unwrap();
        assert_eq!(
            descendant_text(token, "Username").as_deref(),
            Some("a<&\"'")
        );
        let mut hash = Sha1::new();
        hash.update(b"nonce2020-01-01T00:00:00ZverySecret");
        assert_eq!(
            descendant_text(token, "Password").unwrap(),
            STANDARD.encode(hash.finalize())
        );
        assert_eq!(descendant_text(token, "Nonce").as_deref(), Some("bm9uY2U="));
        assert!(!xml.contains("verySecret"));
    }

    #[test]
    fn service_selection_prefers_media1_and_resolves_relative_addresses() {
        let xml = soap(&format!(
            "<d:GetServicesResponse><d:Service><d:Namespace>{MEDIA2}</d:Namespace><d:XAddr>/media2</d:XAddr></d:Service><d:Service><d:Namespace>{MEDIA}</d:Namespace><d:XAddr>/media</d:XAddr></d:Service></d:GetServicesResponse>"
        ));
        let (url, namespace) = parse_services(
            &xml,
            &Url::parse("http://camera/onvif/device_service").unwrap(),
        )
        .unwrap();
        assert_eq!(namespace, MEDIA);
        assert_eq!(url.as_str(), "http://camera/media");
    }

    #[test]
    fn parses_profile_and_optional_video_configuration() {
        let xml = soap(
            "<m:GetProfilesResponse><m:Profiles token='one'><t:Name>Main</t:Name><t:VideoEncoderConfiguration><t:Encoding>H264</t:Encoding><t:Resolution><t:Width>1920</t:Width><t:Height>1080</t:Height></t:Resolution></t:VideoEncoderConfiguration><t:AudioEncoderConfiguration><t:Encoding>G711</t:Encoding></t:AudioEncoderConfiguration></m:Profiles><m:Profiles token='two'><t:Name>Audio</t:Name></m:Profiles></m:GetProfilesResponse>",
        );
        let profiles = parse_profiles(&xml, MEDIA).unwrap();
        assert_eq!(profiles.len(), 2);
        assert_eq!(
            (
                profiles[0].width,
                profiles[0].height,
                profiles[0].codec.as_deref()
            ),
            (Some(1920), Some(1080), Some("H264"))
        );
        assert_eq!(profiles[0].audio_codec.as_deref(), Some("G711"));
        assert_eq!(
            (profiles[1].width, profiles[1].codec.as_deref()),
            (None, None)
        );
    }

    #[test]
    fn rejects_missing_token_wrong_namespace_and_embedded_credentials() {
        let xml = soap(
            "<m:GetProfilesResponse><m:Profiles><t:Name>Main</t:Name></m:Profiles></m:GetProfilesResponse>",
        );
        assert!(parse_profiles(&xml, MEDIA).is_err());
        assert!(parse_profiles(&xml, MEDIA2).is_err());
        assert!(device_url("http://user:secret@camera/").is_err());
        assert_eq!(
            device_url("camera:8080").unwrap().as_str(),
            "http://camera:8080/onvif/device_service"
        );
    }

    #[test]
    fn parses_media2_encoder_and_empty_device_fields() {
        let xml = format!(
            "<e:Envelope xmlns:e='{SOAP}' xmlns:m='{MEDIA2}' xmlns:t='http://www.onvif.org/ver10/schema'><e:Body><m:GetProfilesResponse><m:Profiles token='p'><t:Name>HD</t:Name><m:Configurations><t:VideoEncoder><t:Encoding>H265</t:Encoding><t:Resolution><t:Width>1280</t:Width><t:Height>720</t:Height></t:Resolution></t:VideoEncoder></m:Configurations></m:Profiles></m:GetProfilesResponse></e:Body></e:Envelope>"
        );
        let profiles = parse_profiles(&xml, MEDIA2).unwrap();
        assert_eq!(
            (
                profiles[0].width,
                profiles[0].height,
                profiles[0].codec.as_deref()
            ),
            (Some(1280), Some(720), Some("H265"))
        );
        let info = soap(
            "<d:GetDeviceInformationResponse><d:Manufacturer>Maker</d:Manufacturer><d:Model>Model</d:Model><d:FirmwareVersion/><d:SerialNumber/></d:GetDeviceInformationResponse>",
        );
        assert_eq!(parse_information(&info).unwrap().serial_number, "");
    }

    #[tokio::test]
    async fn exchanges_device_services_profiles_and_stream_uri() {
        use std::io::{Read, Write};
        use std::net::TcpListener;

        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let server = std::thread::spawn(move || {
            let replies = [
                soap(
                    "<d:GetDeviceInformationResponse><d:Manufacturer>Maker</d:Manufacturer><d:Model>Model</d:Model><d:FirmwareVersion>1</d:FirmwareVersion><d:SerialNumber>123</d:SerialNumber></d:GetDeviceInformationResponse>",
                ),
                soap(&format!(
                    "<d:GetServicesResponse><d:Service><d:Namespace>{MEDIA}</d:Namespace><d:XAddr>/media</d:XAddr></d:Service></d:GetServicesResponse>"
                )),
                soap(
                    "<m:GetProfilesResponse><m:Profiles token='one&amp;two'><t:Name>Main</t:Name></m:Profiles></m:GetProfilesResponse>",
                ),
                soap(
                    "<m:GetStreamUriResponse><m:MediaUri><t:Uri>rtsp://camera/live</t:Uri></m:MediaUri></m:GetStreamUriResponse>",
                ),
            ];
            let operations = [
                "GetDeviceInformation",
                "GetServices",
                "GetProfiles",
                "GetStreamUri",
            ];
            let mut nonces = Vec::new();
            for (operation, reply) in operations.into_iter().zip(replies) {
                let (mut stream, _) = listener.accept().unwrap();
                stream
                    .set_read_timeout(Some(Duration::from_secs(5)))
                    .unwrap();
                let mut request = Vec::new();
                let mut buffer = [0; 4096];
                let body_start = loop {
                    let n = stream.read(&mut buffer).unwrap();
                    assert_ne!(n, 0);
                    request.extend_from_slice(&buffer[..n]);
                    if let Some(i) = request.windows(4).position(|w| w == b"\r\n\r\n") {
                        break i + 4;
                    }
                };
                let headers = std::str::from_utf8(&request[..body_start]).unwrap();
                let length: usize = headers
                    .lines()
                    .find_map(|line| {
                        let (key, value) = line.split_once(':')?;
                        key.eq_ignore_ascii_case("content-length")
                            .then(|| value.trim().parse().unwrap())
                    })
                    .unwrap();
                while request.len() < body_start + length {
                    let n = stream.read(&mut buffer).unwrap();
                    assert_ne!(n, 0);
                    request.extend_from_slice(&buffer[..n]);
                }
                let body = std::str::from_utf8(&request[body_start..body_start + length]).unwrap();
                assert!(body.contains(operation));
                assert!(!body.contains("super-secret"));
                if operation == "GetStreamUri" {
                    assert!(body.contains("one&amp;two"));
                }
                let doc = Document::parse(body).unwrap();
                let nonce = doc
                    .descendants()
                    .find(|n| n.tag_name().name() == "Nonce")
                    .unwrap()
                    .text()
                    .unwrap()
                    .to_owned();
                assert!(!nonces.contains(&nonce));
                nonces.push(nonce);
                write!(stream, "HTTP/1.1 200 OK\r\nContent-Type: application/soap+xml\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}", reply.len(), reply).unwrap();
            }
        });
        let camera = connect(&address.to_string(), "user", "super-secret")
            .await
            .unwrap();
        assert_eq!(camera.information.model, "Model");
        assert_eq!(camera.profiles[0].token, "one&two");
        assert_eq!(
            camera.stream_uri(&camera.profiles[0].token).await.unwrap(),
            "rtsp://camera/live"
        );
        server.join().unwrap();
    }
}

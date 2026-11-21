//! Asynchronous ONVIF device and media operations. Dropping a future cancels it.
//!
//! Credentials are held only in memory. ONVIF requests use a WS-Security
//! UsernameToken digest. Use HTTPS when available: digest does not encrypt traffic.

use base64::{Engine as _, engine::general_purpose::STANDARD};
use chrono::Utc;
use rand::{RngCore, rngs::OsRng};
use reqwest::{Client, redirect::Policy};
use roxmltree::{Document, Node};
use sha1::{Digest, Sha1};
use std::{collections::HashMap, fmt, sync::Mutex, time::Duration};
use url::Url;

const DEVICE: &str = "http://www.onvif.org/ver10/device/wsdl";
const MEDIA: &str = "http://www.onvif.org/ver10/media/wsdl";
const MEDIA2: &str = "http://www.onvif.org/ver20/media/wsdl";
const PTZ: &str = "http://www.onvif.org/ver20/ptz/wsdl";
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
    ptz_url: Option<Url>,
    ptz_configurations: HashMap<String, String>,
    ptz_options: Mutex<HashMap<String, Option<PtzOptions>>>,
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
    let ptz_url = parse_ptz_service(&services_xml, &device_url)?;
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
    let ptz_configurations = parse_ptz_configurations(&profiles_xml, namespace)?;

    Ok(Camera {
        information,
        profiles,
        client,
        media_url,
        media_namespace,
        ptz_url,
        ptz_configurations,
        ptz_options: Mutex::new(HashMap::new()),
        username: username.to_owned(),
        password: password.to_owned(),
    })
}

impl Camera {
    /// Discover continuous pan/tilt support for this media profile.
    pub async fn ptz_supported(&self, profile_token: &str) -> Result<bool, Error> {
        Ok(self.ptz_options(profile_token).await?.is_some())
    }

    /// Move continuously at normalized pan and tilt velocities in the range -1 to 1.
    /// The movement expires automatically unless renewed.
    pub async fn ptz_move(&self, profile_token: &str, x: f32, y: f32) -> Result<(), Error> {
        if !x.is_finite()
            || !y.is_finite()
            || !(-1.0..=1.0).contains(&x)
            || !(-1.0..=1.0).contains(&y)
        {
            return Err(error("PTZ velocity must be between -1 and 1"));
        }
        let options = self
            .ptz_options(profile_token)
            .await?
            .ok_or_else(|| error("Continuous pan and tilt are not supported for this profile"))?;
        let url = self
            .ptz_url
            .as_ref()
            .ok_or_else(|| error("Camera has no PTZ service"))?;
        let token = escape(profile_token);
        let space = escape(&options.space);
        let x = options.x.scale(x);
        let y = options.y.scale(y);
        let timeout = options.timeout;
        let body = format!(
            r#"<tptz:ContinuousMove xmlns:tptz="{PTZ}"><tptz:ProfileToken>{token}</tptz:ProfileToken><tptz:Velocity><tt:PanTilt x="{x}" y="{y}" space="{space}"/></tptz:Velocity><tptz:Timeout>PT{timeout}S</tptz:Timeout></tptz:ContinuousMove>"#
        );
        let xml = request(
            &self.client,
            url,
            PTZ,
            "ContinuousMove",
            &body,
            &self.username,
            &self.password,
        )
        .await?;
        let doc = parse_document(&xml)?;
        response_node(&doc, PTZ, "ContinuousMoveResponse")?;
        Ok(())
    }

    /// Stop pan and tilt without affecting zoom.
    pub async fn ptz_stop(&self, profile_token: &str) -> Result<(), Error> {
        let url = self
            .ptz_url
            .as_ref()
            .ok_or_else(|| error("Camera has no PTZ service"))?;
        if !self.ptz_configurations.contains_key(profile_token) {
            return Err(error("Profile has no PTZ configuration"));
        }
        let token = escape(profile_token);
        let body = format!(
            r#"<tptz:Stop xmlns:tptz="{PTZ}"><tptz:ProfileToken>{token}</tptz:ProfileToken><tptz:PanTilt>true</tptz:PanTilt><tptz:Zoom>false</tptz:Zoom></tptz:Stop>"#
        );
        let xml = request(
            &self.client,
            url,
            PTZ,
            "Stop",
            &body,
            &self.username,
            &self.password,
        )
        .await?;
        let doc = parse_document(&xml)?;
        response_node(&doc, PTZ, "StopResponse")?;
        Ok(())
    }

    async fn ptz_options(&self, profile_token: &str) -> Result<Option<PtzOptions>, Error> {
        let (Some(url), Some(configuration)) =
            (&self.ptz_url, self.ptz_configurations.get(profile_token))
        else {
            return Ok(None);
        };
        if let Some(options) = self.ptz_options.lock().unwrap().get(profile_token).cloned() {
            return Ok(options);
        }
        let configuration = escape(configuration);
        let body = format!(
            r#"<tptz:GetConfigurationOptions xmlns:tptz="{PTZ}"><tptz:ConfigurationToken>{configuration}</tptz:ConfigurationToken></tptz:GetConfigurationOptions>"#
        );
        let xml = request(
            &self.client,
            url,
            PTZ,
            "GetConfigurationOptions",
            &body,
            &self.username,
            &self.password,
        )
        .await?;
        let options = parse_ptz_options(&xml)?;
        self.ptz_options
            .lock()
            .unwrap()
            .insert(profile_token.to_owned(), options.clone());
        Ok(options)
    }

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

#[derive(Clone)]
struct PtzOptions {
    space: String,
    x: VelocityRange,
    y: VelocityRange,
    timeout: f64,
}

#[derive(Clone, Copy)]
struct VelocityRange {
    min: f32,
    max: f32,
}

impl VelocityRange {
    fn scale(self, value: f32) -> f32 {
        if value < 0.0 {
            -value * self.min
        } else {
            value * self.max
        }
    }
}

fn parse_ptz_service(xml: &str, base: &Url) -> Result<Option<Url>, Error> {
    let doc = parse_document(xml)?;
    let response = response_node(&doc, DEVICE, "GetServicesResponse")?;
    for service in response
        .children()
        .filter(|n| n.is_element() && n.tag_name().name() == "Service")
    {
        if descendant_text(service, "Namespace").as_deref() != Some(PTZ) {
            continue;
        }
        if let Some(url) = descendant_text(service, "XAddr")
            .and_then(|address| base.join(&address).ok())
            .filter(|url| validate_service_url(url).is_ok())
        {
            return Ok(Some(url));
        }
    }
    Ok(None)
}

fn parse_ptz_configurations(xml: &str, namespace: &str) -> Result<HashMap<String, String>, Error> {
    let doc = parse_document(xml)?;
    let response = response_node(&doc, namespace, "GetProfilesResponse")?;
    let mut configurations = HashMap::new();
    for profile in response
        .children()
        .filter(|n| n.is_element() && n.tag_name().name() == "Profiles")
    {
        if let Some((profile_token, configuration_token)) =
            profile.attribute("token").and_then(|token| {
                profile
                    .descendants()
                    .find(|n| {
                        n.is_element() && matches!(n.tag_name().name(), "PTZConfiguration" | "PTZ")
                    })
                    .and_then(|n| n.attribute("token"))
                    .filter(|s| !s.is_empty())
                    .map(|configuration| (token, configuration))
            })
        {
            configurations.insert(profile_token.to_owned(), configuration_token.to_owned());
        }
    }
    Ok(configurations)
}

// XML Schema durations with day/time components, expressed as seconds.
fn duration_seconds(value: &str) -> Option<f64> {
    let value = value.strip_prefix('P')?;
    let mut rest = value;
    let mut total = 0.0;
    let mut found = false;
    if let Some((days, after)) = rest.split_once('D') {
        total += days.parse::<u64>().ok()? as f64 * 86400.0;
        rest = after;
        found = true;
    }
    if !rest.is_empty() {
        rest = rest.strip_prefix('T')?;
        if rest.is_empty() {
            return None;
        }
        for (suffix, multiplier) in [('H', 3600.0), ('M', 60.0), ('S', 1.0)] {
            if let Some((number, after)) = rest.split_once(suffix) {
                let number = number.parse::<f64>().ok()?;
                if !number.is_finite() || number < 0.0 {
                    return None;
                }
                total += number * multiplier;
                rest = after;
                found = true;
            }
        }
    }
    (found && rest.is_empty() && total.is_finite()).then_some(total)
}

fn parse_ptz_options(xml: &str) -> Result<Option<PtzOptions>, Error> {
    let doc = parse_document(xml)?;
    let response = response_node(&doc, PTZ, "GetConfigurationOptionsResponse")?;
    let Some(timeout) = response
        .descendants()
        .find(|n| n.is_element() && n.tag_name().name() == "PTZTimeout")
    else {
        return Ok(None);
    };
    let min = descendant_text(timeout, "Min").and_then(|s| duration_seconds(&s));
    let max = descendant_text(timeout, "Max").and_then(|s| duration_seconds(&s));
    let (Some(min), Some(max)) = (min, max) else {
        return Ok(None);
    };
    // The UI renews at roughly one second. Require a margin and cap how long
    // a lost connection can leave the camera moving.
    if min > max || min > 5.0 || max < 2.0 {
        return Ok(None);
    }
    let timeout = min.max(2.0);
    let range = |node: Node<'_, '_>, name| -> Option<VelocityRange> {
        let node = node
            .children()
            .find(|n| n.is_element() && n.tag_name().name() == name)?;
        let min: f32 = descendant_text(node, "Min")?.parse().ok()?;
        let max: f32 = descendant_text(node, "Max")?.parse().ok()?;
        (min.is_finite() && max.is_finite() && min < 0.0 && max > 0.0)
            .then_some(VelocityRange { min, max })
    };
    for space in response
        .descendants()
        .filter(|n| n.is_element() && n.tag_name().name() == "ContinuousPanTiltVelocitySpace")
    {
        if let (Some(uri), Some(x), Some(y)) = (
            descendant_text(space, "URI"),
            range(space, "XRange"),
            range(space, "YRange"),
        ) {
            return Ok(Some(PtzOptions {
                space: uri,
                x,
                y,
                timeout,
            }));
        }
    }
    Ok(None)
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
            "<s:Envelope xmlns:s='{SOAP}' xmlns:d='{DEVICE}' xmlns:m='{MEDIA}' xmlns:p='{PTZ}' xmlns:t='http://www.onvif.org/ver10/schema'><s:Body>{body}</s:Body></s:Envelope>"
        )
    }

    fn options_xml(min: &str, max: &str, space: &str) -> String {
        soap(&format!(
            "<p:GetConfigurationOptionsResponse><p:PTZConfigurationOptions><t:Spaces>{space}</t:Spaces><t:PTZTimeout><t:Min>{min}</t:Min><t:Max>{max}</t:Max></t:PTZTimeout></p:PTZConfigurationOptions></p:GetConfigurationOptionsResponse>"
        ))
    }

    const PAN_TILT_SPACE: &str = "<t:ContinuousPanTiltVelocitySpace><t:URI>urn:space&amp;1</t:URI><t:XRange><t:Min>-0.5</t:Min><t:Max>1</t:Max></t:XRange><t:YRange><t:Min>-1</t:Min><t:Max>0.5</t:Max></t:YRange></t:ContinuousPanTiltVelocitySpace>";

    #[test]
    fn ptz_discovery_is_profile_specific_and_optional() {
        let base = Url::parse("http://camera/onvif/device").unwrap();
        let services = soap(&format!(
            "<d:GetServicesResponse><d:Service><d:Namespace>{PTZ}</d:Namespace><d:XAddr>/ptz</d:XAddr></d:Service></d:GetServicesResponse>"
        ));
        assert_eq!(
            parse_ptz_service(&services, &base)
                .unwrap()
                .unwrap()
                .as_str(),
            "http://camera/ptz"
        );
        let absent = soap("<d:GetServicesResponse/>");
        assert!(parse_ptz_service(&absent, &base).unwrap().is_none());
        let unsafe_url = soap(&format!(
            "<d:GetServicesResponse><d:Service><d:Namespace>{PTZ}</d:Namespace><d:XAddr>http://user:pass@camera/</d:XAddr></d:Service></d:GetServicesResponse>"
        ));
        assert!(parse_ptz_service(&unsafe_url, &base).unwrap().is_none());
        let profiles = soap(
            "<m:GetProfilesResponse><m:Profiles token='a'><t:PTZConfiguration token='config&amp;1'/></m:Profiles><m:Profiles token='b'/></m:GetProfilesResponse>",
        );
        let configs = parse_ptz_configurations(&profiles, MEDIA).unwrap();
        assert_eq!(configs.get("a").map(String::as_str), Some("config&1"));
        assert!(!configs.contains_key("b"));
        let media2 = format!(
            "<s:Envelope xmlns:s='{SOAP}' xmlns:m='{MEDIA2}'><s:Body><m:GetProfilesResponse><m:Profiles token='a'><m:Configurations><m:PTZ token='c'/></m:Configurations></m:Profiles></m:GetProfilesResponse></s:Body></s:Envelope>"
        );
        assert_eq!(
            parse_ptz_configurations(&media2, MEDIA2)
                .unwrap()
                .get("a")
                .map(String::as_str),
            Some("c")
        );
    }

    #[test]
    fn ptz_options_require_safe_timeout_and_bidirectional_space() {
        let options = parse_ptz_options(&options_xml("PT1S", "PT60S", PAN_TILT_SPACE))
            .unwrap()
            .unwrap();
        assert_eq!(options.timeout, 2.0);
        assert_eq!(options.space, "urn:space&1");
        assert_eq!(options.x.scale(-1.0), -0.5);
        assert_eq!(options.y.scale(1.0), 0.5);
        assert_eq!(
            parse_ptz_options(&options_xml("PT3S", "PT1M", PAN_TILT_SPACE))
                .unwrap()
                .unwrap()
                .timeout,
            3.0
        );
        for (min, max, space) in [
            ("PT6S", "PT60S", PAN_TILT_SPACE),
            ("PT0S", "PT1S", PAN_TILT_SPACE),
            ("PT3S", "PT2S", PAN_TILT_SPACE),
            ("invalid", "PT60S", PAN_TILT_SPACE),
            ("PT1S", "PT60S", ""),
        ] {
            assert!(
                parse_ptz_options(&options_xml(min, max, space))
                    .unwrap()
                    .is_none()
            );
        }
        assert_eq!(duration_seconds("P1DT1H2M3.5S"), Some(90123.5));
        assert_eq!(duration_seconds("PTNaNS"), None);
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
                    "<d:GetServicesResponse><d:Service><d:Namespace>{MEDIA}</d:Namespace><d:XAddr>/media</d:XAddr></d:Service><d:Service><d:Namespace>{PTZ}</d:Namespace><d:XAddr>/ptz</d:XAddr></d:Service></d:GetServicesResponse>"
                )),
                soap(
                    "<m:GetProfilesResponse><m:Profiles token='one&amp;two'><t:Name>Main</t:Name><t:PTZConfiguration token='config&amp;1'/></m:Profiles><m:Profiles token='fixed'><t:Name>Fixed</t:Name></m:Profiles></m:GetProfilesResponse>",
                ),
                soap(
                    "<m:GetStreamUriResponse><m:MediaUri><t:Uri>rtsp://camera/live</t:Uri></m:MediaUri></m:GetStreamUriResponse>",
                ),
                options_xml("PT1S", "PT60S", PAN_TILT_SPACE),
                soap("<p:ContinuousMoveResponse/>"),
                soap("<p:StopResponse/>"),
            ];
            let operations = [
                "GetDeviceInformation",
                "GetServices",
                "GetProfiles",
                "GetStreamUri",
                "GetConfigurationOptions",
                "ContinuousMove",
                "Stop",
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
                if operation == "GetConfigurationOptions" {
                    assert!(body.contains("config&amp;1"));
                }
                if operation == "ContinuousMove" {
                    assert!(body.contains("one&amp;two"));
                    assert!(body.contains("urn:space&amp;1"));
                    assert!(body.contains("x=\"-0.5\""));
                    assert!(body.contains("y=\"0.5\""));
                    assert!(body.contains("PT2S"));
                }
                if operation == "Stop" {
                    assert!(body.contains("<tptz:PanTilt>true</tptz:PanTilt>"));
                    assert!(body.contains("<tptz:Zoom>false</tptz:Zoom>"));
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
        assert!(!camera.ptz_supported("fixed").await.unwrap());
        assert!(camera.ptz_supported("one&two").await.unwrap());
        assert!(camera.ptz_move("one&two", f32::NAN, 0.0).await.is_err());
        camera.ptz_move("one&two", -1.0, 1.0).await.unwrap();
        camera.ptz_stop("one&two").await.unwrap();
        server.join().unwrap();
    }
}

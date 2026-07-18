use std::collections::HashMap;
use std::io::{self, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

use crate::codex::CodexPlanner;
use crate::model::{Studio, StudioError, json_string};
use crate::prompt::{EditPlan, PromptEngine};

const MAX_REQUEST_BYTES: usize = 64 * 1024;
const INDEX_HTML: &str = include_str!("../web/index.html");
const APP_CSS: &str = include_str!("../web/app.css");
const APP_JS: &str = include_str!("../web/app.js");

pub fn run(port: u16) -> io::Result<()> {
    let address = format!("127.0.0.1:{port}");
    let listener = TcpListener::bind(&address)?;
    let router = Router::new();
    println!("DAW-AI is ready at http://{address}");

    for connection in listener.incoming() {
        match connection {
            Ok(mut stream) => {
                let router = router.clone();
                thread::spawn(move || {
                    if let Err(error) = serve_connection(&mut stream, &router) {
                        eprintln!("request failed: {error}");
                    }
                });
            }
            Err(error) => eprintln!("connection failed: {error}"),
        }
    }
    Ok(())
}

fn serve_connection(stream: &mut TcpStream, router: &Router) -> io::Result<()> {
    stream.set_read_timeout(Some(Duration::from_secs(5)))?;
    let response = match Request::read(stream) {
        Ok(request) => router.handle(&request),
        Err(error) => Response::json(400, error_json(&error)),
    };
    response.write(stream)
}

#[derive(Clone)]
struct Router {
    studio: Arc<Mutex<Studio>>,
    planner: Planner,
}

#[derive(Clone, Copy)]
enum Planner {
    Codex,
    Demo,
}

impl Router {
    fn new() -> Self {
        let planner = match std::env::var("DAW_AI_PROMPT_ENGINE") {
            Ok(value) if value == "demo" => Planner::Demo,
            _ => Planner::Codex,
        };
        Self {
            studio: Arc::new(Mutex::new(Studio::new())),
            planner,
        }
    }

    #[cfg(test)]
    fn demo() -> Self {
        Self {
            studio: Arc::new(Mutex::new(Studio::new())),
            planner: Planner::Demo,
        }
    }

    fn handle(&self, request: &Request) -> Response {
        if !request.has_trusted_host() {
            return Response::json(403, error_json("untrusted host rejected"));
        }
        if request.is_mutation() && !request.is_trusted_mutation() {
            return Response::json(403, error_json("cross-origin request rejected"));
        }

        match (request.method.as_str(), request.path.as_str()) {
            ("GET", "/") => Response::static_asset("text/html; charset=utf-8", INDEX_HTML),
            ("GET", "/app.css") => Response::static_asset("text/css; charset=utf-8", APP_CSS),
            ("GET", "/app.js") => Response::static_asset("text/javascript; charset=utf-8", APP_JS),
            ("GET", "/api/health") => Response::json(200, "{\"status\":\"ok\"}".to_owned()),
            ("GET", "/api/project") => {
                let studio = self.lock_studio();
                Response::json(200, studio.to_json())
            }
            ("POST", "/api/edits") => self.apply_edit(&request.body),
            ("POST", "/api/mix") => self.change_mix(&request.body),
            ("POST", "/api/sound-tools") => self.change_sound_tool(&request.body),
            ("POST", "/api/undo") => self.undo(),
            ("POST", "/api/reset") => self.reset(),
            (_, "/api/edits" | "/api/mix" | "/api/sound-tools" | "/api/undo" | "/api/reset") => {
                Response::json(405, error_json("method not allowed")).with_header("Allow", "POST")
            }
            (_, "/api/project" | "/api/health") => {
                Response::json(405, error_json("method not allowed")).with_header("Allow", "GET")
            }
            _ => Response::json(404, error_json("not found")),
        }
    }

    fn apply_edit(&self, body: &str) -> Response {
        let form = parse_form(body);
        let Some(prompt) = form.get("prompt") else {
            return Response::json(422, error_json("prompt is required"));
        };
        let Some(start) = form
            .get("start")
            .and_then(|value| value.parse::<f32>().ok())
        else {
            return Response::json(422, error_json("selection start is required"));
        };
        let Some(end) = form.get("end").and_then(|value| value.parse::<f32>().ok()) else {
            return Response::json(422, error_json("selection end is required"));
        };

        let project = {
            let studio = self.lock_studio();
            if let Err(error) = studio.validate_edit(start, end, prompt) {
                return Response::json(422, studio_error(error));
            }
            studio.project().clone()
        };
        let plan = match self.plan_edit(prompt, start, end, &project) {
            Ok(plan) => plan,
            Err(message) => return Response::json(503, error_json(&message)),
        };
        let mut studio = self.lock_studio();
        if studio.project().version != project.version {
            return Response::json(
                409,
                error_json("the project changed; submit the edit again"),
            );
        }
        match studio.apply_plan(start, end, prompt, plan) {
            Ok(summary) => Response::json(
                200,
                format!(
                    "{{\"message\":{},\"project\":{}}}",
                    json_string(&summary),
                    studio.to_json()
                ),
            ),
            Err(error) => Response::json(422, studio_error(error)),
        }
    }

    fn plan_edit(
        &self,
        prompt: &str,
        start: f32,
        end: f32,
        project: &crate::model::Project,
    ) -> Result<EditPlan, String> {
        match self.planner {
            Planner::Demo => Ok(PromptEngine::interpret_project(prompt, project, start, end)),
            Planner::Codex => CodexPlanner::interpret(prompt, start, end, project)
                .map_err(|error| error.to_string()),
        }
    }

    fn change_mix(&self, body: &str) -> Response {
        let form = parse_form(body);
        let Some(track_id) = form
            .get("track_id")
            .and_then(|value| value.parse::<u64>().ok())
        else {
            return Response::json(422, error_json("track_id is required"));
        };
        let volume = match form.get("volume") {
            Some(value) => match value.parse::<f32>() {
                Ok(volume) => Some(volume),
                Err(_) => return Response::json(422, error_json("volume must be a number")),
            },
            None => None,
        };
        let muted = match form.get("muted") {
            Some(value) if value == "true" => Some(true),
            Some(value) if value == "false" => Some(false),
            Some(_) => return Response::json(422, error_json("muted must be true or false")),
            None => None,
        };

        let mut studio = self.lock_studio();
        match studio.set_mix(track_id, volume, muted) {
            Ok(()) => Response::json(200, studio.to_json()),
            Err(error) => Response::json(422, studio_error(error)),
        }
    }

    fn change_sound_tool(&self, body: &str) -> Response {
        let form = parse_form(body);
        let Some(track_id) = form
            .get("track_id")
            .and_then(|value| value.parse::<u64>().ok())
        else {
            return Response::json(422, error_json("track_id is required"));
        };
        let Some(tool) = form.get("tool") else {
            return Response::json(422, error_json("tool is required"));
        };
        let Some(tool_id) = form
            .get("tool_id")
            .and_then(|value| value.parse::<u64>().ok())
        else {
            return Response::json(422, error_json("tool_id is required"));
        };
        let clip_id = match form.get("clip_id") {
            Some(value) => match value.parse::<u64>() {
                Ok(value) => Some(value),
                Err(_) => return Response::json(422, error_json("clip_id must be an integer")),
            },
            None => None,
        };
        let Some(parameter) = form.get("parameter") else {
            return Response::json(422, error_json("parameter is required"));
        };
        let Some(value) = form.get("value") else {
            return Response::json(422, error_json("value is required"));
        };

        let mut studio = self.lock_studio();
        match studio.configure_sound_tool(track_id, tool, tool_id, clip_id, parameter, value) {
            Ok(()) => Response::json(200, studio.to_json()),
            Err(error) => Response::json(422, studio_error(error)),
        }
    }

    fn undo(&self) -> Response {
        let mut studio = self.lock_studio();
        if studio.undo() {
            Response::json(200, studio.to_json())
        } else {
            Response::json(409, error_json("nothing to undo"))
        }
    }

    fn reset(&self) -> Response {
        let mut studio = self.lock_studio();
        studio.reset();
        Response::json(200, studio.to_json())
    }

    fn lock_studio(&self) -> std::sync::MutexGuard<'_, Studio> {
        self.studio
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }
}

struct Request {
    method: String,
    path: String,
    headers: HashMap<String, String>,
    body: String,
}

impl Request {
    fn read(stream: &mut impl Read) -> Result<Self, String> {
        let mut bytes = Vec::with_capacity(2048);
        let header_end = loop {
            let mut chunk = [0_u8; 2048];
            let count = stream.read(&mut chunk).map_err(|error| error.to_string())?;
            if count == 0 {
                return Err("incomplete request".to_owned());
            }
            bytes.extend_from_slice(&chunk[..count]);
            if bytes.len() > MAX_REQUEST_BYTES {
                return Err("request is too large".to_owned());
            }
            if let Some(position) = find_bytes(&bytes, b"\r\n\r\n") {
                break position + 4;
            }
        };

        let headers = std::str::from_utf8(&bytes[..header_end])
            .map_err(|_| "request headers must be UTF-8".to_owned())?;
        let mut lines = headers.split("\r\n");
        let request_line = lines
            .next()
            .ok_or_else(|| "missing request line".to_owned())?;
        let mut request_parts = request_line.split_whitespace();
        let method = request_parts
            .next()
            .ok_or_else(|| "missing method".to_owned())?
            .to_owned();
        let target = request_parts
            .next()
            .ok_or_else(|| "missing path".to_owned())?;
        if request_parts.next().is_none() {
            return Err("missing HTTP version".to_owned());
        }
        let path = target.split('?').next().unwrap_or(target).to_owned();

        let headers: HashMap<String, String> = lines
            .filter_map(|line| line.split_once(':'))
            .map(|(name, value)| (name.trim().to_lowercase(), value.trim().to_owned()))
            .collect();
        let content_length = headers.get("content-length").map_or(Ok(0_usize), |value| {
            value
                .parse::<usize>()
                .map_err(|_| "invalid content length".to_owned())
        })?;
        let body_end = header_end
            .checked_add(content_length)
            .ok_or_else(|| "request is too large".to_owned())?;
        if body_end > MAX_REQUEST_BYTES {
            return Err("request is too large".to_owned());
        }

        while bytes.len() < body_end {
            let remaining = body_end - bytes.len();
            let mut chunk = [0_u8; 2048];
            let count = stream
                .read(&mut chunk[..remaining.min(2048)])
                .map_err(|error| error.to_string())?;
            if count == 0 {
                return Err("incomplete request body".to_owned());
            }
            bytes.extend_from_slice(&chunk[..count]);
        }

        let body = std::str::from_utf8(&bytes[header_end..body_end])
            .map_err(|_| "request body must be UTF-8".to_owned())?
            .to_owned();
        Ok(Self {
            method,
            path,
            headers,
            body,
        })
    }

    fn is_mutation(&self) -> bool {
        self.method == "POST"
            && matches!(
                self.path.as_str(),
                "/api/edits" | "/api/mix" | "/api/sound-tools" | "/api/undo" | "/api/reset"
            )
    }

    fn has_trusted_host(&self) -> bool {
        // Forwarded authority identifies the public origin; it never replaces loopback trust.
        self.headers
            .get("host")
            .is_some_and(|host| is_loopback_host(host))
            && self
                .headers
                .get("x-forwarded-host")
                .is_none_or(|host| forwarded_host(host).is_some())
    }

    fn public_host(&self) -> Option<&str> {
        if !self.has_trusted_host() {
            return None;
        }
        self.headers
            .get("x-forwarded-host")
            .and_then(|host| forwarded_host(host))
            .or_else(|| self.headers.get("host").map(String::as_str))
    }

    fn is_trusted_mutation(&self) -> bool {
        let Some(host) = self.public_host() else {
            return false;
        };
        if self
            .headers
            .get("sec-fetch-site")
            .is_some_and(|site| site.eq_ignore_ascii_case("cross-site"))
        {
            return false;
        }

        self.headers
            .get("origin")
            .is_none_or(|origin| origin_matches_host(origin, host))
    }
}

struct Response {
    status: u16,
    content_type: &'static str,
    body: String,
    headers: Vec<(&'static str, &'static str)>,
}

impl Response {
    fn json(status: u16, body: String) -> Self {
        Self {
            status,
            content_type: "application/json; charset=utf-8",
            body,
            headers: vec![("Cache-Control", "no-store")],
        }
    }

    fn static_asset(content_type: &'static str, body: &str) -> Self {
        Self {
            status: 200,
            content_type,
            body: body.to_owned(),
            headers: vec![("Cache-Control", "no-cache")],
        }
    }

    fn with_header(mut self, name: &'static str, value: &'static str) -> Self {
        self.headers.push((name, value));
        self
    }

    fn write(&self, stream: &mut impl Write) -> io::Result<()> {
        let reason = match self.status {
            200 => "OK",
            400 => "Bad Request",
            403 => "Forbidden",
            404 => "Not Found",
            405 => "Method Not Allowed",
            409 => "Conflict",
            422 => "Unprocessable Content",
            _ => "Error",
        };
        let mut head = format!(
            concat!(
                "HTTP/1.1 {} {}\r\nContent-Type: {}\r\nContent-Length: {}\r\n",
                "Connection: close\r\nX-Content-Type-Options: nosniff\r\n",
                "Content-Security-Policy: default-src 'self'; script-src 'self'; ",
                "style-src 'self' 'unsafe-inline'; connect-src 'self'; img-src 'self' data:; ",
                "object-src 'none'; frame-ancestors 'none'; base-uri 'none';\r\n",
                "Referrer-Policy: no-referrer\r\n"
            ),
            self.status,
            reason,
            self.content_type,
            self.body.len()
        );
        for (name, value) in &self.headers {
            head.push_str(name);
            head.push_str(": ");
            head.push_str(value);
            head.push_str("\r\n");
        }
        head.push_str("\r\n");
        stream.write_all(head.as_bytes())?;
        stream.write_all(self.body.as_bytes())
    }
}

fn parse_form(body: &str) -> HashMap<String, String> {
    body.split('&')
        .filter(|part| !part.is_empty())
        .map(|part| {
            let (key, value) = part.split_once('=').unwrap_or((part, ""));
            (url_decode(key), url_decode(value))
        })
        .collect()
}

fn forwarded_host(value: &str) -> Option<&str> {
    let host = value.split(',').next()?.trim();
    parse_authority(host).map(|_| host)
}

fn is_loopback_host(value: &str) -> bool {
    parse_authority(value).is_some_and(|(host, _)| {
        host.eq_ignore_ascii_case("localhost") || matches!(host, "127.0.0.1" | "[::1]")
    })
}

fn origin_matches_host(origin: &str, host: &str) -> bool {
    let (authority, default_port) = origin
        .strip_prefix("http://")
        .map(|authority| (authority, 80))
        .or_else(|| {
            origin
                .strip_prefix("https://")
                .map(|authority| (authority, 443))
        })
        .unwrap_or(("", 0));
    if default_port == 0 {
        return false;
    }
    let Some((origin_host, origin_port)) = parse_authority(authority) else {
        return false;
    };
    let Some((request_host, request_port)) = parse_authority(host) else {
        return false;
    };
    origin_host.eq_ignore_ascii_case(request_host)
        && origin_port.unwrap_or(default_port) == request_port.unwrap_or(default_port)
}

fn parse_authority(value: &str) -> Option<(&str, Option<u16>)> {
    if value.is_empty()
        || value
            .bytes()
            .any(|byte| byte.is_ascii_control() || byte.is_ascii_whitespace())
        || value.contains(['/', '\\', '?', '#', '@', ','])
    {
        return None;
    }

    if value.starts_with('[') {
        let end = value.find(']')?;
        let hostname = &value[..=end];
        if hostname.len() <= 2 {
            return None;
        }
        let remainder = &value[end + 1..];
        let port = if remainder.is_empty() {
            None
        } else {
            Some(parse_port(remainder.strip_prefix(':')?)?)
        };
        return Some((hostname, port));
    }

    let (hostname, port) = value
        .rsplit_once(':')
        .map_or((value, None), |(hostname, port)| (hostname, Some(port)));
    if hostname.is_empty()
        || hostname.contains(':')
        || !hostname
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'-' | b'_'))
    {
        return None;
    }
    let port = match port {
        Some(port) => Some(parse_port(port)?),
        None => None,
    };
    Some((hostname, port))
}

fn parse_port(value: &str) -> Option<u16> {
    value.parse::<u16>().ok().filter(|port| *port > 0)
}

fn url_decode(value: &str) -> String {
    let bytes = value.as_bytes();
    let mut output = Vec::with_capacity(bytes.len());
    let mut index = 0;
    while index < bytes.len() {
        match bytes[index] {
            b'+' => output.push(b' '),
            b'%' if index + 2 < bytes.len() => {
                if let (Some(high), Some(low)) =
                    (hex_value(bytes[index + 1]), hex_value(bytes[index + 2]))
                {
                    output.push(high * 16 + low);
                    index += 2;
                } else {
                    output.push(bytes[index]);
                }
            }
            byte => output.push(byte),
        }
        index += 1;
    }
    String::from_utf8_lossy(&output).into_owned()
}

const fn hex_value(byte: u8) -> Option<u8> {
    match byte {
        b'0'..=b'9' => Some(byte - b'0'),
        b'a'..=b'f' => Some(byte - b'a' + 10),
        b'A'..=b'F' => Some(byte - b'A' + 10),
        _ => None,
    }
}

fn find_bytes(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack
        .windows(needle.len())
        .position(|window| window == needle)
}

fn error_json(message: &str) -> String {
    format!("{{\"error\":{}}}", json_string(message))
}

fn studio_error(error: StudioError) -> String {
    match error {
        StudioError::EmptyPrompt => error_json("describe the change you want"),
        StudioError::InvalidSelection => error_json("select a valid part of the track"),
        StudioError::UnknownTrack => error_json("track not found"),
        StudioError::InvalidMix => error_json("invalid mixer setting"),
        StudioError::UnknownSoundTool => error_json("sound tool not found"),
        StudioError::InvalidSoundTool => error_json("invalid sound tool setting"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn request(method: &str, path: &str, body: &str) -> Request {
        Request {
            method: method.to_owned(),
            path: path.to_owned(),
            headers: HashMap::from([("host".to_owned(), "127.0.0.1:8888".to_owned())]),
            body: body.to_owned(),
        }
    }

    #[test]
    fn serves_the_app_and_project_api() {
        let router = Router::demo();
        let page = router.handle(&request("GET", "/", ""));
        assert_eq!(page.status, 200);
        assert!(page.body.contains("DAW-AI"));

        let project = router.handle(&request("GET", "/api/project", ""));
        assert_eq!(project.status, 200);
        assert!(project.body.contains("\"tracks\""));
    }

    #[test]
    fn edit_api_updates_the_shared_project() {
        let router = Router::demo();
        let response = router.handle(&request(
            "POST",
            "/api/edits",
            "start=4&end=8&prompt=increase+volume",
        ));
        assert_eq!(response.status, 200);
        assert!(response.body.contains("Lifted"));

        let project = router.handle(&request("GET", "/api/project", ""));
        assert!(project.body.contains("increase volume"));
    }

    #[test]
    fn sound_tool_api_updates_the_shared_graph() {
        let router = Router::demo();
        let response = router.handle(&request(
            "POST",
            "/api/sound-tools",
            "track_id=2&tool=instrument&tool_id=201&parameter=waveform&value=sawtooth",
        ));
        assert_eq!(response.status, 200);
        assert!(response.body.contains("\"waveform\":\"sawtooth\""));

        let invalid = router.handle(&request(
            "POST",
            "/api/sound-tools",
            "track_id=2&tool=instrument&tool_id=201&parameter=attack&value=99",
        ));
        assert_eq!(invalid.status, 422);
        let project = router.handle(&request("GET", "/api/project", ""));
        assert!(project.body.contains("\"waveform\":\"sawtooth\""));
    }

    #[test]
    fn validates_api_requests_and_methods() {
        let router = Router::demo();
        assert_eq!(
            router
                .handle(&request("POST", "/api/edits", "start=1&end=2"))
                .status,
            422
        );
        let before = router.handle(&request("GET", "/api/project", "")).body;
        assert_eq!(
            router
                .handle(&request(
                    "POST",
                    "/api/edits",
                    "start=4&end=8&prompt=make+the+lead+louder",
                ))
                .status,
            422
        );
        assert_eq!(
            router.handle(&request("GET", "/api/project", "")).body,
            before
        );
        assert_eq!(
            router
                .handle(&request(
                    "POST",
                    "/api/mix",
                    "track_id=1&volume=bad&muted=true",
                ))
                .status,
            422
        );
        assert_eq!(
            router.handle(&request("GET", "/api/project", "")).body,
            before
        );
        assert_eq!(
            router
                .handle(&request(
                    "POST",
                    "/api/mix",
                    "track_id=1&volume=0.5&muted=maybe",
                ))
                .status,
            422
        );
        assert_eq!(
            router.handle(&request("GET", "/api/project", "")).body,
            before
        );
        assert_eq!(router.handle(&request("GET", "/missing", "")).status, 404);
        assert_eq!(router.handle(&request("GET", "/api/undo", "")).status, 405);
    }

    #[test]
    fn parses_http_request_and_encoded_forms() {
        let body = "prompt=warm+%26+wide&start=0&end=4";
        let raw = format!(
            "POST /api/edits HTTP/1.1\r\nHost: localhost\r\nContent-Length: {}\r\n\r\n{body}",
            body.len()
        );
        let parsed = Request::read(&mut raw.as_bytes()).expect("valid request");
        assert_eq!(parsed.path, "/api/edits");
        assert_eq!(parsed.headers["host"], "localhost");
        assert_eq!(parse_form(&parsed.body)["prompt"], "warm & wide");
    }

    #[test]
    fn rejects_content_lengths_that_overflow_the_request_bound() {
        let raw = format!(
            "POST /api/edits HTTP/1.1\r\nHost: localhost\r\nContent-Length: {}\r\n\r\n",
            usize::MAX
        );
        let error = Request::read(&mut raw.as_bytes())
            .err()
            .expect("oversized request must be rejected");
        assert_eq!(error, "request is too large");
    }

    #[test]
    fn rejects_cross_origin_mutations_without_changing_state() {
        let router = Router::demo();
        let mut hostile = request("POST", "/api/edits", "start=4&end=8&prompt=increase+volume");
        hostile
            .headers
            .insert("origin".to_owned(), "http://127.0.0.1:18867".to_owned());
        hostile
            .headers
            .insert("sec-fetch-site".to_owned(), "cross-site".to_owned());

        assert_eq!(router.handle(&hostile).status, 403);
        let project = router.handle(&request("GET", "/api/project", ""));
        assert!(!project.body.contains("increase volume"));

        hostile.path = "/api/sound-tools".to_owned();
        hostile.body =
            "track_id=2&tool=instrument&tool_id=201&parameter=waveform&value=sawtooth".to_owned();
        assert_eq!(router.handle(&hostile).status, 403);
        let project = router.handle(&request("GET", "/api/project", ""));
        assert!(project.body.contains("\"waveform\":\"square\""));
        assert!(!project.body.contains("\"waveform\":\"sawtooth\""));

        hostile
            .headers
            .insert("x-forwarded-host".to_owned(), "studio.example".to_owned());
        hostile
            .headers
            .insert("origin".to_owned(), "https://attacker.example".to_owned());
        hostile
            .headers
            .insert("sec-fetch-site".to_owned(), "same-origin".to_owned());
        assert_eq!(router.handle(&hostile).status, 403);

        hostile
            .headers
            .insert("origin".to_owned(), "https://studio.example".to_owned());
        assert_eq!(router.handle(&hostile).status, 200);
        let project = router.handle(&request("GET", "/api/project", ""));
        assert!(project.body.contains("\"waveform\":\"sawtooth\""));
    }

    #[test]
    fn supports_reverse_proxy_hosts_without_configuration() {
        let router = Router::demo();
        let mut forwarded = request(
            "POST",
            "/api/sound-tools",
            "track_id=2&tool=instrument&tool_id=201&parameter=waveform&value=sawtooth",
        );
        forwarded.headers.insert(
            "x-forwarded-host".to_owned(),
            "studio.example:443, proxy.internal".to_owned(),
        );
        forwarded
            .headers
            .insert("origin".to_owned(), "https://studio.example".to_owned());
        forwarded
            .headers
            .insert("sec-fetch-site".to_owned(), "same-origin".to_owned());
        assert_eq!(router.handle(&forwarded).status, 200);
    }

    #[test]
    fn rejects_dns_rebinding_hosts_before_reading_or_mutating_project() {
        let router = Router::demo();
        let mut rebound = request("GET", "/api/project", "");
        rebound
            .headers
            .insert("host".to_owned(), "attacker.example".to_owned());

        let response = router.handle(&rebound);
        assert_eq!(response.status, 403);
        assert!(!response.body.contains("Neon First Light"));

        rebound.method = "POST".to_owned();
        rebound.path = "/api/sound-tools".to_owned();
        rebound.body =
            "track_id=2&tool=instrument&tool_id=201&parameter=waveform&value=sawtooth".to_owned();
        rebound
            .headers
            .insert("origin".to_owned(), "http://attacker.example".to_owned());
        rebound
            .headers
            .insert("sec-fetch-site".to_owned(), "same-origin".to_owned());
        assert_eq!(router.handle(&rebound).status, 403);

        rebound
            .headers
            .insert("x-forwarded-host".to_owned(), "attacker.example".to_owned());
        assert_eq!(router.handle(&rebound).status, 403);

        let project = router.handle(&request("GET", "/api/project", ""));
        assert!(project.body.contains("\"waveform\":\"square\""));
        assert!(!project.body.contains("\"waveform\":\"sawtooth\""));
    }

    #[test]
    fn rejects_malformed_host_authorities() {
        let router = Router::demo();
        let mut invalid = request("GET", "/api/project", "");
        invalid
            .headers
            .insert("host".to_owned(), "studio.example/path".to_owned());

        let response = router.handle(&invalid);
        assert_eq!(response.status, 403);
        assert!(!response.body.contains("Neon First Light"));
    }

    #[test]
    fn response_contains_security_and_length_headers() {
        let response = Response::json(200, "{\"ok\":true}".to_owned());
        let mut bytes = Vec::new();
        response.write(&mut bytes).expect("writable buffer");
        let rendered = String::from_utf8(bytes).expect("UTF-8 response");
        assert!(rendered.contains("Content-Length: 11"));
        assert!(rendered.contains("X-Content-Type-Options: nosniff"));
    }
}

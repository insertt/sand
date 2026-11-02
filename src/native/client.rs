//! The client half of ACP (JSON-RPC 2.0, one message per line) for the agent
//! sand starts in the sandbox. sand offers no file system or terminal, so the
//! agent runs its own tools, inside the sandbox.

use std::collections::HashMap;
use std::io::{self, BufRead, Write};
use std::sync::mpsc::Sender;
use std::thread;

use serde_json::{Value, json};

use super::model::Prompt;
use crate::acp::PathMap;

/// Requests sand sends, so that their responses can be told apart.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Call {
    Initialize,
    NewSession,
    Prompt,
    SetMode,
    SetModel,
    SetConfig,
    ListSessions,
    LoadSession,
    ResumeSession,
    ForkSession,
}

impl Call {
    fn method(self) -> &'static str {
        match self {
            Self::Initialize => "initialize",
            Self::NewSession => "session/new",
            Self::Prompt => "session/prompt",
            Self::SetMode => "session/set_mode",
            Self::SetModel => "session/set_model",
            Self::SetConfig => "session/set_config_option",
            Self::ListSessions => "session/list",
            Self::LoadSession => "session/load",
            Self::ResumeSession => "session/resume",
            Self::ForkSession => "session/fork",
        }
    }
}

/// A message from the agent.
#[derive(Debug, PartialEq)]
pub enum Incoming {
    /// `session/update`: its `update`.
    Update(Value),
    /// `session/request_permission`, to answer with [`Client::respond`].
    Permission { id: Value, params: Value },
    /// Any other request: sand answers that it has no such method.
    Unsupported { id: Value, method: String },
    /// The response to a request sand sent.
    Response { id: u64, result: Result<Value, String> },
    /// The agent's output ended.
    Closed,
}

/// Classifies one line of the agent's output; `None` for anything that is
/// not a JSON-RPC message (agents may log on stdout).
pub fn parse(line: &str) -> Option<Incoming> {
    let message: Value = serde_json::from_str(line).ok()?;
    let method = message["method"].as_str();
    let id = message.get("id").filter(|id| !id.is_null());
    match (method, id) {
        (Some("session/update"), None) => Some(Incoming::Update(message["params"]["update"].clone())),
        (Some(_), None) => None,
        (Some("session/request_permission"), Some(id)) => {
            Some(Incoming::Permission { id: id.clone(), params: message["params"].clone() })
        }
        (Some(method), Some(id)) => Some(Incoming::Unsupported { id: id.clone(), method: method.to_string() }),
        (None, Some(id)) => {
            let id = id.as_u64()?;
            let result = match message.get("error") {
                Some(error) => Err(error["message"].as_str().unwrap_or("the agent reported an error").to_string()),
                None => Ok(message["result"].clone()),
            };
            Some(Incoming::Response { id, result })
        }
        (None, None) => None,
    }
}

/// Reads the agent's output on a thread of its own, handing each message to
/// `tx` and calling `wake` so that the session loop picks it up.
pub fn read(
    from: impl BufRead + Send + 'static,
    paths: Option<PathMap>,
    tx: Sender<Incoming>,
    wake: impl Fn() + Send + 'static,
) {
    thread::spawn(move || {
        for line in from.lines() {
            let Ok(line) = line else { break };
            let line = match &paths {
                Some(paths) => paths.to_host(&line),
                None => line,
            };
            if let Some(message) = parse(&line) {
                if tx.send(message).is_err() {
                    return;
                }
                wake();
            }
        }
        let _ = tx.send(Incoming::Closed);
        wake();
    });
}

pub struct Client {
    to: Box<dyn Write + Send>,
    paths: Option<PathMap>,
    next_id: u64,
    calls: HashMap<u64, Call>,
    /// sand's own MCP servers, for every session opened.
    pub servers: Value,
}

impl Client {
    pub fn new(to: Box<dyn Write + Send>, paths: Option<PathMap>) -> Self {
        Self { to, paths, next_id: 1, calls: HashMap::new(), servers: json!([]) }
    }

    fn send(&mut self, message: &Value) -> io::Result<()> {
        let line = message.to_string();
        let line = match &self.paths {
            Some(paths) => paths.to_sandbox(&line),
            None => line,
        };
        self.to.write_all(line.as_bytes())?;
        self.to.write_all(b"\n")?;
        self.to.flush()
    }

    pub fn request(&mut self, call: Call, params: Value) -> io::Result<()> {
        let id = self.next_id;
        self.next_id += 1;
        self.calls.insert(id, call);
        self.send(&json!({"jsonrpc": "2.0", "id": id, "method": call.method(), "params": params}))
    }

    /// Which request a response answers (each is answered once).
    pub fn call(&mut self, id: u64) -> Option<Call> {
        self.calls.remove(&id)
    }

    pub fn notify(&mut self, method: &str, params: Value) -> io::Result<()> {
        self.send(&json!({"jsonrpc": "2.0", "method": method, "params": params}))
    }

    pub fn respond(&mut self, id: Value, result: Value) -> io::Result<()> {
        self.send(&json!({"jsonrpc": "2.0", "id": id, "result": result}))
    }

    pub fn respond_unsupported(&mut self, id: Value, method: &str) -> io::Result<()> {
        self.send(&json!({"jsonrpc": "2.0", "id": id,
            "error": {"code": -32601, "message": format!("sand does not offer {method}")}}))
    }

    pub fn initialize(&mut self) -> io::Result<()> {
        self.request(
            Call::Initialize,
            json!({
                "protocolVersion": 1,
                "clientCapabilities": {"fs": {"readTextFile": false, "writeTextFile": false}, "terminal": false},
                "clientInfo": {"name": "sand", "version": env!("CARGO_PKG_VERSION")},
            }),
        )
    }

    pub fn new_session(&mut self, cwd: &str) -> io::Result<()> {
        self.request(Call::NewSession, json!({"cwd": cwd, "mcpServers": self.servers}))
    }

    /// Sends a prompt: its images (as ACP image blocks), then its text.
    pub fn prompt(&mut self, session: &str, prompt: &Prompt) -> io::Result<()> {
        use base64::Engine as _;
        let mut blocks: Vec<Value> = prompt
            .images
            .iter()
            .map(|image| {
                let data = base64::engine::general_purpose::STANDARD.encode(image.data.as_slice());
                json!({"type": "image", "mimeType": image.mime, "data": data})
            })
            .collect();
        blocks.extend(prompt.context.iter().map(|note| json!({"type": "text", "text": note})));
        // Each mention in its place, as an editor's links sit in its text.
        let mut at = 0;
        for link in &prompt.links {
            if link.range.start > at {
                blocks.push(json!({"type": "text", "text": prompt.text[at..link.range.start]}));
            }
            blocks.push(match &link.text {
                Some(text) => json!({"type": "resource", "resource": {"uri": link.uri, "text": text}}),
                None => json!({"type": "resource_link", "uri": link.uri, "name": link.path}),
            });
            at = link.range.end;
        }
        if at < prompt.text.len() || blocks.is_empty() {
            blocks.push(json!({"type": "text", "text": prompt.text[at..]}));
        }
        self.request(Call::Prompt, json!({"sessionId": session, "prompt": blocks}))
    }

    pub fn set_mode(&mut self, session: &str, mode: &str) -> io::Result<()> {
        self.request(Call::SetMode, json!({"sessionId": session, "modeId": mode}))
    }

    pub fn set_model(&mut self, session: &str, model: &str) -> io::Result<()> {
        self.request(Call::SetModel, json!({"sessionId": session, "modelId": model}))
    }

    pub fn set_config(&mut self, session: &str, option: &str, value: &str) -> io::Result<()> {
        self.request(Call::SetConfig, json!({"sessionId": session, "configId": option, "value": value}))
    }

    pub fn list_sessions(&mut self, cwd: &str) -> io::Result<()> {
        self.request(Call::ListSessions, json!({"cwd": cwd}))
    }

    /// Resumes `session`: `session/load` replays its conversation,
    /// `session/resume` (for an agent that cannot load) does not.
    pub fn open_session(&mut self, session: &str, cwd: &str, load: bool) -> io::Result<()> {
        let call = if load { Call::LoadSession } else { Call::ResumeSession };
        self.request(call, json!({"sessionId": session, "cwd": cwd, "mcpServers": self.servers}))
    }

    /// Copies `session`, its conversation so far, into a new session.
    pub fn fork_session(&mut self, session: &str, cwd: &str) -> io::Result<()> {
        self.request(Call::ForkSession, json!({"sessionId": session, "cwd": cwd, "mcpServers": self.servers}))
    }

    pub fn cancel(&mut self, session: &str) -> io::Result<()> {
        self.notify("session/cancel", json!({"sessionId": session}))
    }

    /// Answers a permission request with the option picked, or as cancelled.
    pub fn permit(&mut self, id: Value, option: Option<&str>) -> io::Result<()> {
        let outcome = match option {
            Some(option) => json!({"outcome": "selected", "optionId": option}),
            None => json!({"outcome": "cancelled"}),
        };
        self.respond(id, json!({"outcome": outcome}))
    }
}

#[cfg(test)]
mod tests {
    use std::sync::{Arc, Mutex};

    use super::*;

    #[test]
    fn messages_are_told_apart() {
        let update = r#"{"jsonrpc":"2.0","method":"session/update","params":{"sessionId":"s","update":{"sessionUpdate":"plan"}}}"#;
        assert_eq!(parse(update), Some(Incoming::Update(json!({"sessionUpdate": "plan"}))));
        let ask = r#"{"jsonrpc":"2.0","id":"a1","method":"session/request_permission","params":{"options":[]}}"#;
        assert_eq!(parse(ask), Some(Incoming::Permission { id: json!("a1"), params: json!({"options": []}) }));
        let read = r#"{"jsonrpc":"2.0","id":3,"method":"fs/read_text_file","params":{}}"#;
        assert_eq!(parse(read), Some(Incoming::Unsupported { id: json!(3), method: "fs/read_text_file".into() }));
        assert_eq!(
            parse(r#"{"jsonrpc":"2.0","id":2,"result":{"sessionId":"s"}}"#),
            Some(Incoming::Response { id: 2, result: Ok(json!({"sessionId": "s"})) })
        );
        assert_eq!(
            parse(r#"{"jsonrpc":"2.0","id":2,"error":{"code":-32000,"message":"Authentication required"}}"#),
            Some(Incoming::Response { id: 2, result: Err("Authentication required".into()) })
        );
        assert_eq!(parse("npm WARN deprecated"), None);
        assert_eq!(parse(r#"{"jsonrpc":"2.0","method":"other/notification"}"#), None);
    }

    #[derive(Clone, Default)]
    struct Shared(Arc<Mutex<Vec<u8>>>);

    impl Write for Shared {
        fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
            self.0.lock().unwrap().extend_from_slice(buf);
            Ok(buf.len())
        }
        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    #[test]
    fn requests_are_numbered_and_answered_once() {
        let out = Shared::default();
        let mut client = Client::new(Box::new(out.clone()), None);
        client.initialize().unwrap();
        client.new_session("/w").unwrap();
        client.permit(json!("a1"), Some("yes")).unwrap();
        client.permit(json!(9), None).unwrap();
        let text = String::from_utf8(out.0.lock().unwrap().clone()).unwrap();
        let lines: Vec<Value> = text.lines().map(|l| serde_json::from_str(l).unwrap()).collect();
        assert_eq!(lines[0]["method"], "initialize");
        assert_eq!(lines[0]["params"]["clientCapabilities"]["terminal"], false);
        assert_eq!((lines[1]["id"].clone(), lines[1]["params"]["cwd"].clone()), (json!(2), json!("/w")));
        assert_eq!(
            lines[2],
            json!({"jsonrpc": "2.0", "id": "a1", "result": {"outcome": {"outcome": "selected", "optionId": "yes"}}})
        );
        assert_eq!(lines[3]["result"]["outcome"], json!({"outcome": "cancelled"}));
        assert_eq!(client.call(2), Some(Call::NewSession));
        assert_eq!(client.call(2), None);
    }

    #[test]
    fn images_go_before_the_text_as_image_blocks() {
        let out = Shared::default();
        let mut client = Client::new(Box::new(out.clone()), None);
        let image = super::super::model::Image {
            data: std::sync::Arc::new(b"png!".to_vec()),
            mime: "image/png".into(),
            name: "Image 1".into(),
            dims: None,
            blob: None,
        };
        let prompt = Prompt { text: "what is this?".into(), images: vec![image.clone()], ..Prompt::default() };
        client.prompt("S", &prompt).unwrap();
        client.prompt("S", &Prompt { images: vec![image], ..Prompt::default() }).unwrap();
        let text = String::from_utf8(out.0.lock().unwrap().clone()).unwrap();
        let lines: Vec<Value> = text.lines().map(|l| serde_json::from_str(l).unwrap()).collect();
        assert_eq!(
            lines[0]["params"]["prompt"],
            json!([{"type": "image", "mimeType": "image/png", "data": "cG5nIQ=="},
                   {"type": "text", "text": "what is this?"}])
        );
        assert_eq!(lines[1]["params"]["prompt"].as_array().map(Vec::len), Some(1), "an image alone needs no text");
    }

    #[test]
    fn mentions_go_in_their_place_as_resources() {
        let out = Shared::default();
        let mut client = Client::new(Box::new(out.clone()), None);
        let link = |range, path: &str, text: Option<&str>| super::super::files::Link {
            range,
            path: path.into(),
            uri: format!("file:///w/{path}"),
            text: text.map(str::to_string),
        };
        let text = "fix @a.rs and @src/";
        let links = vec![link(4..9, "a.rs", Some("fn a() {}")), link(14..19, "src/", None)];
        client.prompt("S", &Prompt { text: text.into(), links, ..Prompt::default() }).unwrap();
        let text = String::from_utf8(out.0.lock().unwrap().clone()).unwrap();
        let line: Value = serde_json::from_str(text.lines().next().unwrap()).unwrap();
        assert_eq!(
            line["params"]["prompt"],
            json!([{"type": "text", "text": "fix "},
                   {"type": "resource", "resource": {"uri": "file:///w/a.rs", "text": "fn a() {}"}},
                   {"type": "text", "text": " and "},
                   {"type": "resource_link", "uri": "file:///w/src/", "name": "src/"}])
        );
    }
}

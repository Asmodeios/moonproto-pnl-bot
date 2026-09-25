//! The few Bot API calls the bot makes, over plain HTTPS long polling.
//!
//! The Bot API puts the token in the URL path, and `reqwest::Error`'s
//! Display prints the URL — every transport error goes through
//! `without_url()` before it is formatted anywhere.

use std::time::Duration;

use reqwest::multipart::{Form, Part};
use serde::de::DeserializeOwned;
use serde::Deserialize;
use serde_json::{json, Value};

/// Long-poll window; the HTTP timeout sits above it.
const POLL_SECS: u64 = 50;

/// No `Debug`: `base` holds the token.
pub struct Tg {
    http: reqwest::Client,
    base: String,
}

#[derive(Deserialize)]
struct Envelope<T> {
    ok: bool,
    result: Option<T>,
    description: Option<String>,
}

#[derive(Deserialize)]
pub struct Update {
    pub update_id: i64,
    pub message: Option<Message>,
    pub callback_query: Option<CallbackQuery>,
}

#[derive(Deserialize)]
pub struct Message {
    pub message_id: i64,
    pub chat: Chat,
    pub from: Option<User>,
    pub text: Option<String>,
    /// Present on a photo message, whose text is a caption.
    pub photo: Option<Vec<Value>>,
}

#[derive(Deserialize)]
pub struct Chat {
    pub id: i64,
    #[serde(rename = "type")]
    pub kind: String,
}

#[derive(Deserialize)]
pub struct User {
    pub id: i64,
    pub username: Option<String>,
}

#[derive(Deserialize)]
pub struct CallbackQuery {
    pub id: String,
    pub from: User,
    pub message: Option<Message>,
    pub data: Option<String>,
}

impl Tg {
    pub fn new(token: &str) -> Self {
        let http = reqwest::Client::builder()
            .timeout(Duration::from_secs(POLL_SECS + 15))
            .build()
            .unwrap_or_default();
        Self { http, base: format!("https://api.telegram.org/bot{token}") }
    }

    async fn call<T: DeserializeOwned>(&self, method: &str, body: Value) -> Result<T, String> {
        self.post(method, self.http.post(format!("{}/{method}", self.base)).json(&body)).await
    }

    /// A call with a file upload; `fields` are the JSON body's.
    async fn call_upload<T: DeserializeOwned>(&self, method: &str, fields: Value, png: Vec<u8>) -> Result<T, String> {
        let part = Part::bytes(png).file_name("pnl.png").mime_str("image/png").map_err(|e| format!("{method}: {e}"))?;
        let mut form = Form::new().part("photo", part);
        for (k, v) in fields.as_object().into_iter().flatten() {
            form = form.text(k.clone(), v.as_str().map_or_else(|| v.to_string(), str::to_string));
        }
        self.post(method, self.http.post(format!("{}/{method}", self.base)).multipart(form)).await
    }

    async fn post<T: DeserializeOwned>(&self, method: &str, req: reqwest::RequestBuilder) -> Result<T, String> {
        let res = req.send().await.map_err(|e| format!("{method}: {}", e.without_url()))?;
        let env: Envelope<T> = res.json().await.map_err(|e| format!("{method}: {}", e.without_url()))?;
        match (env.ok, env.result) {
            (true, Some(result)) => Ok(result),
            _ => Err(format!("{method}: {}", env.description.unwrap_or_else(|| "no result".to_string()))),
        }
    }

    pub async fn get_me(&self) -> Result<User, String> {
        self.call("getMe", json!({})).await
    }

    pub async fn get_updates(&self, offset: i64) -> Result<Vec<Update>, String> {
        self.call(
            "getUpdates",
            json!({ "offset": offset, "timeout": POLL_SECS, "allowed_updates": ["message", "callback_query"] }),
        )
        .await
    }

    pub async fn send(&self, chat: i64, text: &str, keyboard: Option<Value>) -> Result<(), String> {
        self.send_id(chat, text, keyboard).await.map(|_| ())
    }

    /// `send`, answering the new message's id.
    pub async fn send_id(&self, chat: i64, text: &str, keyboard: Option<Value>) -> Result<i64, String> {
        let mut body = json!({ "chat_id": chat, "text": text, "parse_mode": "HTML" });
        with_keyboard(&mut body, keyboard);
        self.call::<Message>("sendMessage", body).await.map(|m| m.message_id)
    }

    /// Edit a message in place; an unchanged text is not an error.
    pub async fn edit(&self, chat: i64, message_id: i64, text: &str, keyboard: Option<Value>) -> Result<(), String> {
        let mut body = json!({ "chat_id": chat, "message_id": message_id, "text": text, "parse_mode": "HTML" });
        with_keyboard(&mut body, keyboard);
        match self.call::<Value>("editMessageText", body).await {
            Err(e) if e.contains("message is not modified") => Ok(()),
            other => other.map(|_| ()),
        }
    }

    /// Send a PNG as a photo, answering the new message's id.
    pub async fn send_photo(&self, chat: i64, png: Vec<u8>, caption: &str, keyboard: Option<Value>) -> Result<i64, String> {
        let mut fields = json!({ "chat_id": chat, "caption": caption, "parse_mode": "HTML" });
        with_keyboard(&mut fields, keyboard);
        self.call_upload::<Message>("sendPhoto", fields, png).await.map(|m| m.message_id)
    }

    /// Swap a photo message's image and caption in place.
    pub async fn edit_photo(
        &self,
        chat: i64,
        message_id: i64,
        png: Vec<u8>,
        caption: &str,
        keyboard: Option<Value>,
    ) -> Result<(), String> {
        let media = json!({ "type": "photo", "media": "attach://photo", "caption": caption, "parse_mode": "HTML" });
        let mut fields = json!({ "chat_id": chat, "message_id": message_id, "media": media });
        with_keyboard(&mut fields, keyboard);
        self.call_upload::<Value>("editMessageMedia", fields, png).await.map(|_| ())
    }

    pub async fn answer_callback(&self, id: &str, text: Option<&str>) {
        let mut body = json!({ "callback_query_id": id });
        if let Some(t) = text {
            body["text"] = json!(t);
        }
        if let Err(e) = self.call::<Value>("answerCallbackQuery", body).await {
            log::warn!("{e}");
        }
    }

    pub async fn delete_message(&self, chat: i64, message_id: i64) -> Result<(), String> {
        self.call::<Value>("deleteMessage", json!({ "chat_id": chat, "message_id": message_id }))
            .await
            .map(|_| ())
    }

    /// Up to 100 messages of one chat in one call; ones already gone are skipped.
    pub async fn delete_messages(&self, chat: i64, message_ids: &[i64]) -> Result<(), String> {
        self.call::<Value>("deleteMessages", json!({ "chat_id": chat, "message_ids": message_ids }))
            .await
            .map(|_| ())
    }

    pub async fn set_commands(&self, commands: &[(&str, &str)]) -> Result<(), String> {
        let list: Vec<Value> = commands.iter().map(|(c, d)| json!({ "command": c, "description": d })).collect();
        self.call::<Value>("setMyCommands", json!({ "commands": list })).await.map(|_| ())
    }
}

fn with_keyboard(body: &mut Value, keyboard: Option<Value>) {
    if let Some(k) = keyboard {
        body["reply_markup"] = json!({ "inline_keyboard": k });
    }
}

/// One inline button.
pub fn button(text: &str, data: &str) -> Value {
    json!({ "text": text, "callback_data": data })
}

pub fn escape(s: &str) -> String {
    s.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;")
}

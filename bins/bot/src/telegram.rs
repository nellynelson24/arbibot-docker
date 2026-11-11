use std::collections::HashSet;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result, anyhow};
use serde_json::{Value, json};
use tokio::sync::{mpsc, watch};
use tokio::task::JoinHandle;
use tokio::time::{sleep, timeout};

const API: &str = "https://api.telegram.org";
const SERVICE: &str = "federbot-telegram";
const POLL_SECS: u64 = 50;
const SEND_TIMEOUT: Duration = Duration::from_secs(15);
const SEND_RETRIES: u32 = 3;
const SEND_RETRY: Duration = Duration::from_secs(2);
const POLL_RETRY: Duration = Duration::from_secs(10);
const CONFLICT_RETRY: Duration = Duration::from_secs(60);
const MAX_CHARS: usize = 4096;
const PENDING_STOP_SECS: i64 = 300;
const CONFLICT_NOTE: &str = "Polecenia /stan i /stop chwilowo nie działają: Telegram podaje, że \
     odbiera je inny proces z tym samym tokenem (kod 409). Ponawiam co minutę.";
const HELP: &str = "Polecenia: /stan — stan bota, /stop — zatrzymanie handlu (plik STOP)";
const VIEWER_HELP: &str = "Podgląd bota federbot. Polecenie: /stan — stan bota";

#[derive(clap::Args)]
pub struct Args {}

#[derive(Debug)]
struct ApiError {
    method: String,
    code: i64,
    description: String,
    retry_after: Option<u64>,
}

impl std::fmt::Display for ApiError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "Telegram {}: kod {} {}",
            self.method, self.code, self.description
        )
    }
}

impl std::error::Error for ApiError {}

pub struct Telegram {
    http: reqwest::Client,
    poll: reqwest::Client,
    token: String,
    chat_id: String,
    viewers: Vec<String>,
}

pub struct Notifier {
    tx: mpsc::UnboundedSender<String>,
    status: watch::Sender<String>,
    sender: JoinHandle<()>,
}

impl Notifier {
    pub fn send(&self, text: impl Into<String>) {
        let _ = self.tx.send(text.into());
    }

    pub fn sender(&self) -> mpsc::UnboundedSender<String> {
        self.tx.clone()
    }

    pub fn set_status(&self, text: String) {
        self.status.send_replace(text);
    }

    pub async fn close(self) {
        drop(self.tx);
        let _ = timeout(Duration::from_secs(5), self.sender).await;
    }
}

impl Telegram {
    pub fn load() -> Result<Self> {
        let token = account::secret(SERVICE, "token")
            .context("brak tokenu bota Telegram — instrukcja: bot telegram")?;
        let chat_id = account::secret(SERVICE, "chat_id")
            .context("brak numeru czatu Telegram — instrukcja: bot telegram")?;
        Ok(Telegram {
            http: client(SEND_TIMEOUT)?,
            poll: client(POLL_TIMEOUT)?,
            token,
            chat_id,
            viewers: viewers(),
        })
    }

    pub fn start(self, label: &'static str, stop_file: PathBuf) -> Notifier {
        let telegram = Arc::new(self);
        let (tx, mut rx) = mpsc::unbounded_channel::<String>();
        let (status, status_rx) = watch::channel("bot jeszcze nie podał stanu".to_owned());
        let sending = telegram.clone();
        let sender = tokio::spawn(async move {
            while let Some(text) = rx.recv().await {
                sending.broadcast(&format!("[{label}] {text}")).await;
            }
        });
        tokio::spawn(commands(telegram, status_rx, stop_file));
        Notifier { tx, status, sender }
    }

    pub async fn send_report(&self, text: &str) {
        self.broadcast(text).await;
    }

    async fn send(&self, text: &str) -> Result<()> {
        self.send_to(&self.chat_id, text).await
    }

    async fn send_to(&self, chat: &str, text: &str) -> Result<()> {
        let body = json!({"chat_id": chat, "text": clip(text)});
        let mut attempt = 0;
        loop {
            match self.call("sendMessage", body.clone()).await {
                Ok(_) => return Ok(()),
                Err(e) => match retry_in(&e, SEND_RETRY) {
                    Some(wait) if attempt < SEND_RETRIES => {
                        attempt += 1;
                        sleep(wait).await;
                    }
                    _ => return Err(e),
                },
            }
        }
    }

    async fn broadcast(&self, text: &str) {
        for chat in std::iter::once(&self.chat_id).chain(&self.viewers) {
            if let Err(e) = self.send_to(chat, text).await {
                eprintln!("Telegram ({chat}): {e:#}");
            }
        }
    }

    async fn call(&self, method: &str, body: Value) -> Result<Value> {
        let client = match method {
            "getUpdates" => &self.poll,
            _ => &self.http,
        };
        let reply = client
            .post(format!("{API}/bot{}/{method}", self.token))
            .header("Content-Type", "application/json")
            .body(body.to_string())
            .send()
            .await
            .map_err(|e| anyhow!("Telegram {method}: {}", e.without_url()))?
            .text()
            .await
            .map_err(|e| anyhow!("Telegram {method}: {}", e.without_url()))?;
        let v: Value = serde_json::from_str(&reply)
            .with_context(|| format!("Telegram {method}: odpowiedź nie jest JSON-em"))?;
        if v["ok"] != true {
            return Err(ApiError {
                method: method.to_owned(),
                code: v["error_code"].as_i64().unwrap_or(0),
                description: v["description"].as_str().unwrap_or_default().to_owned(),
                retry_after: v["parameters"]["retry_after"].as_u64(),
            }
            .into());
        }
        Ok(v["result"].clone())
    }
}

async fn commands(telegram: Arc<Telegram>, status: watch::Receiver<String>, stop_file: PathBuf) {
    let owner: Option<i64> = telegram.chat_id.trim().parse().ok();
    let mut conflict_noted = false;
    let pending = loop {
        match telegram.call("getUpdates", json!({"timeout": 0})).await {
            Ok(updates) => break updates,
            Err(e) => {
                note_poll_error(&telegram, &e, &mut conflict_noted).await;
                sleep(retry_in(&e, POLL_RETRY).unwrap_or(POLL_RETRY)).await;
            }
        }
    };
    let mut offset = pending
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|u| u["update_id"].as_i64())
        .max()
        .map_or(0, |id| id + 1);
    if pending_stop(&pending, owner, (crate::ledger::now_ms() / 1000.0) as i64) {
        let reply = match std::fs::write(&stop_file, "") {
            Ok(()) => "Wykonano /stop wysłane w trakcie restartu: handel wstrzymany (plik STOP)."
                .to_owned(),
            Err(e) => format!("Nie udało się utworzyć pliku STOP: {e}"),
        };
        if let Err(e) = telegram.send(&reply).await {
            eprintln!("Telegram: {e:#}");
        }
    }
    let viewers: Vec<i64> = telegram
        .viewers
        .iter()
        .filter_map(|v| v.parse().ok())
        .collect();
    let mut reported: HashSet<i64> = HashSet::new();
    loop {
        let updates = match telegram
            .call(
                "getUpdates",
                json!({"offset": offset, "timeout": POLL_SECS, "allowed_updates": ["message"]}),
            )
            .await
        {
            Ok(updates) => updates,
            Err(e) => {
                note_poll_error(&telegram, &e, &mut conflict_noted).await;
                sleep(retry_in(&e, POLL_RETRY).unwrap_or(POLL_RETRY)).await;
                continue;
            }
        };
        if conflict_noted {
            conflict_noted = false;
            if let Err(e) = telegram
                .send("Polecenia /stan i /stop znowu działają.")
                .await
            {
                eprintln!("Telegram: {e:#}");
            }
        }
        for update in updates.as_array().into_iter().flatten() {
            offset = offset.max(update["update_id"].as_i64().unwrap_or(0) + 1);
            let message = &update["message"];
            let Some(chat) = message["chat"]["id"].as_i64() else {
                continue;
            };
            let is_owner = Some(chat) == owner;
            let may_stop = message["from"]["id"].as_i64() == owner;
            if !is_owner && !viewers.contains(&chat) {
                if reported.insert(chat) {
                    let name = message["chat"]["username"]
                        .as_str()
                        .or(message["chat"]["first_name"].as_str())
                        .unwrap_or("?");
                    let note = format!(
                        "Do bota napisał(a) {name} (czat {chat}). Żeby dostawał(a) podgląd, dopisz \
                         ten numer do listy widzów w pęku kluczy ({SERVICE} / viewers) i uruchom \
                         bota ponownie."
                    );
                    println!("Telegram: {note}");
                    if let Err(e) = telegram.send(&note).await {
                        eprintln!("Telegram: {e:#}");
                    }
                }
                continue;
            }
            let Some(text) = message["text"].as_str() else {
                continue;
            };
            let command = text
                .split_whitespace()
                .next()
                .and_then(|c| c.split('@').next())
                .unwrap_or_default();
            let reply = match command {
                "/stan" | "/status" => status.borrow().clone(),
                "/stop" if !may_stop => "Tylko właściciel bota może go zatrzymać.".to_owned(),
                "/stop" => match std::fs::write(&stop_file, "") {
                    Ok(()) => "Zatrzymuję handel (plik STOP). Wznowienie: usuń plik data/STOP \
                               i uruchom bota ponownie."
                        .to_owned(),
                    Err(e) => format!("Nie udało się utworzyć pliku STOP: {e}"),
                },
                _ if is_owner => HELP.to_owned(),
                _ => VIEWER_HELP.to_owned(),
            };
            if let Err(e) = telegram.send_to(&chat.to_string(), &reply).await {
                eprintln!("Telegram: {e:#}");
            }
        }
    }
}

fn pending_stop(updates: &Value, owner: Option<i64>, now: i64) -> bool {
    updates.as_array().into_iter().flatten().any(|u| {
        let m = &u["message"];
        m["from"]["id"].as_i64() == owner
            && owner.is_some()
            && m["date"]
                .as_i64()
                .is_some_and(|d| now - d <= PENDING_STOP_SECS)
            && m["text"]
                .as_str()
                .and_then(|t| t.split_whitespace().next())
                .and_then(|c| c.split('@').next())
                == Some("/stop")
    })
}

fn viewers() -> Vec<String> {
    account::secret(SERVICE, "viewers")
        .map(|list| {
            list.split([',', ' ', ';'])
                .map(str::trim)
                .filter(|id| id.trim_start_matches('-').parse::<u64>().is_ok())
                .map(str::to_owned)
                .collect()
        })
        .unwrap_or_default()
}

const POLL_TIMEOUT: Duration = Duration::from_secs(POLL_SECS + 20);

fn client(timeout: Duration) -> Result<reqwest::Client> {
    Ok(reqwest::Client::builder().timeout(timeout).build()?)
}

async fn note_poll_error(telegram: &Telegram, e: &anyhow::Error, conflict_noted: &mut bool) {
    if !is_conflict(e) {
        eprintln!("Telegram (polecenia): {e:#}");
        return;
    }
    if *conflict_noted {
        return;
    }
    *conflict_noted = true;
    eprintln!("Telegram: polecenia odbiera inny proces bota");
    if let Err(e) = telegram.send(CONFLICT_NOTE).await {
        eprintln!("Telegram: {e:#}");
    }
}

fn is_conflict(e: &anyhow::Error) -> bool {
    e.downcast_ref::<ApiError>().is_some_and(|a| a.code == 409)
}

fn retry_in(e: &anyhow::Error, fallback: Duration) -> Option<Duration> {
    match e.downcast_ref::<ApiError>() {
        Some(api) if api.code == 429 => {
            Some(Duration::from_secs(api.retry_after.unwrap_or(5)).min(CONFLICT_RETRY))
        }
        Some(api) if api.code == 409 => Some(CONFLICT_RETRY),
        Some(api) if api.code >= 500 => Some(fallback),
        Some(_) => None,
        None => Some(fallback),
    }
}

fn clip(text: &str) -> String {
    if text.chars().count() <= MAX_CHARS {
        return text.to_owned();
    }
    let mut cut: String = text.chars().take(MAX_CHARS - 1).collect();
    cut.push('…');
    cut
}

pub async fn setup(_: Args) -> Result<()> {
    let Ok(token) = account::secret(SERVICE, "token") else {
        println!(
            "Brak tokenu bota.\n\
             1. W Telegramie napisz do @BotFather: /newbot, nadaj nazwę; dostaniesz token.\n\
             2. Zapisz go (komenda zapyta o wartość):\n   \
             security add-generic-password -s {SERVICE} -a token -w\n\
             3. Napisz cokolwiek do swojego nowego bota i uruchom ponownie: ./target/release/bot telegram"
        );
        return Ok(());
    };
    let mut telegram = Telegram {
        http: client(SEND_TIMEOUT)?,
        poll: client(POLL_TIMEOUT)?,
        token,
        chat_id: String::new(),
        viewers: viewers(),
    };
    let me = telegram.call("getMe", json!({})).await?;
    let link = format!(
        "https://t.me/{}",
        me["username"].as_str().unwrap_or_default()
    );
    let stored = account::secret(SERVICE, "chat_id").ok();
    if let Some(chat_id) = &stored {
        telegram.chat_id = chat_id.clone();
        match telegram
            .send(&format!("federbot: połączenie działa. {HELP}"))
            .await
        {
            Ok(()) => {
                println!("Wysłano wiadomość testową do Ciebie.");
                for viewer in &telegram.viewers {
                    match telegram
                        .send_to(
                            viewer,
                            &format!("federbot: masz podgląd bota. {VIEWER_HELP}"),
                        )
                        .await
                    {
                        Ok(()) => println!("Wysłano wiadomość testową do widza {viewer}."),
                        Err(e) => println!(
                            "Widz {viewer}: nie doszło ({e:#}) — musi najpierw otworzyć {link} i nacisnąć Start"
                        ),
                    }
                }
                println!(
                    "\nPodgląd dla innej osoby: niech otworzy {link} i naciśnie Start. Działający bot \
                     z --telegram napisze Ci wtedy numer jej czatu. Dopisz go do widzów \
                     (numery po przecinku; komenda zapyta o wartość):\n  \
                     security add-generic-password -U -s {SERVICE} -a viewers -w\n\
                     i uruchom bota ponownie."
                );
                return Ok(());
            }
            Err(e) => println!("Wiadomość testowa nie doszła: {e:#}"),
        }
    }
    let updates = telegram.call("getUpdates", json!({"timeout": 0})).await?;
    let mut chats: Vec<(String, String)> = Vec::new();
    for update in updates.as_array().into_iter().flatten() {
        let chat = &update["message"]["chat"];
        let id = chat["id"].to_string();
        if chat["id"].is_null() || chats.iter().any(|(c, _)| *c == id) {
            continue;
        }
        let name = chat["username"]
            .as_str()
            .or(chat["first_name"].as_str())
            .unwrap_or("?")
            .to_owned();
        chats.push((id, name));
    }
    if let Some(chat_id) = &stored {
        let digits = chat_id.trim().trim_start_matches('-');
        let looks_numeric = !digits.is_empty() && digits.chars().all(|c| c.is_ascii_digit());
        println!(
            "Zapisany numer czatu {}.",
            if !looks_numeric {
                "nie jest liczbą (to ma być sam numer, bez nazwy bota)"
            } else if chats.iter().any(|(id, _)| id == chat_id.trim()) {
                "pasuje do czatu poniżej, ale Telegram go nie przyjął"
            } else {
                "nie pasuje do żadnego czatu, który pisał do bota"
            }
        );
        println!(
            "Usuń go i zapisz ponownie:\n  security delete-generic-password -s {SERVICE} -a chat_id"
        );
    }
    if chats.is_empty() {
        println!(
            "Telegram nie ma wiadomości do bota. Napisz cokolwiek do swojego bota (tego, którego \
             token jest zapisany) i uruchom tę komendę ponownie."
        );
        return Ok(());
    }
    for (id, name) in &chats {
        println!("Czat {id} ({name})");
    }
    println!(
        "Zapisz numer swojego czatu (komenda zapyta o wartość, wpisz {}):\n  \
         security add-generic-password -s {SERVICE} -a chat_id -w\n\
         Potem uruchom ponownie ./target/release/bot telegram — przyjdzie wiadomość testowa.",
        chats[0].0
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn api(code: i64, retry_after: Option<u64>) -> anyhow::Error {
        ApiError {
            method: "sendMessage".to_owned(),
            code,
            description: String::new(),
            retry_after,
        }
        .into()
    }

    #[test]
    fn failed_calls_are_retried_only_when_it_can_help() {
        let fallback = Duration::from_secs(2);
        assert_eq!(
            retry_in(&api(429, Some(7)), fallback),
            Some(Duration::from_secs(7))
        );
        assert_eq!(retry_in(&api(409, None), fallback), Some(CONFLICT_RETRY));
        assert_eq!(retry_in(&api(502, None), fallback), Some(fallback));
        assert_eq!(retry_in(&api(400, None), fallback), None);
        assert_eq!(retry_in(&api(403, None), fallback), None);
        assert_eq!(
            retry_in(&anyhow!("Telegram sendMessage: timeout"), fallback),
            Some(fallback)
        );
        assert!(is_conflict(&api(409, None)));
        assert!(!is_conflict(&api(429, None)));
    }

    #[test]
    fn a_recent_stop_from_the_owner_survives_a_restart() {
        let msg = |from: i64, text: &str, date: i64| json!({"update_id": 1, "message": {"from": {"id": from}, "chat": {"id": from}, "text": text, "date": date}});
        let now = 1_794_000_000;
        let owner = Some(42);
        assert!(pending_stop(
            &json!([msg(42, "/stop", now - 60)]),
            owner,
            now
        ));
        assert!(pending_stop(
            &json!([msg(42, "/stop@arbibot", now - 60)]),
            owner,
            now
        ));
        assert!(!pending_stop(
            &json!([msg(42, "/stop", now - 600)]),
            owner,
            now
        ));
        assert!(!pending_stop(
            &json!([msg(7, "/stop", now - 60)]),
            owner,
            now
        ));
        assert!(!pending_stop(
            &json!([msg(42, "/stan", now - 60)]),
            owner,
            now
        ));
        assert!(!pending_stop(
            &json!([msg(42, "/stop", now - 60)]),
            None,
            now
        ));
    }

    #[test]
    fn long_messages_are_clipped_to_the_telegram_limit() {
        assert_eq!(clip("krótko"), "krótko");
        let long = "ą".repeat(MAX_CHARS + 10);
        let clipped = clip(&long);
        assert_eq!(clipped.chars().count(), MAX_CHARS);
        assert!(clipped.ends_with('…'));
    }
}

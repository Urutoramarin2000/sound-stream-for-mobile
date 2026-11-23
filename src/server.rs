//! HTTP + WebSocket сервер: отдаёт страницу плеера и рассылает звук.
//!
//! 协议（服务端 → 网页）：
//! - 文本 JSON：`{"type":"live"}`、`{"type":"offline"}`、
//!   `{"type":"format","sampleRate":N,"channels":2,"device":"…","input":false,"fallback":false}`、
//!   `{"type":"choice","id":"…"}`（空 = 跟随默认输出）、
//!   `{"type":"devices","choice":"…","list":[{"id","name","input","default"}]}`、
//!   `{"type":"pong","t":…}`。
//! - 二进制：12 字节包头（见 capture::HEADER）+ 立体声交错 Int16 LE。
//!
//! 网页 → 服务端（纯文本命令，不需要 JSON 解析）：
//! `ping <任意>`、`devices`、`select <设备 ID>`（`select` 不带 ID = 跟随默认输出）。

use std::net::{Ipv4Addr, Ipv6Addr, SocketAddr};
use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::time::Duration;

use axum::extract::ws::rejection::WebSocketUpgradeRejection;
use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
use axum::extract::State;
use axum::http::{header, StatusCode, Uri};
use axum::response::{IntoResponse, Response};
use axum::serve::ListenerExt;
use axum::Router;
use futures_util::{SinkExt, StreamExt};
use socket2::{Domain, Protocol, Socket, Type};
use tokio::sync::broadcast::error::RecvError;

use crate::capture;
use crate::state::{App, Choice, Event, Source};
use crate::tray::{self, Kind};
use crate::{assets, net};

pub fn spawn(app: Arc<App>) {
    std::thread::Builder::new()
        .name("server".into())
        .spawn(move || {
            let rt = tokio::runtime::Builder::new_multi_thread()
                .worker_threads(2)
                .thread_name("server-rt")
                .enable_all()
                .build()
                .expect("tokio runtime");
            rt.block_on(serve(app));
        })
        .expect("server thread");
}

async fn serve(app: Arc<App>) {
    let port = app.cfg.port;

    // Порт может быть занят (например, ещё работает старая версия) — не падаем,
    // а ждём, пока освободится. Первые секунды пробуем часто: это нормальная
    // ситуация при «Перезапустить».
    let mut attempt = 0u32;
    let listener = loop {
        match bind(port) {
            Ok(l) => break l,
            Err(e) => {
                attempt += 1;
                if attempt == 1 {
                    log!("[server] порт {port} занят: {e} — жду, пока освободится");
                }
                if attempt == 10 {
                    app.port_busy.store(true, Ordering::SeqCst);
                    tray::notify(
                        format!("端口 {port} 被占用"),
                        "被其他程序占用（可能是旧版 sound-stream）。关闭它后服务会自动启动。",
                        Kind::Warning,
                    );
                }
                let pause = if attempt < 10 { 300 } else { 2000 };
                tokio::time::sleep(Duration::from_millis(pause)).await;
            }
        }
    };
    app.port_busy.store(false, Ordering::SeqCst);
    app.server_up.store(true, Ordering::SeqCst);
    announce(&app);

    let router = Router::new().fallback(handle).with_state(app.clone());
    // Без Nagle: кадры по 10 мс должны уходить сразу, а не копиться.
    let listener = listener.tap_io(|tcp| {
        let _ = tcp.set_nodelay(true);
    });
    if let Err(e) = axum::serve(listener, router).await {
        log!("[server] остановлен с ошибкой: {e}");
    }
    app.server_up.store(false, Ordering::SeqCst);
}

/// Слушаем и IPv4, и IPv6 (dual-stack), как и прежний сервер на Node.
fn bind(port: u16) -> std::io::Result<tokio::net::TcpListener> {
    let sock = match Socket::new(Domain::IPV6, Type::STREAM, Some(Protocol::TCP)) {
        Ok(s) => {
            s.set_only_v6(false)?;
            s.bind(&SocketAddr::from((Ipv6Addr::UNSPECIFIED, port)).into())?;
            s
        }
        Err(_) => {
            let s = Socket::new(Domain::IPV4, Type::STREAM, Some(Protocol::TCP))?;
            s.bind(&SocketAddr::from((Ipv4Addr::UNSPECIFIED, port)).into())?;
            s
        }
    };
    sock.listen(1024)?;
    sock.set_nonblocking(true)?;
    tokio::net::TcpListener::from_std(sock.into())
}

fn announce(app: &App) {
    let cfg = &app.cfg;
    log!("[server] слушаю порт {}", cfg.port);
    log!("  Постоянный адрес для телефона: {}", cfg.mdns_url());
    for ip in net::lan_ipv4() {
        log!("  По IP (запасной вариант): {}", cfg.ip_url(ip));
    }

    // Окно открыто и само показывает адрес — уведомление лишнее.
    if app.show_window && !app.migrated {
        return;
    }
    let title = if app.restarted {
        "sound-stream 已重启"
    } else {
        "sound-stream 已启动"
    };
    let mut text = format!("收听地址：{}", cfg.mdns_url());
    if app.migrated {
        text.push_str("\n开机自启已改为 sound-stream.exe。");
    }
    tray::notify(title, text, Kind::Info);
}

async fn handle(
    State(app): State<Arc<App>>,
    ws: Result<WebSocketUpgrade, WebSocketUpgradeRejection>,
    uri: Uri,
) -> Response {
    // WebSocket принимаем на любом пути (страница стучится в /?role=listener).
    if let Ok(ws) = ws {
        return ws.on_upgrade(move |socket| listener(socket, app));
    }
    match assets::find(uri.path()) {
        Some(a) => (
            [
                (header::CONTENT_TYPE, a.mime),
                (header::CACHE_CONTROL, a.cache),
            ],
            a.body,
        )
            .into_response(),
        None => (StatusCode::NOT_FOUND, "Not found").into_response(),
    }
}

fn text(s: String) -> Message {
    Message::Text(s.into())
}

fn live() -> Message {
    text(r#"{"type":"live"}"#.into())
}

fn offline() -> Message {
    text(r#"{"type":"offline"}"#.into())
}

fn format_msg(s: &Source) -> Message {
    text(format!(
        r#"{{"type":"format","sampleRate":{},"channels":2,"srcChannels":{},"device":{},"input":{},"fallback":{}}}"#,
        s.sample_rate,
        s.channels,
        json_str(&s.device),
        s.input,
        s.fallback
    ))
}

fn choice_msg(id: &str) -> Message {
    text(format!(r#"{{"type":"choice","id":{}}}"#, json_str(id)))
}

fn devices_msg(app: &App, list: &[capture::DeviceInfo]) -> Message {
    let items: Vec<String> = list
        .iter()
        .map(|d| {
            format!(
                r#"{{"id":{},"name":{},"input":{},"default":{}}}"#,
                json_str(&d.id),
                json_str(&d.name),
                d.input,
                d.default
            )
        })
        .collect();
    let choice = app.choice().map(|c| c.id).unwrap_or_default();
    text(format!(
        r#"{{"type":"devices","choice":{},"list":[{}]}}"#,
        json_str(&choice),
        items.join(",")
    ))
}

/// JSON 字符串字面量（带引号、转义）。
fn json_str(s: &str) -> String {
    let mut o = String::with_capacity(s.len() + 2);
    o.push('"');
    for c in s.chars() {
        match c {
            '"' => o.push_str("\\\""),
            '\\' => o.push_str("\\\\"),
            c if (c as u32) < 0x20 => o.push_str(&format!("\\u{:04x}", c as u32)),
            c => o.push(c),
        }
    }
    o.push('"');
    o
}

async fn device_list() -> Vec<capture::DeviceInfo> {
    tokio::task::spawn_blocking(capture::list_devices)
        .await
        .unwrap_or_default()
}

/// 处理网页发来的文本命令；返回要回给这个网页的消息。
async fn command(app: &App, cmd: &str) -> Option<Message> {
    let cmd = cmd.trim();
    if let Some(t) = cmd.strip_prefix("ping") {
        // 原样带回网页的时间戳，网页据此算往返时间。
        let t: f64 = t.trim().parse().unwrap_or(0.0);
        return Some(text(format!(r#"{{"type":"pong","t":{t}}}"#)));
    }
    if cmd == "devices" {
        let list = device_list().await;
        return Some(devices_msg(app, &list));
    }
    if let Some(id) = cmd.strip_prefix("select") {
        let id = id.trim();
        if id.is_empty() {
            app.set_choice(None);
        } else {
            let list = device_list().await;
            let Some(d) = list.iter().find(|d| d.id == id) else {
                return Some(devices_msg(app, &list));
            };
            app.set_choice(Some(Choice {
                id: d.id.clone(),
                name: d.name.clone(),
            }));
        }
        return None;
    }
    None
}

async fn listener(socket: WebSocket, app: Arc<App>) {
    // Подписываемся ДО того, как смотрим на текущее состояние, — иначе можно
    // пропустить «live», если захват поднимется между этими шагами.
    let mut events = app.events.subscribe();
    let mut audio = app.audio.subscribe();
    let n = app.listener_joined();
    log!("[listener] connected ({n} total)");

    let (mut tx, mut rx) = socket.split();

    // Захват уже идёт — сразу live и формат. Нет — offline; поток захвата
    // поднимется сам и разошлёт live + формат всем.
    let mut hello = match app.source() {
        Some(s) => vec![live(), format_msg(&s)],
        None => vec![offline()],
    };
    hello.push(choice_msg(&app.choice().map(|c| c.id).unwrap_or_default()));
    let mut ok = true;
    for m in hello {
        if tx.send(m).await.is_err() {
            ok = false;
            break;
        }
    }

    while ok {
        tokio::select! {
            msg = rx.next() => match msg {
                None | Some(Err(_)) | Some(Ok(Message::Close(_))) => break,
                Some(Ok(Message::Text(t))) => {
                    if let Some(reply) = command(&app, t.as_str()).await {
                        ok = tx.send(reply).await.is_ok();
                    }
                }
                Some(Ok(_)) => {}
            },
            ev = events.recv() => match ev {
                Ok(Event::Live(s)) => {
                    ok = tx.send(live()).await.is_ok()
                        && tx.send(format_msg(&s)).await.is_ok();
                }
                Ok(Event::Choice(id)) => ok = tx.send(choice_msg(&id)).await.is_ok(),
                Ok(Event::Offline) => ok = tx.send(offline()).await.is_ok(),
                Err(RecvError::Lagged(_)) => {}
                Err(RecvError::Closed) => break,
            },
            frame = audio.recv() => match frame {
                Ok(b) => ok = tx.send(Message::Binary(b)).await.is_ok(),
                // Медленная сеть: старые кадры пропускаем, задержку не копим.
                Err(RecvError::Lagged(_)) => {}
                Err(RecvError::Closed) => break,
            },
        }
    }

    let n = app.listener_left();
    log!("[listener] disconnected ({n} total)");
}

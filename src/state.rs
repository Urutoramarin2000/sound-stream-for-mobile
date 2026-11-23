//! Общее состояние: слушатели, захват, статус сервера. Связывает поток захвата,
//! WebSocket-сервер и интерфейс (окно, трей).
//!
//! 新增：用户选择的采集设备（持久化到 %LOCALAPPDATA%\sound-stream\device.txt）。

use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::{mpsc, Mutex};

use axum::body::Bytes;
use tokio::sync::broadcast;

use crate::Config;

/// Сообщения слушателям (кроме самих PCM-кадров).
#[derive(Clone, Debug)]
pub enum Event {
    /// Захват идёт: формат и устройство.
    Live(Source),
    /// Захват пропал (устройство отвалилось) — ждём восстановления.
    Offline,
    /// 用户换了采集设备（id 为空 = 跟随默认输出）。
    Choice(String),
}

/// Команды потоку захвата.
pub enum CaptureCmd {
    /// Число слушателей изменилось (или сменили устройство) — пересмотреть захват.
    Wake,
    Shutdown,
}

/// 正在采集的来源。
#[derive(Clone, Debug)]
pub struct Source {
    pub device: String,
    pub sample_rate: u32,
    /// 原始声道数（发送时统一为立体声）。
    pub channels: usize,
    /// true = 输入设备（麦克风/线路输入）；false = 输出设备的回环。
    pub input: bool,
    /// 用户选的设备不可用，暂时用默认输出顶替。
    pub fallback: bool,
}

/// 用户选的采集设备；None = 跟随 Windows 默认输出。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Choice {
    pub id: String,
    pub name: String,
}

pub struct App {
    pub cfg: Config,
    /// Запущен пунктом «Перезапустить».
    pub restarted: bool,
    /// При старте старый ярлык автозапуска (Node + PowerShell) заменён на exe.
    pub migrated: bool,
    /// При старте открывается окно (запуск вручную, а не из автозапуска).
    pub show_window: bool,

    /// 音频包（12 字节包头 + 立体声交错 Int16 LE），每次 WASAPI 唤醒一包。
    pub audio: broadcast::Sender<Bytes>,
    pub events: broadcast::Sender<Event>,

    listeners: AtomicUsize,
    source: Mutex<Option<Source>>,
    capture: Mutex<mpsc::Sender<CaptureCmd>>,
    choice: Mutex<Option<Choice>>,
    /// 每次改选设备 +1，采集线程据此判断是否需要重开。
    choice_gen: AtomicU64,

    pub server_up: AtomicBool,
    pub port_busy: AtomicBool,
}

impl App {
    pub fn new(
        cfg: Config,
        restarted: bool,
        migrated: bool,
        show_window: bool,
    ) -> (Self, mpsc::Receiver<CaptureCmd>) {
        // ~0,3 с аудио на слушателя: кто отстал сильнее (медленный Wi-Fi) —
        // пропускает кадры, а не копит задержку.
        let (audio, _) = broadcast::channel(32);
        let (events, _) = broadcast::channel(16);
        let (tx, rx) = mpsc::channel();
        let app = App {
            cfg,
            restarted,
            migrated,
            show_window,
            audio,
            events,
            listeners: AtomicUsize::new(0),
            source: Mutex::new(None),
            capture: Mutex::new(tx),
            choice: Mutex::new(load_choice()),
            choice_gen: AtomicU64::new(0),
            server_up: AtomicBool::new(false),
            port_busy: AtomicBool::new(false),
        };
        (app, rx)
    }

    pub fn listeners(&self) -> usize {
        self.listeners.load(Ordering::SeqCst)
    }

    pub fn listener_joined(&self) -> usize {
        let n = self.listeners.fetch_add(1, Ordering::SeqCst) + 1;
        self.send_capture(CaptureCmd::Wake);
        n
    }

    pub fn listener_left(&self) -> usize {
        let n = self.listeners.fetch_sub(1, Ordering::SeqCst) - 1;
        self.send_capture(CaptureCmd::Wake);
        n
    }

    pub fn shutdown_capture(&self) {
        self.send_capture(CaptureCmd::Shutdown);
    }

    fn send_capture(&self, cmd: CaptureCmd) {
        if let Ok(tx) = self.capture.lock() {
            let _ = tx.send(cmd);
        }
    }

    /// Текущий источник, если захват идёт.
    pub fn source(&self) -> Option<Source> {
        self.source.lock().ok().and_then(|s| s.clone())
    }

    /// 当前选择（None = 跟随默认输出）。
    pub fn choice(&self) -> Option<Choice> {
        self.choice.lock().ok().and_then(|c| c.clone())
    }

    pub fn choice_gen(&self) -> u64 {
        self.choice_gen.load(Ordering::SeqCst)
    }

    /// 改选采集设备：保存、通知采集线程和所有听众。
    pub fn set_choice(&self, choice: Option<Choice>) {
        if let Ok(mut c) = self.choice.lock() {
            if *c == choice {
                return;
            }
            *c = choice.clone();
        }
        save_choice(choice.as_ref());
        self.choice_gen.fetch_add(1, Ordering::SeqCst);
        log!(
            "[capture] 选择设备: {}",
            choice.as_ref().map_or("跟随默认输出", |c| c.name.as_str())
        );
        let _ = self
            .events
            .send(Event::Choice(choice.map(|c| c.id).unwrap_or_default()));
        self.send_capture(CaptureCmd::Wake);
    }

    /// Захват поднят (или переключился на другое устройство) — всем live + формат.
    pub fn go_live(&self, src: Source) {
        if let Ok(mut s) = self.source.lock() {
            *s = Some(src.clone());
        }
        let _ = self.events.send(Event::Live(src));
    }

    /// Захват остановлен. `notify` — сообщить слушателям (если они ещё есть).
    pub fn go_offline(&self, notify: bool) {
        if let Ok(mut s) = self.source.lock() {
            *s = None;
        }
        if notify {
            let _ = self.events.send(Event::Offline);
        }
    }

    pub fn publish(&self, frame: Bytes) {
        let _ = self.audio.send(frame);
    }
}

fn choice_path() -> std::path::PathBuf {
    crate::log::dir().join("device.txt")
}

/// 文件格式：第一行设备 ID，第二行名称。
fn load_choice() -> Option<Choice> {
    let s = std::fs::read_to_string(choice_path()).ok()?;
    let mut lines = s.lines();
    let id = lines.next()?.trim().to_string();
    let name = lines.next().unwrap_or("").trim().to_string();
    (!id.is_empty()).then_some(Choice { id, name })
}

fn save_choice(c: Option<&Choice>) {
    let path = choice_path();
    let _ = std::fs::create_dir_all(crate::log::dir());
    match c {
        Some(c) => {
            let _ = std::fs::write(path, format!("{}\n{}\n", c.id, c.name));
        }
        None => {
            let _ = std::fs::remove_file(path);
        }
    }
}

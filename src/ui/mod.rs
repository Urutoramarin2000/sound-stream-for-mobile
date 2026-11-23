//! Окно приложения и панель у значка в трее — в стиле Windows 11: Mica и
//! Acrylic, скруглённые углы, тёмная и светлая тема. Рисуется через Direct2D,
//! без библиотек интерфейса и рантаймов: exe остаётся одним файлом.
//!
//! Всё, что здесь, живёт в главном потоке — том же, где цикл сообщений трея.

pub mod flyout;
mod gfx;
mod kit;
pub mod window;

use std::cell::{Cell, RefCell};
use std::net::Ipv4Addr;
use std::sync::atomic::Ordering;
use std::time::{Duration, Instant};

use windows_sys::Win32::Foundation::{GlobalFree, HWND};
use windows_sys::Win32::Graphics::Dwm::{
    DwmExtendFrameIntoClientArea, DwmSetWindowAttribute, DWMWA_SYSTEMBACKDROP_TYPE,
    DWMWA_USE_IMMERSIVE_DARK_MODE, DWMWA_WINDOW_CORNER_PREFERENCE, DWMWCP_ROUND,
};
use windows_sys::Win32::System::DataExchange::{
    CloseClipboard, EmptyClipboard, OpenClipboard, SetClipboardData,
};
use windows_sys::Win32::System::Memory::{GlobalAlloc, GlobalLock, GlobalUnlock, GMEM_MOVEABLE};
use windows_sys::Win32::System::Ole::CF_UNICODETEXT;
use windows_sys::Win32::System::Registry::{RegGetValueW, HKEY_CURRENT_USER, RRF_RT_REG_DWORD};
use windows_sys::Win32::System::SystemInformation::{GetVersionExW, OSVERSIONINFOW};
use windows_sys::Win32::UI::Controls::MARGINS;
use windows_sys::Win32::UI::Shell::ShellExecuteW;
use windows_sys::Win32::UI::WindowsAndMessaging::SW_SHOWNORMAL;

use crate::state::{Choice, Source};
use crate::tray::{self, Exit, Kind};
use crate::wide::wide;
use crate::{autostart, capture, net};

use gfx::Color;
use kit::Theme;

/// Что можно сделать кликом в окне или в панели.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Act {
    /// Показать этот адрес в QR-коде (0 — постоянный, дальше — по IP).
    Select(u8),
    /// Скопировать адрес (нумерация как у `Select`).
    Copy(u8),
    /// Открыть плеер на этом ПК.
    Player,
    /// Открыть главное окно.
    Window,
    /// 弹出采集设备菜单。
    Device,
    Autostart,
    Log,
    Restart,
    Exit,
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Status {
    Starting,
    PortBusy,
    Ready,
    Live(usize),
}

impl Status {
    fn color(self, t: Theme) -> Color {
        match self {
            Status::Starting => t.warn(),
            Status::PortBusy => t.bad(),
            Status::Ready | Status::Live(_) => t.accent(),
        }
    }

    fn label(self) -> String {
        match self {
            Status::Starting => "启动中…".into(),
            Status::PortBusy => "端口被占用".into(),
            Status::Ready => "就绪 · 等待收听".into(),
            Status::Live(n) => format!("推流中 · {}", listeners(n)),
        }
    }
}

/// Снимок состояния для одного кадра.
pub struct Info {
    pub status: Status,
    pub port: u16,
    /// [0] — постоянный адрес `.local`, дальше — по IP.
    pub urls: Vec<String>,
    pub source: Option<Source>,
    /// 用户选的采集设备（None = 跟随默认输出）。
    pub choice: Option<Choice>,
    pub autostart: bool,
}

fn info() -> Info {
    let a = tray::app();
    let status = if a.port_busy.load(Ordering::SeqCst) {
        Status::PortBusy
    } else if !a.server_up.load(Ordering::SeqCst) {
        Status::Starting
    } else {
        match a.listeners() {
            0 => Status::Ready,
            n => Status::Live(n),
        }
    };
    let mut urls = vec![a.cfg.mdns_url()];
    urls.extend(lan_ips().into_iter().map(|ip| a.cfg.ip_url(ip)));
    Info {
        status,
        port: a.cfg.port,
        urls,
        source: a.source(),
        choice: a.choice(),
        autostart: autostart::is_enabled(),
    }
}

thread_local! {
    static IPS: RefCell<Option<(Instant, Vec<Ipv4Addr>)>> = const { RefCell::new(None) };
    static COPIED: Cell<Option<(Act, Instant)>> = const { Cell::new(None) };
}

/// Адреса по IP: опрос адаптеров не на каждый кадр, а раз в несколько секунд.
fn lan_ips() -> Vec<Ipv4Addr> {
    IPS.with(|c| {
        let mut c = c.borrow_mut();
        match c.as_ref() {
            Some((at, ips)) if at.elapsed() < Duration::from_secs(5) => ips.clone(),
            _ => {
                let ips = net::lan_ipv4();
                *c = Some((Instant::now(), ips.clone()));
                ips
            }
        }
    })
}

/// Окно открылось — адреса должны быть свежими, а не пятисекундной давности.
fn refresh_ips() {
    IPS.with(|c| *c.borrow_mut() = None);
}

/// Кнопка «скопировать» ещё полторы секунды показывает галочку.
fn copied(act: Act) -> bool {
    COPIED.with(|c| {
        c.get()
            .is_some_and(|(a, at)| a == act && at.elapsed() < Duration::from_millis(1500))
    })
}

fn perform(act: Act) {
    let a = tray::app();
    match act {
        Act::Select(_) => {}
        Act::Copy(i) => {
            if let Some(url) = info().urls.get(i as usize) {
                if copy_text(tray::hwnd(), url) {
                    COPIED.with(|c| c.set(Some((act, Instant::now()))));
                }
            }
        }
        Act::Player => open(&a.cfg.local_url()),
        Act::Window => window::show(),
        Act::Device => device_menu(),
        Act::Autostart => {
            if !autostart::set(!autostart::is_enabled()) {
                tray::notify("开机自启", "无法修改此设置。", Kind::Warning);
            }
        }
        Act::Log => open(&crate::log::path().to_string_lossy()),
        Act::Restart => tray::quit(Exit::Restart),
        Act::Exit => tray::quit(Exit::Quit),
    }
}

/// 「2 位听众」。
fn listeners(n: usize) -> String {
    format!("{n} 位听众")
}

fn khz(rate: u32) -> String {
    if rate % 1000 == 0 {
        (rate / 1000).to_string()
    } else {
        format!("{:.1}", rate as f64 / 1000.0)
    }
}

/// Тёмная тема Windows: `apps` — для окон приложений, иначе — для панели
/// задач и её всплывающих панелей (в Windows их настраивают отдельно).
fn dark_mode(apps: bool) -> bool {
    let key = wide(r"Software\Microsoft\Windows\CurrentVersion\Themes\Personalize");
    let name = wide(if apps {
        "AppsUseLightTheme"
    } else {
        "SystemUsesLightTheme"
    });
    let mut v: u32 = 1;
    let mut size = 4u32;
    // SAFETY: буфер — один DWORD, размер передан.
    let r = unsafe {
        RegGetValueW(
            HKEY_CURRENT_USER,
            key.as_ptr(),
            name.as_ptr(),
            RRF_RT_REG_DWORD,
            std::ptr::null_mut(),
            (&mut v as *mut u32).cast(),
            &mut size,
        )
    };
    r == 0 && v == 0
}

fn os_build() -> u32 {
    // SAFETY: структура с проставленным размером. Манифест объявляет поддержку
    // Windows 10, поэтому GetVersionExW возвращает настоящий номер сборки.
    unsafe {
        let mut v: OSVERSIONINFOW = std::mem::zeroed();
        v.dwOSVersionInfoSize = std::mem::size_of::<OSVERSIONINFOW>() as u32;
        if GetVersionExW(&mut v) == 0 {
            return 0;
        }
        v.dwBuildNumber
    }
}

/// Оформление окна средствами DWM: тёмный заголовок, скругление (Windows 11)
/// и системный фон `backdrop` (Mica = 2, Acrylic = 3; Windows 11 22H2+).
/// Возвращает true, если фон рисует система и наш должен быть прозрачным.
unsafe fn decorate(hwnd: HWND, dark: bool, backdrop: i32, round: bool) -> bool {
    let set = |attr: i32, v: i32| {
        DwmSetWindowAttribute(hwnd, attr as u32, (&v as *const i32).cast(), 4) >= 0
    };
    set(DWMWA_USE_IMMERSIVE_DARK_MODE, dark as i32);
    let build = os_build();
    if round && build >= 22000 {
        set(DWMWA_WINDOW_CORNER_PREFERENCE, DWMWCP_ROUND);
    }
    if build < 22621 {
        return false;
    }
    let all = MARGINS {
        cxLeftWidth: -1,
        cxRightWidth: -1,
        cyTopHeight: -1,
        cyBottomHeight: -1,
    };
    DwmExtendFrameIntoClientArea(hwnd, &all) >= 0 && set(DWMWA_SYSTEMBACKDROP_TYPE, backdrop)
}

/// 采集来源的一行说明：「48 kHz · 立体声 · 输出回环」。
fn source_detail(s: &Source) -> String {
    if s.fallback {
        return format!("{} kHz · 所选设备不可用，暂用默认输出", khz(s.sample_rate));
    }
    let ch = if s.channels == 1 { "单声道→立体声" } else { "立体声" };
    let kind = if s.input { "输入设备" } else { "输出回环" };
    format!("{} kHz · {ch} · {kind}", khz(s.sample_rate))
}

/// 未在采集时显示的目标设备名。
fn choice_label(c: &Option<Choice>) -> String {
    match c {
        Some(c) if !c.name.is_empty() => c.name.clone(),
        Some(_) => "已选设备".into(),
        None => "Windows 默认输出".into(),
    }
}

const MENU_DEFAULT: usize = 1;
const MENU_KEEP: usize = 2;
const MENU_SETTINGS: usize = 3;
const MENU_DEVICE0: usize = 100;

/// 弹出菜单：跟随默认输出 / 各输出设备（回环）/ 各输入设备 / 打开声音设置。
fn device_menu() {
    use windows_sys::Win32::Foundation::POINT;
    use windows_sys::Win32::UI::Input::KeyboardAndMouse::GetActiveWindow;
    use windows_sys::Win32::UI::WindowsAndMessaging::{
        AppendMenuW, CreatePopupMenu, DestroyMenu, GetCursorPos, PostMessageW,
        SetForegroundWindow, TrackPopupMenu, MF_CHECKED, MF_GRAYED, MF_SEPARATOR, MF_STRING,
        TPM_NONOTIFY, TPM_RETURNCMD, TPM_RIGHTBUTTON, WM_NULL,
    };

    let app = tray::app();
    let list = capture::list_devices();
    let choice = app.choice();
    let chosen = |id: &str| choice.as_ref().is_some_and(|c| c.id == id);
    let default_out = list
        .iter()
        .find(|d| !d.input && d.default)
        .map(|d| d.name.clone())
        .unwrap_or_else(|| "无".into());

    // SAFETY: 菜单在本函数内创建并销毁；字符串在调用期间有效。
    unsafe {
        let m = CreatePopupMenu();
        if m.is_null() {
            return;
        }
        let add = |flags: u32, id: usize, label: &str| {
            let w = wide(label);
            AppendMenuW(m, flags, id, w.as_ptr());
        };
        let check = |on: bool| if on { MF_CHECKED } else { 0 };
        add(
            MF_STRING | check(choice.is_none()),
            MENU_DEFAULT,
            &format!("跟随 Windows 默认输出（当前：{default_out}）"),
        );
        add(MF_SEPARATOR, 0, "");
        add(MF_STRING | MF_GRAYED, 0, "输出设备 — 采集它正在播放的声音");
        for (i, d) in list.iter().enumerate().filter(|(_, d)| !d.input) {
            let label = if d.default { format!("    {}（默认）", d.name) } else { format!("    {}", d.name) };
            add(MF_STRING | check(chosen(&d.id)), MENU_DEVICE0 + i, &label);
        }
        add(MF_SEPARATOR, 0, "");
        add(MF_STRING | MF_GRAYED, 0, "输入设备 — 麦克风 / 线路输入 / 虚拟声卡");
        for (i, d) in list.iter().enumerate().filter(|(_, d)| d.input) {
            let label = if d.default { format!("    {}（默认）", d.name) } else { format!("    {}", d.name) };
            add(MF_STRING | check(chosen(&d.id)), MENU_DEVICE0 + i, &label);
        }
        if let Some(c) = choice.as_ref().filter(|c| !list.iter().any(|d| d.id == c.id)) {
            add(MF_SEPARATOR, 0, "");
            add(
                MF_STRING | MF_CHECKED,
                MENU_KEEP,
                &format!("{}（未连接，暂用默认输出）", choice_label(&Some(c.clone()))),
            );
        }
        add(MF_SEPARATOR, 0, "");
        add(MF_STRING, MENU_SETTINGS, "打开 Windows 声音设置…");

        let mut owner = GetActiveWindow();
        if owner.is_null() {
            owner = tray::hwnd();
        }
        let mut pt = POINT { x: 0, y: 0 };
        GetCursorPos(&mut pt);
        SetForegroundWindow(owner);
        let cmd = TrackPopupMenu(
            m,
            TPM_RETURNCMD | TPM_RIGHTBUTTON | TPM_NONOTIFY,
            pt.x,
            pt.y,
            0,
            owner,
            std::ptr::null(),
        ) as usize;
        PostMessageW(owner, WM_NULL, 0, 0);
        DestroyMenu(m);

        match cmd {
            MENU_DEFAULT => app.set_choice(None),
            MENU_SETTINGS => sound_settings(),
            n if n >= MENU_DEVICE0 => {
                if let Some(d) = list.get(n - MENU_DEVICE0) {
                    app.set_choice(Some(Choice {
                        id: d.id.clone(),
                        name: d.name.clone(),
                    }));
                }
            }
            _ => {}
        }
    }
}

/// 经典「声音」控制面板（可以设默认设备、采样率）。
fn sound_settings() {
    let verb = wide("open");
    let exe = wide("control.exe");
    let args = wide("mmsys.cpl");
    // SAFETY: 字符串以 0 结尾。
    unsafe {
        ShellExecuteW(
            std::ptr::null_mut(),
            verb.as_ptr(),
            exe.as_ptr(),
            args.as_ptr(),
            std::ptr::null(),
            SW_SHOWNORMAL,
        );
    }
}

fn copy_text(hwnd: HWND, s: &str) -> bool {
    let w = wide(s);
    // SAFETY: обычная работа с буфером обмена; при успехе памятью владеет он.
    unsafe {
        if OpenClipboard(hwnd) == 0 {
            return false;
        }
        EmptyClipboard();
        let mem = GlobalAlloc(GMEM_MOVEABLE, w.len() * 2);
        let mut ok = false;
        if !mem.is_null() {
            let p = GlobalLock(mem) as *mut u16;
            if !p.is_null() {
                std::ptr::copy_nonoverlapping(w.as_ptr(), p, w.len());
                GlobalUnlock(mem);
                ok = !SetClipboardData(CF_UNICODETEXT as u32, mem).is_null();
            }
            if !ok {
                GlobalFree(mem);
            }
        }
        CloseClipboard();
        ok
    }
}

/// Открывает адрес в браузере или файл в связанной программе.
fn open(target: &str) {
    let verb = wide("open");
    let t = wide(target);
    // SAFETY: строки нуль-терминированы.
    unsafe {
        ShellExecuteW(
            std::ptr::null_mut(),
            verb.as_ptr(),
            t.as_ptr(),
            std::ptr::null(),
            std::ptr::null(),
            SW_SHOWNORMAL,
        );
    }
}

/// Размер в физических пикселях для `dip` при данном DPI.
fn px(dip: f32, dpi: u32) -> i32 {
    (dip * dpi as f32 / 96.0).ceil() as i32
}

//! 音频采集（WASAPI）。
//!
//! - 默认：对 Windows 默认输出设备做 loopback（电脑正在播放的声音）。
//! - 可以在窗口/托盘/网页里改选：任意输出设备（回环）或输入设备（麦克风、线路输入、
//!   虚拟声卡输出端等）。选择持久保存；设备拔掉时暂时退回默认输出，插回后自动切回。
//! - 发送立体声交错 Int16。每次 WASAPI 唤醒（通常 10 ms 一个引擎周期）立刻打包发出，
//!   不再攒整 10 ms 帧；每包带 12 字节包头（序号 + 包内首帧在 PC 上已等待的微秒数），
//!   网页据此估算端到端延迟。
//!
//! Захват «по требованию»: поднимается при первом слушателе и гаснет, когда
//! слушателей не осталось.

use std::sync::mpsc::{self, RecvTimeoutError, TryRecvError};
use std::sync::Arc;
use std::time::{Duration, Instant};

use axum::body::Bytes;
use windows::core::{Interface, PCWSTR, PWSTR};
use windows::Win32::Devices::FunctionDiscovery::PKEY_Device_FriendlyName;
use windows::Win32::Foundation::{CloseHandle, HANDLE};
use windows::Win32::Media::Audio::{
    eCapture, eConsole, eRender, EDataFlow, IAudioCaptureClient, IAudioClient, IMMDevice,
    IMMDeviceEnumerator, IMMEndpoint, MMDeviceEnumerator, AUDCLNT_BUFFERFLAGS_SILENT,
    AUDCLNT_SHAREMODE_SHARED, AUDCLNT_STREAMFLAGS_EVENTCALLBACK, AUDCLNT_STREAMFLAGS_LOOPBACK,
    DEVICE_STATE_ACTIVE, WAVEFORMATEX, WAVEFORMATEXTENSIBLE,
};
use windows::Win32::System::Com::{
    CoCreateInstance, CoInitializeEx, CoTaskMemFree, CoUninitialize, CLSCTX_ALL,
    COINIT_MULTITHREADED, STGM_READ,
};
use windows::Win32::System::Threading::{CreateEventW, WaitForSingleObject};

use crate::state::{App, CaptureCmd, Choice, Source};
use crate::wide::wide;

/// Как часто проверять, не сменилось ли устройство (默认输出变化 / 所选设备插拔)。
const DEVICE_CHECK: Duration = Duration::from_secs(1);
/// Пауза перед новой попыткой, если открыть захват не удалось.
const RETRY: Duration = Duration::from_secs(2);
/// 包头长度：[版本=1][声道=2][保留 u16][序号 u32][PC 端等待 µs u32]，小端。
pub const HEADER: usize = 12;

/// 设备列表里的一项（给窗口菜单和网页用）。
#[derive(Clone, Debug)]
pub struct DeviceInfo {
    pub id: String,
    pub name: String,
    /// true = 输入设备；false = 输出设备（采集其回环）。
    pub input: bool,
    /// 是该方向的 Windows 默认设备。
    pub default: bool,
}

/// 枚举所有启用中的输出和输入设备。在独立线程里做 COM 初始化，
/// 可以从任何线程（界面线程、tokio 阻塞池）调用。
pub fn list_devices() -> Vec<DeviceInfo> {
    std::thread::spawn(|| {
        // SAFETY: 本线程自己的 COM 初始化；所有 COM 对象在 CoUninitialize 前释放。
        unsafe {
            let _ = CoInitializeEx(None, COINIT_MULTITHREADED);
            let list = list_inner();
            CoUninitialize();
            list
        }
    })
    .join()
    .unwrap_or_default()
}

unsafe fn list_inner() -> Vec<DeviceInfo> {
    let en: IMMDeviceEnumerator = match CoCreateInstance(&MMDeviceEnumerator, None, CLSCTX_ALL) {
        Ok(e) => e,
        Err(_) => return Vec::new(),
    };
    let mut out = Vec::new();
    for (flow, input) in [(eRender, false), (eCapture, true)] {
        let def = en
            .GetDefaultAudioEndpoint(flow, eConsole)
            .ok()
            .and_then(|d| d.GetId().ok())
            .map(|p| take_pwstr(p));
        let Ok(col) = en.EnumAudioEndpoints(flow, DEVICE_STATE_ACTIVE) else {
            continue;
        };
        let n = col.GetCount().unwrap_or(0);
        for i in 0..n {
            let Ok(dev) = col.Item(i) else { continue };
            let Ok(id) = dev.GetId() else { continue };
            let id = take_pwstr(id);
            let name = friendly_name(&dev).unwrap_or_else(|| id.clone());
            out.push(DeviceInfo {
                default: def.as_deref() == Some(id.as_str()),
                id,
                name,
                input,
            });
        }
    }
    out
}

pub fn spawn(app: Arc<App>, rx: mpsc::Receiver<CaptureCmd>) -> std::thread::JoinHandle<()> {
    std::thread::Builder::new()
        .name("capture".into())
        .spawn(move || run(&app, &rx))
        .expect("capture thread")
}

/// 让 Windows 以“专业音频”优先级调度采集线程（MMCSS），减小调度抖动。
fn boost_thread() {
    use windows_sys::Win32::System::Threading::AvSetMmThreadCharacteristicsW;
    let task = wide("Pro Audio");
    let mut index = 0u32;
    // SAFETY: 字符串以 0 结尾；句柄随线程结束自动失效，不必手动释放。
    let h = unsafe { AvSetMmThreadCharacteristicsW(task.as_ptr(), &mut index) };
    if h.is_null() {
        log!("[capture] MMCSS 不可用，采集线程按普通优先级运行");
    }
}

fn run(app: &App, rx: &mpsc::Receiver<CaptureCmd>) {
    boost_thread();
    // SAFETY: COM-вызовы ниже идут только из этого потока.
    let enumerator: Option<IMMDeviceEnumerator> = unsafe {
        let _ = CoInitializeEx(None, COINIT_MULTITHREADED);
        match CoCreateInstance(&MMDeviceEnumerator, None, CLSCTX_ALL) {
            Ok(e) => Some(e),
            Err(e) => {
                log!("[capture] WASAPI недоступен — звука не будет: {e}");
                None
            }
        }
    };

    let mut cap: Option<Capture> = None;
    let mut retry_at = Instant::now();
    let mut next_device_check = Instant::now();
    let mut failing = false;

    loop {
        // Пока захват идёт, «спим» на событии WASAPI (в pump), а команды
        // только подбираем. Без захвата — спим на канале до пробуждения.
        let cmd = if cap.is_some() {
            match rx.try_recv() {
                Ok(c) => Some(c),
                Err(TryRecvError::Empty) => None,
                Err(TryRecvError::Disconnected) => Some(CaptureCmd::Shutdown),
            }
        } else {
            let timeout = if app.listeners() > 0 {
                retry_at
                    .saturating_duration_since(Instant::now())
                    .max(Duration::from_millis(20))
            } else {
                Duration::from_secs(3600)
            };
            match rx.recv_timeout(timeout) {
                Ok(c) => Some(c),
                Err(RecvTimeoutError::Timeout) => None,
                Err(RecvTimeoutError::Disconnected) => Some(CaptureCmd::Shutdown),
            }
        };
        if matches!(cmd, Some(CaptureCmd::Shutdown)) {
            break;
        }
        let Some(enumerator) = enumerator.as_ref() else {
            continue;
        };

        let want = app.listeners() > 0;

        if !want {
            if cap.take().is_some() {
                app.go_offline(false);
                log!("[capture] 采集停止（没有听众）");
            }
            continue;
        }

        if cap.is_none() {
            if Instant::now() < retry_at {
                continue;
            }
            let gen = app.choice_gen();
            match Capture::open(enumerator, app.choice().as_ref(), gen) {
                Ok(c) => {
                    log!(
                        "[capture] 开始采集: {} ({}) @ {} Hz, {} 声道{}",
                        c.name,
                        if c.input { "输入设备" } else { "输出回环" },
                        c.rate,
                        c.channels,
                        if c.fallback {
                            "（所选设备不可用，暂用默认输出）"
                        } else {
                            ""
                        }
                    );
                    app.go_live(Source {
                        device: c.name.clone(),
                        sample_rate: c.rate,
                        channels: c.channels,
                        input: c.input,
                        fallback: c.fallback,
                    });
                    failing = false;
                    next_device_check = Instant::now() + DEVICE_CHECK;
                    cap = Some(c);
                }
                Err(e) => {
                    if !failing {
                        log!("[capture] 无法打开采集: {e} — 稍后重试");
                        failing = true;
                    }
                    if app.source().is_some() {
                        app.go_offline(true);
                    }
                    retry_at = Instant::now() + RETRY;
                }
            }
            continue;
        }

        let c = cap.as_mut().expect("checked above");
        if let Err(e) = c.pump(app) {
            log!("[capture] захват прерван: {e}");
            cap = None;
            app.go_offline(true);
            retry_at = Instant::now() + Duration::from_millis(500);
            continue;
        }
        if c.gen != app.choice_gen() {
            log!("[capture] 设备选择已改变 — 切换");
            cap = None;
            retry_at = Instant::now();
            continue;
        }
        if Instant::now() >= next_device_check {
            next_device_check = Instant::now() + DEVICE_CHECK;
            // 默认输出变了、所选设备拔掉或插回 —— 期望的设备和当前不同就重开。
            // SAFETY: 本线程的 COM 调用。
            let want_id = unsafe { pick(enumerator, app.choice().as_ref()) }.map(|p| p.id);
            if want_id.as_deref().ok() != Some(c.id.as_str()) {
                log!("[capture] 目标设备已变化 — 切换");
                cap = None;
                retry_at = Instant::now();
            }
        }
    }
    drop(cap);
}

#[derive(Clone, Copy, Debug)]
enum SampleFmt {
    F32,
    I16,
    I24,
    I32,
}

/// 选出来要打开的设备。
struct Picked {
    dev: IMMDevice,
    id: String,
    input: bool,
    fallback: bool,
}

/// 用户选的设备在且启用 → 用它；否则用默认输出（回环）。
///
/// SAFETY: 调用线程已初始化 COM。
unsafe fn pick(en: &IMMDeviceEnumerator, choice: Option<&Choice>) -> Result<Picked, String> {
    if let Some(c) = choice {
        if let Some((dev, input)) = active_device(en, &c.id) {
            return Ok(Picked {
                dev,
                id: c.id.clone(),
                input,
                fallback: false,
            });
        }
    }
    let dev = en
        .GetDefaultAudioEndpoint(eRender, eConsole)
        .map_err(|e| format!("没有默认输出设备 ({e})"))?;
    let id = take_pwstr(dev.GetId().map_err(|e| e.to_string())?);
    Ok(Picked {
        dev,
        id,
        input: false,
        fallback: choice.is_some(),
    })
}

unsafe fn active_device(en: &IMMDeviceEnumerator, id: &str) -> Option<(IMMDevice, bool)> {
    let w = wide(id);
    let dev = en.GetDevice(PCWSTR(w.as_ptr())).ok()?;
    if dev.GetState().ok()? != DEVICE_STATE_ACTIVE {
        return None;
    }
    let ep: IMMEndpoint = dev.cast().ok()?;
    let flow: EDataFlow = ep.GetDataFlow().ok()?;
    Some((dev, flow == eCapture))
}

struct Capture {
    id: String,
    name: String,
    input: bool,
    fallback: bool,
    /// 打开时的 choice_gen。
    gen: u64,
    rate: u32,
    channels: usize,
    center: Option<usize>,
    block_align: usize,
    fmt: SampleFmt,
    client: IAudioClient,
    capture: IAudioCaptureClient,
    event: HANDLE,
    /// 正在组装的包（含包头）。
    out: Vec<u8>,
    seq: u32,
    qpc_freq: i64,
}

impl Capture {
    fn open(en: &IMMDeviceEnumerator, choice: Option<&Choice>, gen: u64) -> Result<Self, String> {
        // SAFETY: обычная последовательность WASAPI; указатель на формат
        // освобождается CoTaskMemFree сразу после Initialize.
        unsafe {
            let p = pick(en, choice)?;
            let name = friendly_name(&p.dev).unwrap_or_else(|| {
                if p.input {
                    "输入设备".into()
                } else {
                    "输出设备".into()
                }
            });
            let client: IAudioClient = p.dev.Activate(CLSCTX_ALL, None).map_err(|e| e.to_string())?;

            let wf = client.GetMixFormat().map_err(|e| e.to_string())?;
            let parsed = parse_format(wf);
            let flags = if p.input {
                AUDCLNT_STREAMFLAGS_EVENTCALLBACK
            } else {
                AUDCLNT_STREAMFLAGS_LOOPBACK | AUDCLNT_STREAMFLAGS_EVENTCALLBACK
            };
            let init = parsed
                .ok_or_else(|| "不支持的设备格式".to_string())
                .and_then(|_| {
                    client
                        .Initialize(
                            AUDCLNT_SHAREMODE_SHARED,
                            flags,
                            200_000, // буфер 20 мс (в единицах по 100 нс)
                            0,
                            wf,
                            None,
                        )
                        .map_err(|e| e.to_string())
                });
            CoTaskMemFree(Some(wf as *const _));
            init?;
            let (fmt, rate, channels, block_align, center) = parsed.expect("checked above");

            let event = CreateEventW(None, false, false, None).map_err(|e| e.to_string())?;
            let started = (|| {
                client.SetEventHandle(event)?;
                let capture: IAudioCaptureClient = client.GetService()?;
                client.Start()?;
                Ok::<_, windows::core::Error>(capture)
            })();
            let capture = match started {
                Ok(c) => c,
                Err(e) => {
                    let _ = CloseHandle(event);
                    return Err(e.to_string());
                }
            };

            Ok(Capture {
                id: p.id,
                name,
                input: p.input,
                fallback: p.fallback,
                gen,
                rate,
                channels,
                center,
                block_align,
                fmt,
                client,
                capture,
                event,
                out: Vec::with_capacity(HEADER + rate as usize / 10 * 4),
                seq: 0,
                qpc_freq: qpc_frequency(),
            })
        }
    }

    /// 等数据（最多 100 ms），把这次能取到的全部打成一个包立刻发出。
    fn pump(&mut self, app: &App) -> windows::core::Result<()> {
        // SAFETY: буфер от GetBuffer читается только до ReleaseBuffer, его
        // размер — frames * block_align байт.
        unsafe {
            WaitForSingleObject(self.event, 100);
            let mut first_qpc: Option<u64> = None;
            self.out.clear();
            self.out.resize(HEADER, 0);
            loop {
                if self.capture.GetNextPacketSize()? == 0 {
                    break;
                }
                let mut data = std::ptr::null_mut();
                let mut frames = 0u32;
                let mut flags = 0u32;
                let mut qpc = 0u64;
                self.capture.GetBuffer(
                    &mut data,
                    &mut frames,
                    &mut flags,
                    None,
                    Some(&mut qpc),
                )?;
                first_qpc.get_or_insert(qpc);
                let n = frames as usize;
                if flags & AUDCLNT_BUFFERFLAGS_SILENT.0 as u32 != 0 || data.is_null() {
                    // «Тишина»: содержимое буфера надо игнорировать.
                    self.out.resize(self.out.len() + n * 4, 0);
                } else {
                    let bytes = std::slice::from_raw_parts(data, n * self.block_align);
                    self.convert(bytes);
                }
                self.capture.ReleaseBuffer(frames)?;
                // 积压过多（比如线程被卡住）时分包发，避免单包过大。
                if self.out.len() >= HEADER + self.rate as usize / 20 * 4 {
                    self.send(app, first_qpc.take());
                }
            }
            if self.out.len() > HEADER {
                self.send(app, first_qpc);
            }
        }
        Ok(())
    }

    /// 转成立体声 Int16：单声道复制到左右；5.1/7.1 的中置（对白）按 -3 dB 混入左右。
    fn convert(&mut self, bytes: &[u8]) {
        let fmt = self.fmt;
        let width = self.block_align / self.channels;
        let stereo = self.channels >= 2;
        let center = self.center;
        let to_i16 = |v: f32| ((v * 32767.0).round().clamp(-32768.0, 32767.0) as i16).to_le_bytes();
        for frame in bytes.chunks_exact(self.block_align) {
            let s = |i: usize| sample(fmt, &frame[i * width..]);
            let (mut l, mut r) = if stereo {
                (s(0), s(1))
            } else {
                let v = s(0);
                (v, v)
            };
            if let Some(c) = center {
                let v = s(c) * 0.707;
                l += v;
                r += v;
            }
            self.out.extend_from_slice(&to_i16(l));
            self.out.extend_from_slice(&to_i16(r));
        }
    }

    /// 填包头并广播。`qpc` = 包内首帧被 Windows 混音器产生的时刻（100 ns 单位）。
    fn send(&mut self, app: &App, qpc: Option<u64>) {
        let age_us = match qpc {
            Some(q) if q > 0 => {
                let now = qpc_now_100ns(self.qpc_freq);
                (now.saturating_sub(q) / 10).min(u32::MAX as u64) as u32
            }
            _ => 0,
        };
        self.seq = self.seq.wrapping_add(1);
        let h = &mut self.out[..HEADER];
        h[0] = 1;
        h[1] = 2;
        h[2] = 0;
        h[3] = 0;
        h[4..8].copy_from_slice(&self.seq.to_le_bytes());
        h[8..12].copy_from_slice(&age_us.to_le_bytes());
        let cap = self.out.capacity();
        let pkt = std::mem::replace(&mut self.out, Vec::with_capacity(cap));
        app.publish(Bytes::from(pkt));
        self.out.resize(HEADER, 0);
    }
}

impl Drop for Capture {
    fn drop(&mut self) {
        // SAFETY: client и event принадлежат только этому захвату.
        unsafe {
            let _ = self.client.Stop();
            let _ = CloseHandle(self.event);
        }
    }
}

fn qpc_frequency() -> i64 {
    let mut f = 0i64;
    // SAFETY: 输出参数是本地变量。
    unsafe { windows_sys::Win32::System::Performance::QueryPerformanceFrequency(&mut f) };
    f.max(1)
}

/// 当前 QPC 时间，换算成 100 ns 单位（与 GetBuffer 的 u64QPCPosition 同一时基）。
fn qpc_now_100ns(freq: i64) -> u64 {
    let mut c = 0i64;
    // SAFETY: 输出参数是本地变量。
    unsafe { windows_sys::Win32::System::Performance::QueryPerformanceCounter(&mut c) };
    (c as u128 * 10_000_000 / freq as u128) as u64
}

fn sample(fmt: SampleFmt, b: &[u8]) -> f32 {
    match fmt {
        SampleFmt::F32 => f32::from_le_bytes([b[0], b[1], b[2], b[3]]),
        SampleFmt::I16 => i16::from_le_bytes([b[0], b[1]]) as f32 / 32768.0,
        SampleFmt::I24 => (i32::from_le_bytes([0, b[0], b[1], b[2]]) >> 8) as f32 / 8_388_608.0,
        SampleFmt::I32 => i32::from_le_bytes([b[0], b[1], b[2], b[3]]) as f32 / 2_147_483_648.0,
    }
}

const WAVE_FORMAT_PCM: u16 = 1;
const WAVE_FORMAT_IEEE_FLOAT: u16 = 3;
const WAVE_FORMAT_EXTENSIBLE: u16 = 0xFFFE;
const SPEAKER_FRONT_LEFT_RIGHT: u32 = 0x3;
const SPEAKER_FRONT_CENTER: u32 = 0x4;

/// (формат сэмпла, частота, каналы, байт на кадр, индекс центрального канала)
type Parsed = (SampleFmt, u32, usize, usize, Option<usize>);

/// SAFETY: `wf` — указатель от GetMixFormat.
unsafe fn parse_format(wf: *const WAVEFORMATEX) -> Option<Parsed> {
    let f = std::ptr::read_unaligned(wf);
    let channels = f.nChannels as usize;
    let block_align = f.nBlockAlign as usize;
    if channels == 0 || block_align == 0 || block_align % channels != 0 {
        return None;
    }
    let (tag, center) = if f.wFormatTag == WAVE_FORMAT_EXTENSIBLE && f.cbSize >= 22 {
        let x = std::ptr::read_unaligned(wf as *const WAVEFORMATEXTENSIBLE);
        let mask = x.dwChannelMask;
        let center = (channels >= 3 && mask & SPEAKER_FRONT_CENTER != 0)
            .then(|| (mask & SPEAKER_FRONT_LEFT_RIGHT).count_ones() as usize);
        // Подтип: {00000001-...} — PCM, {00000003-...} — float.
        (x.SubFormat.data1 as u16, center)
    } else {
        (f.wFormatTag, None)
    };
    let fmt = match (tag, block_align / channels) {
        (WAVE_FORMAT_IEEE_FLOAT, 4) => SampleFmt::F32,
        (WAVE_FORMAT_PCM, 2) => SampleFmt::I16,
        (WAVE_FORMAT_PCM, 3) => SampleFmt::I24,
        (WAVE_FORMAT_PCM, 4) => SampleFmt::I32,
        _ => return None,
    };
    Some((fmt, f.nSamplesPerSec, channels, block_align, center))
}

/// SAFETY: `dev` — живой IMMDevice.
unsafe fn friendly_name(dev: &IMMDevice) -> Option<String> {
    let store = dev.OpenPropertyStore(STGM_READ).ok()?;
    let value = store.GetValue(&PKEY_Device_FriendlyName).ok()?;
    let name = value.to_string();
    (!name.is_empty()).then_some(name)
}

/// SAFETY: `p` выделен COM (CoTaskMemAlloc) и больше нигде не используется.
unsafe fn take_pwstr(p: PWSTR) -> String {
    let s = p.to_string().unwrap_or_default();
    CoTaskMemFree(Some(p.0 as *const _));
    s
}

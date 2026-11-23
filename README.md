# 🔊 Sound Stream

> **本分支（stereo-lowlat）改动**
>
> - **立体声**：发送交错立体声 Int16（原版是单声道），5.1/7.1 的中置声道按 -3 dB 混入左右。
> - **更低延迟**：每次 WASAPI 唤醒立刻发包（不再攒整 10 ms 帧）；采集线程用 MMCSS「Pro Audio」优先级；
>   播放页按样本级首尾相接排程，用 ≤0.6% 的平滑重采样追赶时钟漂移（原版是 ±2% 的变速，会听到音高跳变），
>   缓冲默认**自动**：让最近 8 秒最紧张的时刻也还剩约 3 ms，卡顿后自动加大、稳定后收回，积压超标直接裁掉；
>   默认直连音频输出，「锁屏/后台播放」改为可选开关（iPhone 锁屏才需要）。
> - **选择采集设备**：窗口「采集来源」卡片的「选择设备」按钮、托盘面板的来源行、网页下拉框都能选：
>   跟随 Windows 默认输出 / 任意输出设备（采集它播放的声音）/ 任意输入设备（麦克风、线路输入、虚拟声卡）。
>   选择保存在 `%LOCALAPPDATA%\sound-stream\device.txt`；设备拔掉时暂用默认输出，插回后自动切回。
> - **延迟估算**：网页显示 ≈总延迟 = 采集（PC 端实测，包头携带）+ 网络（ping 往返/2）+ 缓冲 + 浏览器输出延迟，另有抖动、卡顿次数。
> - **Mac / iPhone**：AudioContext 与电脑同采样率；iOS 17+ 设为 playback 会话，静音开关不再静掉声音；
>   频谱动画限 30 帧/秒且可关；Mac 上空格键播放/停止。界面改为中文。
>
> 用法：Windows 上双击 `sound-stream.exe` → Mac 浏览器打开窗口里显示的地址（`http://sound-stream.local:47823/` 或 IP）→ 点播放。
> 只想在 Mac 上听、电脑不出声：装一个虚拟声卡（如 VB-CABLE）设为 Windows 默认输出，然后在「选择设备」里选它（或保持「跟随默认输出」）。

Listen to your Windows PC's audio on your phone, right in the browser. No app to install.

A single `sound-stream.exe` captures everything your speakers play (WASAPI loopback) and
streams it over Wi-Fi to any iPhone or Android browser. It has a small window with a QR code
for your phone and lives in the system tray.

## Run

1. Download `sound-stream.exe` from the latest release (Releases).
2. Run it. A window opens with a QR code and the phone address.
3. On your phone (same Wi-Fi), scan the QR code or open **`http://sound-stream.local:47823/`**, then tap play.

It works on Windows 10/11 x64 with nothing to install.

- **SmartScreen** may warn about an unknown publisher. Click **More info → Run anyway**.
- **Windows Firewall** may ask on first launch. Allow it for private networks.

## Build from source

```bash
cargo build --release
```

This needs [Rust](https://rustup.rs) 1.85+ with the MSVC toolchain and gives you
`target\release\sound-stream.exe`.

To publish a release, push an annotated version tag (`git tag -a 2.1 && git push origin 2.1`).
GitHub Actions then builds the exe and attaches it to the release, using the tag message as the
release notes.

## Window and tray

The interface is in Russian and follows the Windows light or dark theme.

- **Window:** a QR code for the phone, the permanent address and fallback IP addresses (click
  one to show it in the QR code, or copy it), the captured device and listener count, start with
  Windows, the log, restart and exit. Closing the window keeps the server running in the tray.
- **Tray icon:** a click opens a compact panel with the status, addresses, the player on this
  PC, start with Windows, the log, restart and exit. A double-click opens the window.
- Started with Windows, it shows only the tray icon. Running the exe again opens the window.

## Notes

- Capture starts with the first listener, stops when everyone leaves, and follows changes of the default output device.
- `sound-stream.local` is announced via mDNS, so the bookmark survives DHCP IP changes. If it doesn't resolve, check that your router has client/AP isolation turned off, or use the IP address from the window.
- Optional environment variables: `PORT` (default `47823`) and `MDNS_NAME` (default `sound-stream`).
- Log: `%LOCALAPPDATA%\sound-stream\server.log`.

## License

MIT

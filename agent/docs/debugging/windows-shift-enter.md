# Windows 上 Shift+Enter 无法换行 — 排查指引

输入框里 `Shift+Enter` 不能插入换行、也没有任何报错（按键像"消失"了），而同样版本
在 Linux / macOS 上正常。本文说明 SHIFT 位可能在哪一层丢失、如何区分，以及 rpi
两侧的兜底各自覆盖什么。

适用于任何"修饰键 + Enter"在 Windows 上失效的反馈，不限于 `Shift+Enter`。

---

## 1. 先分清两条链路

Windows 上 `Shift+Enter` 失效有两个**互不相同**的原因，排查方向相反：

| 链路 | 终端发下来的东西 | rpi 侧落点 | 兜底是否覆盖 |
|---|---|---|---|
| A. 传统 VT 编码 | 裸 `\r`（`Enter`，无修饰键） | `terminal.rs` 的原生 SHIFT 采样（#108） | ✅ 覆盖 |
| B. 宿主终端改写键位 | `\n` → ConPTY 还原成 `Ctrl+Enter` | `editor.rs` 的 `CONTROL + Enter` 分支 | ✅ 覆盖（本次新增） |

两者的差别不在 crossterm，而在**终端有没有把按键原样送下去**。

---

## 2. 链路 A：终端不携带 SHIFT

传统 VT 编码里，`Enter` 与 `Shift+Enter` 都是 `\r`，crossterm 解码为
`Enter + KeyModifiers::NONE`，SHIFT 信息在编码层就不存在。

rpi 的兜底（#108）在 `crates/rpi-tui/src/terminal.rs:522-525`：

```rust
#[cfg(windows)]
if key.code == event::KeyCode::Enter && key.modifiers.is_empty() {
    return normalize_windows_key_event(key, shift_key_pressed(), ssh_session());
}
```

`shift_key_pressed()`（`terminal.rs:612`）用 `GetAsyncKeyState(VK_SHIFT)` 读物理键盘，
把 SHIFT 补回事件。约束是刻意的：

- 只在**本地会话**生效（`ssh_session()` 为假）——远程用户的按键不属于本机键盘；
- 只在 `modifiers.is_empty()` 时生效——事件自带修饰键时以事件为准；
- 采样发生在 `crossterm::event::read` 出口，之后不再重复采样，避免把"为下一个按键
  按下的 Shift"错记到更早的 Enter 上。

这条兜底**救不了链路 B**：改写后的记录带 `CONTROL`，`modifiers.is_empty()` 为假，
分支根本不进入。

---

## 3. 链路 B：宿主终端改写键位（常见，也最容易排查错方向）

Windows Terminal 允许把某个组合键绑成"发送一段文本"：

```json
"actions": [
  { "command": { "action": "sendInput", "input": "\n" }, "id": "User.sendInput.DFCDAF06" }
],
"keybindings": [
  { "id": "User.sendInput.DFCDAF06", "keys": "shift+enter" }
]
```

`sendInput` 的语义是**把这段文本塞进输入流**，不是"把这个按键送下去"。于是 SHIFT
标志在终端层就没了，rpi 收到的是一个裸 `LF`（`0x0a`）。

ConPTY 把这个 `0x0a` 还原成一条按键记录：`VK_CONTROL` 按下 + `VK_RETURN` 按下
（`char=0x0a`，`dwControlKeyState` 含 `LEFT_CTRL_PRESSED`）。crossterm 的 Windows
后端对 `VK_RETURN` 一律映射成 `KeyCode::Enter`（不做字符化），修饰键取自
`control_key_state`，因此最终事件是 **`Enter + CONTROL`** —— 和用户真的按
`Ctrl+Enter` 无法区分。

这个形态此前会一路落到 `crates/rpi-tui/src/editor.rs:1429` 的 `_ => return false`：
既不换行也不提交，表现为"按了没反应"。

### 为什么 `Char('\n')` 不是这条链路的形态

Windows 上 crossterm 从 `INPUT_RECORD` 解码，`VK_RETURN` 恒为 `KeyCode::Enter`，
不会产生 `Char('\n')` / `Char('\r')`。`Char('\n')` 只出现在 unix 的 VT 解析器里，
且仅在 raw mode **未**开启时（`crossterm-0.27.0/src/event/sys/unix/parse.rs:552`）；
rpi 常开 raw mode，所以那条路径在本进程内不成立。

结论：给 `#[cfg(windows)]` 守卫加 `Char('\n')` / `Char('\r')` 判断是死代码——平台与
事件形态对不上。要覆盖改写场景，改 `editor.rs` 的落点，而不是放宽守卫。

---

## 4. 排查步骤

### 4.1 看事件实际形态

```bash
RPI_DEBUG_KEYS=1 rpi
```

按键会打印成 `code=... mods=... kind=...`：

| 观察结果 | 判定 |
|---|---|
| `code=Enter mods=NONE`（按 Shift+Enter 时） | 链路 A，检查守卫是否被 SSH 门禁挡住 |
| `code=Enter mods=CONTROL` | 链路 B，查宿主终端的键位绑定 |
| 完全没有输出 | 按键被终端吞掉（未送到进程） |

### 4.2 查宿主终端配置

Windows Terminal 的 `settings.json`：

```powershell
$p = "$env:LOCALAPPDATA\Packages\Microsoft.WindowsTerminal_8wekyb3d8bbwe\LocalState\settings.json"
Select-String -Path $p -Pattern 'sendInput','shift\+enter' -Context 0,2
```

只要 `keybindings` 里存在 `keys: "shift+enter"` 指向某个 `sendInput` action，SHIFT
就到此为止。

### 4.3 排除"修复没进版本"

`#108` 自 `v0.9.5` 起包含在所有版本里。确认本机二进制确实带该逻辑：

```powershell
dumpbin /imports .\rpi.exe | Select-String USER32
```

`GetAsyncKeyState` 存在即为已包含。PE 构建时间应晚于该 PR 的合并时间。

---

## 5. 代码落点

| 位置 | 作用 | 覆盖 |
|---|---|---|
| `crates/rpi-tui/src/terminal.rs:522-525` | Windows 原生 SHIFT 采样守卫（#108） | 链路 A（裸 `\r`） |
| `crates/rpi-tui/src/terminal.rs:592` | `normalize_windows_key_event`，SSH 门禁 | 同上 |
| `crates/rpi-tui/src/terminal.rs:620` | `normalize_native_shift_enter`，补 SHIFT | 同上 |
| `crates/rpi-tui/src/editor.rs:1308` | `Enter` / `Char('\n')` / `Char('\r')` + `NONE` → 提交 | 远端客户端的 LF/CR 编码 |
| `crates/rpi-tui/src/editor.rs:1319` | `SHIFT + Enter` → 换行 | Linux kitty 协议、macOS 兜底 |
| `crates/rpi-tui/src/editor.rs:1330` | `CONTROL + Enter` → 换行 | 链路 B（终端改写） |
| `crates/rpi-tui/src/editor.rs:1396` | `Ctrl+J` → 换行 | 手动替代绑定 |

注意 `terminal.rs:620` 内部还有一份 `code == Enter && modifiers.is_empty()` 判断：
只放宽外层守卫而不动它，SHIFT 依然补不进去。

---

## 6. 为什么不启用 win32-input-mode

`win32-input-mode`（`CSI ? 9001 h`）能让终端把完整修饰键信息送下来，是理论上最干净
的解法。rpi 不启用它，原因是一旦进程异常退出没有复位，用户 shell 的输入会坏掉
（microsoft/terminal#19576、fish-shell#11877）。为一个键位绑定承担"shell 不可用"的
风险不划算，因此选择"事件自带就用自带的，不自带就采样物理键盘"的尽力而为策略。

代价是已知且被接受的：

- Shift 在事件被采样前已松开 → 按提交处理（与兜底存在之前一致）；
- 为下一个按键按下的 Shift 被误采到前一个 Enter → 插入换行；下一次 Enter 提交，
  草稿不丢。

---

## 7. 修复

### 7.1 用户侧（立即可用，0 行代码）

删掉 `settings.json` 里 `shift+enter` 的 `sendInput` 绑定（连同对应的 `actions`
条目）。删除后 `Shift+Enter` 以真实按键记录（`vk=13` + `SHIFT_PRESSED`）到达，
crossterm 直接给出 `Enter + SHIFT`，`editor.rs:1319` 插入换行——连 #108 的兜底都不
需要被触发。

临时替代：`Ctrl+J` 在 rpi 中已是换行。

### 7.2 源码侧（防御性）

`editor.rs:1330` 把 `CONTROL + Enter` 绑成换行，使被改写的输入不再静默失败。这条
改动只作用于"此前无分支可匹配、静默丢弃"的形态，不改变裸 `Enter` 提交与
`Shift+Enter` 换行的既有语义。

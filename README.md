# fcitx5-voxkit

在 fcitx5 中**按住按鍵說話、放開就輸入文字**的語音輸入功能。
全程在本機辨識，錄音不會上傳；可搭配任何 fcitx5 輸入法使用。

## 功能

- **按住說話**：按住**右 Alt** 說話，放開後輸出辨識結果
- **繁體中文**：自動轉換成台灣用語的繁體中文
- **速度**：5 秒的語音，放開後約 1 秒出字
- **本機辨識**：使用 Qwen3-ASR 語音模型，不需要網路，錄音不會離開電腦
- **與輸入法整合**：搭配 [新酷音 Plus（chewing-plus）](https://github.com/siygle/chewing-plus) 時，
  - 結果會插入游標位置，前後打好的字都會保留
  - 會參考你自己加的詞，提高專有名詞的辨識率
- **其他輸入法**：沒有特別整合的輸入法也能使用，在組字區為空時直接輸出
- **安全**：在密碼等敏感欄位中自動停用

## 使用方式

1. 啟動語音服務：

   ```bash
   systemctl --user enable --now voxkit.service
   ```

2. 在任何輸入框中，**按住右 Alt** 說話，放開後就會輸出文字。

| 狀態 | 說明 |
|---|---|
| 🎙 聆聽中 ●●●○○ | 正在錄音，圓點顯示音量 |
| ⋯ 辨識中 | 放開按鍵後，正在辨識 |
| （沒有聽到語音） | 沒有偵測到說話聲 |

這些狀態字出現在 fcitx5 的候選／提示浮動視窗裡（`inputPanel` 的 aux 區），由**經典使用者介面**繪製，Voxkit 沒有另外的字級設定。要放大請到 fcitx5 設定 → 附加元件 → 經典使用者介面 → 字型（預設 `Sans 10`），或在 `~/.config/fcitx5/conf/classicui.conf` 寫入 `Font="Sans 13"`，再執行 `fcitx5-remote -r`。若在 Wayland 調大後仍偏小，可在同一檔案把 `ForceWaylandDPI` 設成 `144` 或 `192`。

- 按住時間少於 0.25 秒會被忽略，不小心誤按不會觸發
- 按住時按下其他按鍵，會取消這次錄音

## 設定

開啟 `fcitx5-configtool` → **附加元件** → **Voxkit**：

| 選項 | 預設 | 說明 |
|---|---|---|
| 啟用語音輸入 | 開 | 整個功能的開關 |
| 按住說話的按鍵 | 右 Alt | 只能設定修飾鍵（Alt、Ctrl、Shift、Super） |
| 轉換為繁體中文 | 開 | 使用 OpenCC 轉成台灣用語 |
| 忽略短按（毫秒） | 250 | 短於這個時間的按壓不會觸發 |

## 語音模型

預設直接使用 [Hex](https://github.com/anomalyco/hex) 已下載的 Qwen3-ASR 模型（`~/.local/share/hex/models`），
只讀取、不修改，也不需要再下載一次。

沒有安裝 Hex 時，可以自行下載模型（約 850 MB）：

```bash
voxkit model install qwen3-asr-0.6b
```

## 授權

MIT。語音模型不包含在本專案中，各自適用原本的授權。

開發、架構與效能測試說明請見 [docs/DEVELOPMENT.md](docs/DEVELOPMENT.md)。

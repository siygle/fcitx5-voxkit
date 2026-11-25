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
| 啟用語音翻譯 | 關 | 按住翻譯鍵說中文，輸入譯文 |
| 按住翻譯的按鍵 | 右 Ctrl | 只能設定修飾鍵。右 Shift 留給新酷音 Plus 的英文模式 |
| 翻譯成 | `en` | `en`、`ja`、`ko`、`zh-Hant`、`zh-Hans`、`fr`、`de`、`es`… |

## 語音翻譯（實驗）

按住翻譯鍵說話，放開後先辨識，再由本機翻譯模型翻成指定語言輸入。細節見 [docs/TRANSLATION.md](docs/TRANSLATION.md)。

- 需要選用的翻譯外掛 `libvoxkit-mt.so`（套件 `fcitx5-voxkit-translate` 或 `chewing-plus-translate`）。
- 翻譯模型 HY-MT1.5-1.8B（約 1.1 GB）在第一次使用時於背景下載；輸入法會立刻輸入原文並顯示「翻譯模型下載中」。命令列 `voxkit translate` 同樣先印出原文並以狀態碼 3 結束，不會卡住等下載；加上 `--wait` 才會先下載再翻譯。也可以先手動下載：`voxkit model install hy-mt1.5-1.8b`。
- 翻譯失敗時一律輸入原文，並短暫提示原因。

## 語音模型

系統服務預設偏好 [Hex](https://github.com/anomalyco/hex) 的 Qwen3-ASR（`VOXKIT_MODEL=qwen3-asr-0.6b`，目錄 `~/.local/share/hex/models`，只讀取、不修改）。
若該模型檔不在倉庫裡，daemon 會改用已安裝的語音模型（通常是 `whisper-tiny`）並在日誌寫下這次後援。

沒有安裝 Hex 時，可以自行下載偏好的模型（約 850 MB），或先用較小的 `whisper-tiny`（`voxkit model install` 不指定 id 時的預設）：

```bash
voxkit model install qwen3-asr-0.6b
# 或
voxkit model install            # whisper-tiny + Silero VAD
```

## 授權

MIT。語音模型不包含在本專案中，各自適用原本的授權。

開發、架構與效能測試說明請見 [docs/DEVELOPMENT.md](docs/DEVELOPMENT.md)。

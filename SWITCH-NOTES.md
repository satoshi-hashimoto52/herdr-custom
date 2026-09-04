# Herdr fork への切り替えメモ（2026-09-04）

## 何が変わったか

Ghostty が起動する Herdr を、Homebrew 版から自前ビルド版へ切り替えました。

| | パス | 版 |
|---|---|---|
| 切り替え前 | `/opt/homebrew/bin/herdr` | 0.8.0 (Homebrew) |
| 切り替え後 | `~/.local/bin/herdr` | 0.8.2 + 今回の改修 |

変更したのは `~/.config/ghostty/config` の 14 行目 `initial-command` のみです。

- バックアップ: `~/.config/ghostty/config.bak-before-herdr-fork-20260904`
- fork ソース: このリポジトリ（ブランチ `herdr-sidebar-resources-agent-status`）
- Homebrew 版は削除していません。`brew` 管理下のまま残っています。

## 追加された機能

1. 左ペイン最下部に SSD 空き容量と Swap 使用量を固定表示（Agents 一覧のみスクロール）
2. Agents 一覧に状態表示 `idle / working / waiting / done / error`
   - 表示ラベルのみの変更で、状態機械・内部状態名・socket API は従来どおり
     （`idle` / `working` / `blocked` / `done` / `unknown`）
   - 検出上の `blocked` は「何を待っているか」に合わせて `waiting` と表示
   - 状態色はテーマ固有の色相を保ったまま、暗背景で読める彩度・明度へ補正
3. Agents 一覧のタブ名を暖色アクセント `#FFB375` で表示
   - 固定文字列ではなく `tab` トークン単位の指定なので、任意のタブ名に適用される
     （自動命名タブや今後追加するタブも対象）
   - 選択中・非選択・Navigate モードのいずれでも維持される
   - ワークスペース名・区切り `·`・エージェント名・状態表示は従来色のまま

```text
● ~ · my-project
  claude · working

────────────────
SSD   188 GB
SWAP  0.0 GB
```

## 起動確認

新しい Ghostty ウィンドウで以下を確認してください。

```bash
herdr --version                     # PATH 次第では Homebrew 版が出ます
~/.local/bin/herdr --version
```

左ペイン最下部に SSD / SWAP が出ていれば切り替え成功です。
タブ名が `#FFB375` のオレンジになっていれば、サイドバーの配色も新しいバイナリで動いています。
実機の答え合わせ:

```bash
df -h /System/Volumes/Data
diskutil info /System/Volumes/Data | grep -i 'free space'
sysctl vm.swapusage
```

## バイナリを入れ替えたあとの反映

Herdr はサーバー／クライアント構成で、**サイドバーを描画しているのはサーバープロセス**です。
Ghostty を再起動してもクライアントが作り直されるだけで、既存のサーバーにアタッチし直すため、
バイナリを差し替えただけでは表示が変わりません。サーバーを明示的に止めてください。

```bash
~/.local/bin/herdr server stop
# そのあと Ghostty を手動で起動する
```

注意点:

- `herdr` と素で打つと PATH 上の別のインストール（この環境では Homebrew 版 0.8.0）に
  解決されることがあります。**必ず更新した実体のパスを指定**してください。
  この環境での実体は `~/.local/bin/herdr`（Ghostty の `initial-command` も同じ絶対パス）です。
- 反映されているか疑わしいときは、稼働中サーバーが実行している実体を確認します。
  `rm` してから `cp` する安全手順の性質上、古いプロセスは削除済み inode を保持し続けます。

```bash
pgrep -fl 'herdr server'
lsof -p <server-pid> | grep -w txt   # 表示された inode が現在のファイルと一致するか
```

## ロールバック

問題があれば元に戻せます。

```bash
cp ~/.config/ghostty/config.bak-before-herdr-fork-20260904 ~/.config/ghostty/config
~/.local/bin/herdr server stop
# Ghostty を再起動
```

これだけで Homebrew 版 0.8.0 に戻ります。

## footer を消したいとき

`~/.config/herdr/config.toml` に追記（Herdr 本体の再起動が必要）:

```toml
[ui.sidebar.resources]
enabled = false
```

## 再ビルド手順

```bash
export PATH="$(brew --prefix zig@0.15)/bin:$HOME/.cargo/bin:$PATH"
cd path/to/herdr
cargo build --release --locked
# 実行中のバイナリを上書きするとコード署名が無効化され、以後 SIGKILL される。
# 必ず削除してから新しい inode でコピーすること。
rm -f ~/.local/bin/herdr
cp target/release/herdr ~/.local/bin/herdr
~/.local/bin/herdr --version   # 動作確認
```

必要な toolchain: Rust 1.96.1（`rustup` 導入済み）、Zig 0.15.2（`brew install zig@0.15` 済み）。

## 併用している Claude Code 側の配色

Herdr のペイン内で動かす Claude Code には、このフォークの左ペイン配色と合わせて
次のテーマ上書きを使っています。Herdr 本体の設定ではなく、Claude Code 側の設定です
（この環境では `~/.claude/themes/herdr-dark.json`、`settings.json` の `theme` は
`custom:herdr-dark`）。

| キー | 値 | 反映先 |
|---|---|---|
| `userMessageBackground` | `#1A4A58` | ユーザー入力の背景 |
| `userMessageBackgroundHover` | `#215C6D` | 同じくホバー時 |
| `claude` | `#50FA7B` | アシスタント発話先頭の `⏺` とスピナー |
| `briefLabelClaude` | `#7EDB9A` | brief 表示の `Claude` ラベル |

Ghostty は `background-opacity = 0.6` / `background-opacity-cells = true` で動かしているため、
セル背景は背後と合成されて表示されます。ターミナルのセルに個別の alpha は無いので、
透け具合の調整は RGB 値そのもので行っています。

## 既知の制約

- `done` は非フォーカス時のみ表示されます（Herdr 既存仕様。フォーカス中の pane は即「既読」になり `idle`）。
- `error` はエージェントのプロセスが処理中に終了した場合のみです。同 pane で新しいエージェントが起動すると解除されます。
- footer は展開時のデスクトップサイドバー専用。折りたたみ時・mobile レイアウト時は非表示になり、取得処理も止まります。
- Windows 非対応。Linux は実装済みですが未検証です。
- `brew upgrade herdr` では更新されません。上流の新版に追従するには rebase して再ビルドしてください。

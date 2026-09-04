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
2. Agents 一覧に状態表示 `idle / running / waiting / done / error`

```text
● ~ · my-project
  claude · running

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
実機の答え合わせ:

```bash
df -h /System/Volumes/Data
diskutil info /System/Volumes/Data | grep -i 'free space'
sysctl vm.swapusage
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

## 既知の制約

- `done` は非フォーカス時のみ表示されます（Herdr 既存仕様。フォーカス中の pane は即「既読」になり `idle`）。
- `error` はエージェントのプロセスが処理中に終了した場合のみです。同 pane で新しいエージェントが起動すると解除されます。
- footer は展開時のデスクトップサイドバー専用。折りたたみ時・mobile レイアウト時は非表示になり、取得処理も止まります。
- Windows 非対応。Linux は実装済みですが未検証です。
- `brew upgrade herdr` では更新されません。上流の新版に追従するには rebase して再ビルドしてください。

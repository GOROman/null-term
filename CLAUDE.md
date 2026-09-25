# pasotsu-term

パソコン通信用 2 画面シリアルターミナル (Rust / ratatui)。

- ビルド: `cargo build --release`
- 実機は `/dev/cu.usbserial-*` に aiwa PV-PF24MK2 (2400bps モデム) を接続し、秋月の PIC 簡易疑似電話交換機キットにつないでいる。DTE 速度は 2400 で応答 (9600 では応答しない)。
- シリアルの操作は `pasotsu-term ctl ...` で行う (README の「外部からの操作」参照)。TUI はターミナルでしか動かないので、Claude Code から使うときは `--headless` をバックグラウンドで起動するか、ユーザーが開いている TUI にそのまま ctl で接続する。
- ポートは排他的に開かれる。pasotsu-term が起動中は pyserial などで直接開かず ctl を使う。

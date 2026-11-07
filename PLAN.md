# DSH Web UI を Blitz でネイティブ再現する計画 (v2)

作成: 2026-11-07 / 方針改訂: **HTML+CSS は本物をそのまま使い、JS は Rust で同等実装**

## 0. ゴール（改訂）

| 層 | 方針 |
|---|---|
| CSS | DSH の実物（`index-*.css` 87KB + `vendor-*.css` 27KB）を**無改変で読む** |
| HTML/DOM | 本物と**同一の DOM ツリー・同一 class**（＝ CSS がそのまま当たる） |
| JS | React + cordis + 75 クライアントパッケージの挙動を **Rust で再実装** |
| バックエンド | 既存 dsh サーバをそのまま使用（`/api/remote.mux`） |

「全く同じ動作」の判定は **DOM ゴールデン一致 + 操作シナリオの一致**で行う（React 内部まで同一にはしない）。

## 1. 調査で確定した事実

### 1.1 フロントエンドの実体

```html
<body><div id="root"></div></body>
```

- **静的な HTML 資産は存在しない**。DOM はすべて JS（React）が生成する
- したがって「HTML 資産」の正本は次の 2 つ:
  1. 上流リポジトリの TSX（構造仕様。`packages/client/` に 75 パッケージ、MIT、取得可）
  2. 稼働中 GUI から採取した実 DOM（`document.querySelector('#root').outerHTML`）
- CSS 側は静的で完全に再利用可能
  - 61 `@media` / 23 `@font-face` / 22 animation / 22 transition / 35 カスタムプロパティ（`--dsh-*` 参照 100 箇所）
  - **CSS ネスト・`@layer`・`@container` は未使用** → Stylo に素直に載る
  - 未対応になりそうな機能（実測出現数）: `:has()` 18、`color-mix()` 10、`backdrop-filter` 7、`position:sticky` 4、`overflow:auto` 8、`mask-image` 11

### 1.2 移植対象の規模

| 項目 | 実測 |
|---|---|
| JS バンドル | `index` 970KB + `vendor` 741KB（minified 合計 1.7MB） |
| クライアントパッケージ | 75（`packages/client/`） |
| 転送 | WS 1 本 `/api/remote.mux`（JSON 多重化、`$events` ストリーム） |
| 認証 | トークン不要。browser-trust fence（`--trusted-host`） |
| Blitz | 0.3.0-beta.4 / JS 実行なし / `<textarea>` あり / `overflow:auto` 未対応 |
| IME | Issue #272（日中韓 commit）は 2025-11-12 closed。実機確認は要 |

**結論**: 1.7MB の JS 全移植は非現実的。**MVP に必要な十数パッケージに絞り、DOM 一致で品質を担保する。**

## 2. アーキテクチャ

```
            CSS 資産（無改変）       DOM ゴールデン（採取/TSX）
                    │                        │
                    ▼                        ▼
┌──────────────────────────────────────────────────────┐
│ dsh-blitz (Rust)                                     │
│  runtime/   signals + keyed diff + 合成イベント        │ ← React 相当
│  cordis/    サービスコンテナ + イベントバス + 注入      │ ← cordis 相当
│  transport/ /api/remote.mux の open/item/cancel/end   │
│  packages/  ui-layout, ui-sidebar, ui-conversation …   │ ← 各パッケージを 1:1 移植
│  dom/       blitz-dom へ出力（class 名は本物と同一）    │
└──────────────────────────────────────────────────────┘
                    │
                    ▼
        blitz-shell + blitz-renderer-vello
```

**CSS 適合レイヤ**（本物の CSS は書き換えない。差分だけ後段で当てる）
| Blitz 未対応 | 箇所 | 手当て |
|---|---|---|
| `overflow:auto` | 8 | `overflow:scroll` へ差し替え（または自前スクロール実装） |
| `position:sticky` | 4 | 固定ヘッダは `position:fixed` + 手動追従 |
| `backdrop-filter` | 7 | 背景色の不透明近似 |
| `mask-image` | 11 | グラデーション枠を自前ペイント or 省略 |
| `:has()` / `color-mix()` | 28 | Stylo の対応状況を実測（Firefox 相当なら通る見込み） |

## 3. 移植の進め方

1. **DOM 採取ハーネス**: 稼働中 GUI を操作して状態別に `outerHTML` を保存
   （初期画面 / セッション一覧 / 会話表示 / ストリーミング中 / ツールカード / 承認ダイアログ）
2. **差分テスト**: Rust が出力した DOM を正規化（空白・属性順）して採取物と比較
3. **パッケージ移植**: 依存の下流から
   `connection` → `store` → `ui-layout` → `ui-sidebar` → `ui-conversation` → `ui-chat` → `ui-tool`/`ui-approval` → `ui-command` …
4. **Markdown/ハイライト**: 59 言語のハイライトは Rust 実装（syntect 等）に置換し、**出力 class は本物に合わせる**

## 4. マイルストーン

### M0 CSS/DOM 適合検証（3〜4 日）
- [ ] 上流を shallow clone して TSX / CSS ソースを手元に置く
- [ ] GUI から 6 状態の DOM を採取
- [ ] 採取 HTML + 本物 CSS を Blitz で描画し、**崩れ箇所を一覧化**（スクショ比較）
- [ ] `overflow:auto` / `sticky` / `:has()` / `color-mix()` の実挙動を実測
- [ ] 日本語 IME（`<textarea>`、未確定表示、確定イベント）

**受入**: 崩れ箇所リストと workaround 表が出ている。ここが致命的なら方針再考。

### M1 ランタイム + 転送（6〜8 日）
- [ ] signals + keyed diff（React 相当の最小実装）
- [ ] 合成イベント（capture/bubble）、フォーカス管理、controlled input
- [ ] cordis 相当（サービス + イベント + 依存注入）
- [ ] `/api/remote.mux` 接続、`$events` 購読、endpoint 台帳を型化
- [ ] headless CLI で「一覧取得 → 送信 → 応答受信」

**受入**: CLI で Web UI と同じ内容が取れる。

### M2 シェル移植（6〜8 日）
- [ ] `ui-layout` / `ui-sidebar` を DOM 一致で移植
- [ ] セッション一覧・選択・新規作成
- [ ] テーマ切替、ウィンドウリサイズ、スクロール

**受入**: 初期画面の DOM が採取物と一致（差分ゼロ）。

### M3 会話移植（10〜12 日）
- [ ] `ui-conversation` / `ui-chat`: メッセージ、ストリーミング、Markdown、コードブロック、コピー
- [ ] `ui-tool` / `ui-approval`: ツールカード、承認、plan/todo、thinking
- [ ] 入力欄（IME・Enter/Shift+Enter・添付）、中断
- [ ] 自動スクロール（sticky 代替）

**受入 = MVP**: ブラウザを開かずに 1 ターン完結。DOM 差分テストが主要 6 状態で通る。

### M4 残り（必要に応じて）
設定画面、model 選択、ファイルサイドバー、成果物カード、ショートカット。

**見積り**: M0〜M3 で **25〜32 人日**（MVP）。M4 以降は対象パッケージ次第で +10 人日以上。
全 75 パッケージの完全移植は **数百人日**で、現実的な計画にはならない。

## 5. リスク

| # | リスク | 対策 |
|---|---|---|
| 1 | 本物 CSS が Blitz で崩れる（`overflow:auto`/`sticky`/`mask`） | M0 で実測。差し替えレイヤで吸収。致命的なら該当画面のみ自前スタイル |
| 2 | 日本語 IME | M0 最優先。不可なら自前テキストウィジェット（`~/dev/AntiWebUI` の text-input-v3 実装が参考） |
| 3 | React 相当ランタイムの作り込みが膨らむ | `ui-layout` 1 枚で先に完成させ、diff の必要機能をそこから逆算 |
| 4 | パッケージ間の暗黙依存（cordis サービス名・イベント名） | TSX/TS を読み、サービス契約を先に台帳化してから書く |
| 5 | 移植範囲の膨張 | MVP 対象パッケージを最初に確定して固定 |

## 6. 決定が必要なこと

1. MVP の対象パッケージ（既定: layout / sidebar / conversation / chat / tool / approval / markdown）
2. 上流ソースの参照方法（shallow clone を `~/dev/dsh-blitz/upstream/` に置くか）
3. 置き場所・名前（`~/dev/dsh-blitz/`）

## 7. 最初の 1 手

稼働中 GUI から初期画面の DOM を採取し、その HTML + 本物 CSS を Blitz で開いて崩れを数える。

```
# 1) 採取（browser MCP で document.querySelector('#root').outerHTML を保存）
# 2) 上流の CSS をコピーして 1 枚の HTML に束ねる
# 3) probe バイナリで描画 → スクショ比較
```

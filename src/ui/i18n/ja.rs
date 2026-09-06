use super::L10nKey;

pub fn translate_ja(key: L10nKey) -> Option<&'static str> {
    Some(match key {
        L10nKey::SearchTabs => "タブを検索…",
        L10nKey::SearchFiles => "ファイルを検索…",
        L10nKey::SearchThemes => "テーマを検索…",
        L10nKey::SearchSettings => "設定を検索…",
        L10nKey::FilterHosts => "ホストを絞り込み…",
        L10nKey::SearchCommandsOrHost => "コマンドを検索するか、user@host を入力して接続…",
        L10nKey::SearchTheme => "検索…",
        L10nKey::SearchWorkspacesAndMachines => "ワークスペース、タブ、マシンを検索…",
        L10nKey::SearchFonts => "フォントを検索…",
        L10nKey::SearchFind => "検索…",
        L10nKey::SearchMatchCase => "大文字と小文字を区別",
        L10nKey::SearchUseRegex => "正規表現を使用",
        L10nKey::NewFolderName => "新しいフォルダ名",
        L10nKey::NewFileName => "新しいファイル名",
        L10nKey::HomeNewTab => "新規タブ",
        L10nKey::HomeReopenClosedTab => "閉じたタブをもう一度開く",
        L10nKey::HomeSwitchWorkspace => "ワークスペースを切り替える…",
        L10nKey::HomeCommandPalette => "コマンドパレット…",
        L10nKey::HomeSplitRight => "右に分割",
        L10nKey::HomeSplitDown => "下に分割",
        L10nKey::HomeSettings => "設定…",
        L10nKey::TrayQuitStopServer => "終了してサーバーを停止…",
        L10nKey::Reconnect => "再接続",
        L10nKey::None => "なし",
        L10nKey::TryAgain => "再試行",
        L10nKey::Refreshing => "更新中…",
        L10nKey::Binary => "バイナリファイル",
        L10nKey::Delete => "削除",
        L10nKey::NoMatchingCommands => "一致するコマンドがありません",
        L10nKey::ConnectSshHint => "SSH で接続するには user@host を入力してください",
        L10nKey::EditHint => "編集",
        L10nKey::OpenFileFromTree => "ファイルツリーからファイルを開く",
        L10nKey::TreeDirLoading => "読み込み中…",
        L10nKey::TreeDirEmpty => "空",
        L10nKey::TreeDirHiddenOnly => "隠しファイルのみ",
        L10nKey::TreeDirUnreadable => "読み取れません",
        L10nKey::TreeSearchCapped => "最初の {n} 件のみ",
        L10nKey::TreeSearchFailed => "検索に失敗しました",
        L10nKey::FileChangedOnDisk => "ディスク上でファイルが変更されました",
        L10nKey::Reload => "再読み込み",
        L10nKey::KeepMine => "自分の変更を保持",
        L10nKey::Dismiss => "閉じる",
        L10nKey::StoredPasswordRejected => {
            "保存されたパスワードが拒否されました。新しいパスワードを入力してください"
        }
        L10nKey::StoredPassphraseRejected => {
            "保存されたパスフレーズではこの鍵を解除できませんでした。正しいものを入力してください"
        }
        L10nKey::Trust => "信頼する",
        L10nKey::Abort => "中止",
        L10nKey::HostKeyOverrideMessage => {
            "「yes」を入力すると新しいキーを上書きして信頼します。中止するには Esc を押してください"
        }
        L10nKey::Override => "上書き",
        L10nKey::RememberKeychain => "キーチェーンに保存",
        L10nKey::Cancel => "キャンセル",
        L10nKey::Close => "閉じる",
        L10nKey::QuitStopServerTitle => "tty7 を終了してサーバーを停止しますか？",
        L10nKey::QuitStopServerBody => {
            "tty7 を終了してバックグラウンドサーバーを停止します。シェルで実行中のものはすべて終了します。タブとレイアウトは次回起動時に新しいシェルで開きます。（ウィンドウを閉じるだけならトレイに退避し、シェルは動き続けます）"
        }
        L10nKey::QuitAndStop => "終了して停止",
        L10nKey::CloseSshConnectionTitle => "この SSH 接続を閉じますか？",
        L10nKey::CloseSshConnectionBody => "接続中です。閉じると切断されます",
        L10nKey::ClosePaneBusyTitle => "このペインを閉じますか？",
        L10nKey::CloseTabBusyTitle => "このタブを閉じますか？",
        L10nKey::CloseBusyCommandBody => "{what} はまだ実行中です。閉じると終了します。",
        L10nKey::CloseBusyAgentBody => {
            "{agent} はまだ作業中です。閉じるとこのターンは中断されます。"
        }
        L10nKey::Keep => "保持",
        L10nKey::SettingsNavAppearance => "外観",
        L10nKey::SettingsNavTerminal => "ターミナル",
        L10nKey::SettingsNavInput => "入力",
        L10nKey::SettingsNavSsh => "SSH",
        L10nKey::SettingsNavAgents => "エージェント",
        L10nKey::SettingsNavWindowTabs => "ウィンドウとタブ",
        L10nKey::SettingsNavKeybindings => "キーバインド",
        L10nKey::SettingsNavAbout => "情報",
        L10nKey::SettingsHeader => "設定",
        L10nKey::Reset => "リセット",
        L10nKey::Save => "保存",
        L10nKey::Connect => "接続",
        L10nKey::Download => "ダウンロード",
        L10nKey::Link => "リンク",
        L10nKey::SettingsThemeIntroTitle => "テーマ",
        L10nKey::SettingsThemeIntroDesc => {
            "配色テーマを選びます。明るいテーマと暗いテーマがあります"
        }
        L10nKey::SettingsTypography => "タイポグラフィ",
        L10nKey::SettingsFontSize => "フォントサイズ",
        L10nKey::SettingsFontSizeDesc => "ターミナルテキストのサイズ（ピクセル）",
        L10nKey::SettingsUiFontSize => "インターフェースのフォントサイズ",
        L10nKey::SettingsUiFontSizeDesc => {
            "ターミナル以外すべての文字サイズ（タブ・パネル・設定）。Retina でないディスプレイでは大きめに"
        }
        L10nKey::SettingsUiFontFamily => "インターフェースのフォントファミリー",
        L10nKey::SettingsUiFontFamilyDesc => {
            "タブ、サイドバー、ダイアログ、設定で使用するフォント。デフォルトではシステム UI フォントを使用します。"
        }
        L10nKey::SettingsLineHeight => "行の高さ",
        L10nKey::SettingsLineHeightDesc => "フォントサイズに対する行間の倍率",
        L10nKey::SettingsFontFamily => "フォントファミリー",
        L10nKey::SettingsFontFamilyDesc => "システムにインストールされているフォントから選択",
        L10nKey::SettingsBoldFont => "太字フォント",
        L10nKey::SettingsBoldFontDesc => {
            "太字テキストに使用する書体。デフォルトではメインフォントから合成されます"
        }
        L10nKey::SettingsItalicFont => "斜体フォント",
        L10nKey::SettingsItalicFontDesc => {
            "斜体テキストに使用する書体。デフォルトではメインフォントから合成されます"
        }
        L10nKey::SettingsFontLigatures => "フォントリガチャー",
        L10nKey::SettingsFontLigaturesDesc => {
            "ターミナルテキストで一般的なプログラミング用リガチャー（合字）を有効にする"
        }
        L10nKey::SettingsCursor => "カーソル",
        L10nKey::SettingsCursorShape => "カーソルの形状",
        L10nKey::SettingsCursorShapeDesc => "ターミナルカーソルの描画方法",
        L10nKey::SettingsCursorBlink => "カーソルの点滅",
        L10nKey::SettingsCursorBlinkDesc => {
            "ターミナルがフォーカスされている間、カーソルを点滅させる"
        }
        L10nKey::SettingsLanguage => "言語",
        L10nKey::SettingsLanguageDesc => "tty7 の表示言語を選択します",
        L10nKey::SettingsLanguageEnglish => "English",
        L10nKey::SettingsLanguageChinese => "简体中文",
        L10nKey::SettingsLanguageJapanese => "日本語",
        L10nKey::SettingsSearchLanguageKeywords => {
            "言語 ロケール 英語 中国語 language locale english chinese"
        }
        L10nKey::SettingsTransparency => "透明度",
        L10nKey::SettingsOpacity => "不透明度",
        L10nKey::SettingsOpacityDesc => {
            "すべてのテーマにおけるウィンドウ背景の不透明度。100% 未満ではデスクトップが透けて見えます"
        }
        L10nKey::SettingsBlur => "背景のぼかし",
        L10nKey::SettingsBlurDesc => {
            if cfg!(target_os = "macos") {
                "半透明ウィンドウの背後にあるものをぼかす"
            } else {
                "半透明ウィンドウの背後にあるものをぼかす。対応するコンポジターが必要です（KDE Plasma は対応、GNOME と素の X11 ではウィンドウが透けるだけです）"
            }
        }
        L10nKey::SettingsBlurAutoDesc => {
            "半透明ウィンドウの背後にあるものをぼかす。背景マテリアルが「自動」のときのみ有効です"
        }
        L10nKey::SettingsBackdrop => "背景マテリアル",
        L10nKey::SettingsBackdropDesc => {
            "半透明ウィンドウの背後にあるネイティブ Windows 背景マテリアル。Mica には Windows 11 22H2、Acrylic には 1809 が必要です。古いビルドでは自動的にフォールバックします"
        }
        L10nKey::SettingsSearchBackdropKeywords => {
            "背景 マテリアル ぼかし すりガラス material backdrop mica acrylic blur frosted window background"
        }
        L10nKey::SettingsBackdropAuto => "自動",
        L10nKey::SettingsBackdropBlur => "ぼかし",
        L10nKey::SettingsBackdropMica => "Mica",
        L10nKey::SettingsBackdropMicaAlt => "Mica Alt",
        L10nKey::SettingsBackdropAcrylic => "Acrylic",
        L10nKey::SettingsBackdropOff => "オフ",
        L10nKey::FollowTheme => "テーマに従う",
        L10nKey::SettingsDimInactivePanes => "非アクティブなペインを暗くする",
        L10nKey::SettingsDimInactivePanesDesc => {
            "分割内のフォーカスされていないペインを暗くし、アクティブなペインを目立たせる"
        }
        L10nKey::SettingsOpenThemesFolder => "テーマフォルダを開く",
        L10nKey::SettingsChangeThemeImage => "変更…",
        L10nKey::SettingsChooseThemeImage => "選択…",
        L10nKey::SettingsRemoveThemeImage => "削除",
        L10nKey::SettingsImageOpacity => "画像の不透明度",
        L10nKey::SettingsImageOpacityDesc => "背景色の上に画像をどれだけ強く表示するか",
        L10nKey::SettingsEditTheme => "テーマを編集",
        L10nKey::SettingsEditThemeIntro => {
            "コピーを編集します。変更はテーマフォルダ内のファイルに保存され、すぐ反映されます"
        }
        L10nKey::SettingsBackgroundImage => "背景画像",
        L10nKey::SettingsBackgroundImageDesc => "背景色の上、テキストの下に表示されます",
        L10nKey::SettingsAnsiColors => "ANSI カラー",
        L10nKey::SettingsCustomThemes => "カスタムテーマ",
        L10nKey::SettingsThemesRejected => "テーマフォルダから読み込めなかったもの",
        L10nKey::ThemeDuplicateFailed => "テーマを複製できませんでした",
        L10nKey::ThemeSaveFailed => "テーマを保存できませんでした",
        L10nKey::OpenInFileManagerFailed => "{path} を開けませんでした",
        L10nKey::ExplorerMenuOpenIn => "tty7 で開く",
        L10nKey::ExplorerMenuOpenHere => "ここで tty7 を開く",
        L10nKey::SettingsCustomThemesIntro => {
            "テーマを複製して色を編集するか、tty7 の YAML テーマや iTerm2 の .itermcolors をテーマフォルダに置いてください"
        }
        L10nKey::SettingsDuplicateToEdit => "複製して編集",
        L10nKey::SettingsHosts => "ホスト",
        L10nKey::SettingsDefaults => "デフォルト",
        L10nKey::SettingsInheritedByEveryHost => "すべてのホストに継承されます",
        L10nKey::SettingsNoSavedHosts => "保存済みホストはまだありません",
        L10nKey::SettingsNothingMatches => "「{query}」に一致する項目がありません",
        L10nKey::SettingsInTty7 => "tty7 内",
        L10nKey::SettingsImportFromSshConfig => "~/.ssh/config からインポート",
        L10nKey::SettingsExpandAllGroups => "すべてのグループを展開",
        L10nKey::SettingsNoHostsYet => "まだホストがありません",
        L10nKey::SettingsNothingSelected => "選択されていません",
        L10nKey::SettingsTypeAddressToConnect => {
            "アドレスを入力するとすぐに接続できます。tty7 はあとで保存するか尋ねます"
        }
        L10nKey::SettingsMoreInSshConfig => "~/.ssh/config にさらに {count} 件",
        L10nKey::SettingsAliasesLinked => "{count} 件のエイリアスがリンクされています",
        L10nKey::SettingsImportAliases => "エイリアスをインポート",
        L10nKey::SettingsImportAliasesDesc => {
            "ファイルを再読み込みして新しい項目を追加します。ここでの編集は tty7 が保存します — ファイル自体には書き込まれません"
        }
        L10nKey::SettingsImportNow => "今すぐインポート",
        L10nKey::SettingsImportUnreadable => {
            "{path} を読み取れませんでした — 何もインポートされていません"
        }
        L10nKey::SettingsImportNoHosts => {
            "{path} にインポートできるホストがありません — ワイルドカードや Match のルールだけです"
        }
        L10nKey::SettingsImportSummary => {
            "ホスト {count} 件を追加 — {updated} 件を更新、{unchanged} 件は変更なし"
        }
        L10nKey::SettingsImportIgnored => {
            "tty7 に設定のないオプションが {count} 件あり、ファイルに残されています: {options}"
        }
        L10nKey::SettingsImportMoreOptions => "他 {count} 件",
        L10nKey::SettingsDefaultsIntro => {
            "すべてのホストはこの設定から始まります。各ホストは詳細設定で個別に上書きできます"
        }
        L10nKey::SettingsCopyAddress => "アドレスをコピー",
        L10nKey::SettingsDuplicate => "複製",
        L10nKey::SettingsForgetPassword => "パスワードを消去",
        L10nKey::SettingsForgetPasswordTitle => "{endpoint} の保存されたパスワードを消去しますか？",
        L10nKey::SettingsForgetPasswordBody => {
            "次に接続するときに、もう一度パスワードを尋ねられます。このホストの他の設定は変わりません"
        }
        L10nKey::SettingsForgetPasswordSharedBody => {
            "他にも {count} 件のホストプロファイルが {endpoint} を使っているため、それらの接続でもパスワードの再入力が必要になります"
        }
        L10nKey::SettingsForgotPasswordFor => "{endpoint} の保存されたパスワードを消去しました",
        L10nKey::SettingsDeleteProfileBody => {
            "保存されたパスワードも一緒に削除されます。同じアドレスを使う接続が他にある場合は残ります。"
        }
        L10nKey::SettingsDeleteProfileCascade => {
            "{endpoint} を参照するリモートワークスペースのエントリが {count} 件あり、一緒に削除されます。リモートマシン上のセッションは動いたままで、新しいプロファイルで接続すれば一覧に戻ります。"
        }
        L10nKey::SettingsCouldntForgetPassword => {
            "{endpoint} のパスワードを消去できませんでした: {error}"
        }
        L10nKey::SettingsSecurity => "セキュリティ",
        L10nKey::SettingsSecurityIntro => "ホストは詳細設定でこれらを上書きできます",
        L10nKey::SettingsVerifyHostKeys => "ホストキーを検証",
        L10nKey::SettingsVerifyHostKeysDesc => {
            "接続前に各サーバーのキーを known_hosts と照合します。オフでは確認しないため、なりすましサーバーに気づきません"
        }
        L10nKey::WarnBeforeClosing => "閉じる前に警告",
        L10nKey::SettingsWarnBeforeClosingDesc => {
            "アクティブな SSH セッションのあるタブやペインを閉じる前に確認を求めます"
        }
        L10nKey::SettingsNewHost => "新規ホスト",
        L10nKey::SettingsDiscardChangesTitle => "保存していない変更を破棄しますか？",
        L10nKey::SettingsDiscardChangesBody => "編集中の接続に、まだ保存していない変更があります。",
        L10nKey::SettingsKeepEditing => "編集を続ける",
        L10nKey::SettingsName => "名前",
        L10nKey::SettingsNameDesc => "この接続の表示名",
        L10nKey::SettingsHost => "ホスト名",
        L10nKey::SettingsHostDesc => "ホスト名または IP アドレス",
        L10nKey::SettingsHostRequired => "ホスト名が必要です — 保存されません",
        L10nKey::SettingsPortInvalid => "ポートは 1-65535 の範囲です — 空欄なら 22 です",
        L10nKey::SettingsUser => "ユーザー名",
        L10nKey::SettingsUserDesc => "ログインユーザー (空欄 = 接続時に解決)",
        L10nKey::SettingsAuth => "認証方式",
        L10nKey::SettingsAuthDesc => "認証方式。自動の場合は適用可能なすべての方式を試します",
        L10nKey::SettingsAuthModeAuto => "自動",
        L10nKey::SettingsAuthModePassword => "パスワード",
        L10nKey::SettingsAuthModeKey => "公開鍵",
        L10nKey::SettingsAuthModeAgent => "SSH エージェント",
        L10nKey::SettingsAuthMode2Fa => "二要素認証 (2FA)",
        L10nKey::SettingsJumpHost => "ジャンプホスト",
        L10nKey::SettingsJumpHostDesc => {
            "トンネリングに使用する別のプロファイル名 (空欄 = 直接接続)"
        }
        L10nKey::SettingsJumpHostUnknown => {
            "{jump_name} という名前のホストプロファイルはありません — 保存されません"
        }
        L10nKey::SettingsJumpHostSelf => {
            "ホストを自分自身のジャンプホストにはできません — 保存されません"
        }
        L10nKey::SettingsNoneSummary => "(なし)",
        L10nKey::SettingsPortForwarding => "ポートフォワーディング",
        L10nKey::SettingsRulesOpenedWithConnection => "接続と同時に開くルール 1 件",
        L10nKey::SettingsAddRule => "+ ルールを追加",
        L10nKey::SettingsRemoveRule => "ルールを削除",
        L10nKey::SettingsFwdLegendLocal => "L — ローカルポートからリモート側へアクセスできる",
        L10nKey::SettingsFwdLegendRemote => "R — リモートポートからこのマシンへアクセスできる",
        L10nKey::SettingsFwdLegendDynamic => "D — ダイナミック SOCKS プロキシ",
        L10nKey::SettingsFwdNeedsBoth => {
            "待受ポートとターゲットの host:port が必要です — 保存されません"
        }
        L10nKey::SettingsFwdNeedsListen => "待受ポートが必要です — 保存されません",
        L10nKey::SettingsAdvanced => "詳細設定",
        L10nKey::SettingsAdvancedSummary => {
            "アルゴリズム / キープアライブ / プロキシ / X11 / ログインスクリプト"
        }
        L10nKey::SettingsGroupAuthentication => "認証",
        L10nKey::SettingsGroupProxies => "プロキシ",
        L10nKey::SettingsGroupAlgorithms => "アルゴリズム",
        L10nKey::SettingsGroupConnection => "接続",
        L10nKey::SettingsGroupSession => "セッション",
        L10nKey::SettingsGroupSecurity => "セキュリティ",
        L10nKey::SettingsRemoteClipboardWrite => "リモートのクリップボード画像",
        L10nKey::SettingsRemoteClipboardWriteDesc => {
            "このホスト上のプログラムが OSC 5522 でローカルクリップボードを画像に置き換えることを許可します"
        }
        L10nKey::SettingsIdentityFiles => "秘密鍵ファイル",
        L10nKey::SettingsIdentityFilesDesc => "秘密鍵のパス（1 行に 1 つ。%h/%r は展開されます）",
        L10nKey::SettingsAgentForwarding => "エージェント転送",
        L10nKey::SettingsAgentForwardingDesc => "ローカルの ssh-agent を接続先へ転送します",
        L10nKey::SettingsProxyCommand => "ProxyCommand",
        L10nKey::SettingsProxyCommandDesc => "転送コマンド（%h/%p/%r は置換されます）",
        L10nKey::SettingsSocks5Proxy => "SOCKS5 プロキシ",
        L10nKey::SettingsSocks5ProxyDesc => "host:port（空欄 = なし）",
        L10nKey::SettingsHttpProxy => "HTTP プロキシ",
        L10nKey::SettingsHttpProxyDesc => "host:port（空欄 = なし）",
        L10nKey::SettingsProxyOverridden => "使われません：{winner} が優先されます",
        L10nKey::SettingsTestConnection => "テスト",
        L10nKey::SettingsTestRunning => "接続をテスト中…",
        L10nKey::SettingsTestReached => "接続と認証に成功しました（{time}）",
        L10nKey::SettingsTestNeedsPassword => {
            "サーバーに到達しました — パスワードを求められています。接続して入力してください"
        }
        L10nKey::SettingsTestNeedsPassphrase => {
            "サーバーに到達しました — 秘密鍵のパスフレーズを求められています。接続して入力してください"
        }
        L10nKey::SettingsTestNeedsInteractive => {
            "サーバーに到達しました — キーボードインタラクティブ認証を求められています。接続して応答してください"
        }
        L10nKey::SettingsTestNeedsHostKey => {
            "サーバーに到達しました — ホストキーがまだ承認されていません。一度接続して確認してください"
        }
        L10nKey::SettingsTestHostKeyChanged => {
            "サーバーに到達しました — ホストキーが以前のものと異なります。一度接続して変更内容を確認してください"
        }
        L10nKey::SettingsTestFailed => "接続できませんでした: {reason}",
        L10nKey::SettingsProxyPortInvalid => {
            "ポートは 1-65535 の範囲です — ホストだけならデフォルトポートを使います"
        }
        L10nKey::SettingsKexAlgorithms => "KEX アルゴリズム",
        L10nKey::SettingsKexAlgorithmsDesc => "カンマ区切り（空欄 = ライブラリのデフォルト）",
        L10nKey::SettingsCiphers => "暗号方式",
        L10nKey::SettingsCiphersDesc => "カンマ区切り（空欄 = デフォルト）",
        L10nKey::SettingsMacs => "MAC アルゴリズム",
        L10nKey::SettingsMacsDesc => "カンマ区切り（空欄 = デフォルト）",
        L10nKey::SettingsHostKeyAlgorithms => "ホストキーアルゴリズム",
        L10nKey::SettingsHostKeyAlgorithmsDesc => "カンマ区切り（空欄 = デフォルト）",
        L10nKey::SettingsCompression => "圧縮アルゴリズム",
        L10nKey::SettingsJumpHostVia => "{jump_name} 経由",
        L10nKey::SettingsConnected => "接続済み",
        L10nKey::SettingsProfileCopied => "{name}（コピー）",
        L10nKey::SettingsCompressionDesc => "カンマ区切り（空欄 = デフォルト）",
        L10nKey::SettingsKeepaliveInterval => "Keepalive 間隔（秒）",
        L10nKey::SettingsKeepaliveIntervalDesc => "空欄 = ライブラリのデフォルト",
        L10nKey::SettingsKeepaliveCountMax => "Keepalive 最大試行回数",
        L10nKey::SettingsKeepaliveCountMaxDesc => "キープアライブが何回失敗すると切断扱いにするか",
        L10nKey::SettingsConnectTimeout => "接続タイムアウト（秒）",
        L10nKey::SettingsConnectTimeoutDesc => "空欄 = ライブラリのデフォルト",
        L10nKey::SettingsX11Forwarding => "X11 転送",
        L10nKey::SettingsX11ForwardingDesc => {
            if cfg!(target_os = "macos") {
                "X11 転送を要求（XQuartz が必要）"
            } else if cfg!(target_os = "windows") {
                "X11 転送を要求（VcXsrv や X410 などの X サーバーの起動が必要）"
            } else {
                "X11 転送を要求"
            }
        }
        L10nKey::SettingsShellIntegration => "シェル統合",
        L10nKey::SettingsShellIntegrationDesc => {
            "リモートシェルにプロンプト・終了コード・作業ディレクトリを報告させる"
        }
        L10nKey::SettingsLoginScripts => "ログインスクリプト",
        L10nKey::SettingsLoginScriptsDesc => "シェル起動後に送信するコマンド（1 行に 1 つ）",
        L10nKey::SettingsSkipBanner => "バナーをスキップ",
        L10nKey::SettingsSkipBannerDesc => "サーバーのログインバナーを非表示にする",
        L10nKey::SettingsDefaultFollowsDefaults => {
            "「デフォルト」はデフォルト設定に従います。現在は {value}"
        }
        L10nKey::SettingsValueOn => "オン",
        L10nKey::SettingsValueOff => "オフ",
        L10nKey::SettingsDefault => "デフォルト",
        L10nKey::SettingsOn => "オン",
        L10nKey::SettingsOff => "オフ",
        L10nKey::SettingsShell => "シェル",
        L10nKey::SettingsShellIntro => {
            "新しいターミナルで起動するプログラム。空欄ならプラットフォーム既定の {default} を使います"
        }
        L10nKey::SettingsProgram => "プログラム",
        L10nKey::SettingsProgramDesc => {
            "PATH 上の実行可能ファイル名または絶対パス。例: zsh、fish、pwsh"
        }
        L10nKey::SettingsArguments => "引数",
        L10nKey::SettingsArgumentsDesc => {
            "コマンドラインと同じ規則で分割される起動フラグ。空白を含むものはクォートしてください（例: -l、-c \"echo hi\"）"
        }
        L10nKey::SettingsArgumentsInvalid => {
            "引用符が対応していないため、この値は保存されませんでした"
        }
        L10nKey::SettingsStartIn => "初期作業ディレクトリ",
        L10nKey::SettingsStartInDesc => {
            "新しいシェルの開始場所: tty7 の起動ディレクトリ、ホームフォルダ、または固定パス"
        }
        L10nKey::SettingsCustomPath => "カスタムパス",
        L10nKey::SettingsCustomPathDesc => "新しいシェルが起動するディレクトリ",
        L10nKey::SettingsWdInherit => "継承",
        L10nKey::SettingsWdHome => "ホーム",
        L10nKey::SettingsWdCustom => "カスタム",
        L10nKey::SettingsWdPathInvalid => {
            "このディレクトリは存在しないため、この値は保存されませんでした"
        }
        L10nKey::SettingsShellFooter => {
            "継承元のないシェル（ウィンドウの最初のタブなど）に適用されます。新しいタブと分割はアクティブなペインのディレクトリを引き継ぎ、開いているシェルは動き続けます"
        }
        L10nKey::SettingsScrolling => "スクロール",
        L10nKey::SettingsScrollback => "スクロールバック",
        L10nKey::SettingsScrollbackDesc => {
            "各ペインに保存する履歴の行数。新しいペインに適用されます"
        }
        L10nKey::SettingsScrollSpeed => "スクロール速度",
        L10nKey::SettingsScrollSpeedDesc => "マウスホイールのスクロールに適用する倍率",
        L10nKey::SettingsSmoothScroll => "スムーズスクロール",
        L10nKey::SettingsSmoothScrollDesc => {
            "ホイール1ノッチ分を一気に飛ばさず、数フレームかけて動かす。\
             トラックパッドは元から連続的なので影響しない"
        }
        L10nKey::SettingsMouse => "マウス",
        L10nKey::SettingsFocusFollowsMouse => "フォーカスがマウスに追従する",
        L10nKey::SettingsFocusFollowsMouseDesc => {
            "クリックしなくてもペインにホバーするとフォーカスされる"
        }
        L10nKey::SettingsHideMouseWhileTyping => "入力時にマウスポインタを非表示",
        L10nKey::SettingsHideMouseWhileTypingDesc => {
            "入力中はポインタを隠し、次のマウス移動で再表示する"
        }
        L10nKey::SettingsMouseZoom => "ホイールで拡大縮小",
        L10nKey::SettingsMouseZoomDesc => {
            "この修飾キーを押しながらホイールを回すと、スクロールではなくフォントサイズが変わる"
        }
        L10nKey::SettingsMouseZoomOff => "オフ",
        L10nKey::SettingsReportMouseToApps => "マウスイベントをアプリに報告",
        L10nKey::SettingsReportMouseToAppsDesc => {
            "フルスクリーンアプリ（vim、tmux）にクリックとスクロールを処理させる。Shift を押している間はローカルで処理されます"
        }
        L10nKey::SettingsBell => "ベル通知",
        L10nKey::SettingsTerminalBell => "ターミナルベル",
        L10nKey::SettingsTerminalBellDesc => {
            "ベル（^G）の通知方法: サイレント、短い点滅、システムサウンド、またはその両方"
        }
        L10nKey::SettingsLinks => "リンク",
        L10nKey::DetectUrls => "URL を自動検出",
        L10nKey::SettingsDetectUrlsDesc => {
            "ホバーでリンクに下線を表示し、{modifier}+クリックで開く"
        }
        L10nKey::ForwardSshLoopbackLinks => "SSH ループバックリンクを転送",
        L10nKey::SettingsForwardSshLoopbackLinksDesc => {
            "ペインが SSH 接続中の場合、一時的なポートフォワード経由で localhost リンクを開く"
        }
        L10nKey::SettingsOpenFilesInternal => "内蔵エディタ",
        L10nKey::SettingsOpenFilesSystem => "デフォルトアプリ",
        L10nKey::SettingsOpenFilesCommand => "コマンド",
        L10nKey::SettingsOpenFilesModeDesc => {
            "ファイルリンクを {modifier}+クリックしたときに開くもの。行番号へのジャンプとリモートファイルを開けるのは内蔵エディタだけです"
        }
        L10nKey::LinkFileNotUnder => "{path} — {dir} にそのファイルはありません",
        L10nKey::LinkFileNoDirectory => {
            "{path} — このペインはどのディレクトリにいるかを報告していないため、相対パスの起点がありません"
        }
        L10nKey::LinkFileMissing => "{path} — そのパスには何もありません",
        L10nKey::LinkDirOutsideTree => {
            "{path} — 別のマシン上にあり、ファイルパネルで開いているどのフォルダにも含まれていません"
        }
        L10nKey::OpenFilesWith => "ファイルを開くアプリケーション",
        L10nKey::SettingsOpenFilesWithDesc => {
            "ファイルリンクを {modifier}+クリックしたときに実行するコマンド。{path}、{line}、{column} を使えます — 値のないフラグは除外されます。空欄ならデフォルトアプリ"
        }
        L10nKey::SettingsBellModeOff => "オフ",
        L10nKey::SettingsBellModeVisual => "視覚的（画面点滅）",
        L10nKey::SettingsBellModeAudible => "音声（効果音）",
        L10nKey::SettingsBellModeBoth => "点滅 + 音声",
        L10nKey::SettingsPrompt => "プロンプト",
        L10nKey::SettingsPromptIntro => {
            "シェルプロンプトでの tty7 独自のエディターとメニュー。オフにするとその分がシェルに渡されます"
        }
        L10nKey::SettingsPromptEditor => "プロンプトエディター",
        L10nKey::SettingsPromptEditorDesc => {
            "シェルプロンプトで入力する行を tty7 が編集します — 選択、取り消し、下のメニュー。オフにするとシェル自身の行エディター（ZLE、readline、fish）に戻ります"
        }
        L10nKey::SettingsNeedsPromptEditor => {
            "プロンプトエディターが必要です。オフの間、このキーはすでにシェルのものです"
        }
        L10nKey::SettingsTabCompletion => "タブ補完",
        L10nKey::SettingsTabCompletionDesc => {
            "プロンプトで Tab を押すと tty7 の補完メニューが開きます。オフの場合、Tab はシェル自身の補完に渡されます"
        }
        L10nKey::SettingsHistorySearch => "履歴検索",
        L10nKey::SettingsHistorySearchDesc => {
            "プロンプトで ⌃R を押すと tty7 のファジー履歴メニューが開きます。オフなら ⌃R はシェルへ — 逆方向検索や、そこでバインドしたもの（fzf、percol）"
        }
        L10nKey::SettingsSelectionClipboard => "選択とクリップボード",
        L10nKey::SettingsSmartSelection => "スマート選択",
        L10nKey::SettingsSmartSelectionDesc => {
            "ダブルクリックでカーソル下の URL、ファイルパス、メールアドレス、または括弧ペア全体を選択"
        }
        L10nKey::SettingsCopyOnSelect => "選択時に自動コピー",
        L10nKey::SettingsCopyOnSelectDesc => {
            if cfg!(target_os = "macos") {
                "マウスでテキストを選択するとすぐにクリップボードへコピーされます。⌘C は不要です"
            } else {
                "マウスでテキストを選択するとすぐにクリップボードへコピーされます。Ctrl+Shift+C は不要です"
            }
        }
        L10nKey::SettingsTrimTrailingSpaces => "コピー時に末尾の空白を除去",
        L10nKey::SettingsTrimTrailingSpacesDesc => "コピーした各行の末尾の空白を除去する",
        L10nKey::SettingsKeyboard => "キーボード",
        L10nKey::SettingsOptionAsMeta => "Option（⌥）を Meta として使用",
        L10nKey::SettingsOptionAsMetaDesc => {
            "⌥+キーでシェルが期待するエスケープシーケンス（⌥B = 単語 1 つ戻る）を送信し、特殊文字（∫）を入力しない"
        }
        L10nKey::SettingsAgentsIntro => "エージェント",
        L10nKey::SettingsAgentsIntroDesc => {
            "フックにより、これらのエージェントを実行するペインの状態（作業中 / 待機中 / 完了）がタブバーに表示されます。tty7 内でのみ有効"
        }
        L10nKey::SettingsReadingAgentConfig => "このマシンのエージェント設定を読み込んでいます…",
        L10nKey::SettingsStatusNotInstalled => "未インストール",
        L10nKey::SettingsStatusInstalled => "インストール済み",
        L10nKey::SettingsStatusOutdated => "更新あり",
        L10nKey::SettingsInstall => "インストール",
        L10nKey::SettingsReinstall => "再インストール",
        L10nKey::SettingsUpdate => "アップデート",
        L10nKey::SettingsUninstall => "アンインストール",
        L10nKey::SettingsOfflineMachines => {
            "未接続の保存済みマシンがさらに {count} 台あります。いずれかでワークスペースを開くと、そこにフックをインストールできます"
        }
        L10nKey::SettingsSyncWithSystem => "システムテーマと同期",
        L10nKey::SettingsSyncWithSystemDesc => {
            "OS の外観に従い、ライトとダークのテーマを別々に使用する"
        }
        L10nKey::SettingsLegiblePalette => "明色の可読性",
        L10nKey::SettingsLegiblePaletteDesc => {
            "テーマ背景でコントラスト不足の明色を自動調整して、可読性を確保します。"
        }
        L10nKey::SettingsChangeTheme => "テーマを変更",
        L10nKey::SettingsThemes => "テーマ一覧",
        L10nKey::SettingsThemesCloseTooltip => "テーマ一覧を閉じる (Esc)",
        L10nKey::SettingsThemePanelManual => "現在のテーマを変更",
        L10nKey::SettingsThemePanelLight => "ライトモード用のテーマを選択",
        L10nKey::SettingsThemePanelDark => "ダークモード用のテーマを選択",
        L10nKey::SettingsCustom => "カスタム",
        L10nKey::SettingsCustomValue => "カスタム ({value})",
        L10nKey::SettingsBuiltIn => "組み込み",
        L10nKey::SettingsDark => "ダーク",
        L10nKey::SettingsLight => "ライト",
        L10nKey::SettingsLightMode => "ライトモード",
        L10nKey::SettingsDarkMode => "ダークモード",
        L10nKey::SettingsActive => "アクティブ",
        L10nKey::SettingsStartupWindow => "起動時のウィンドウ状態",
        L10nKey::SettingsStartupWindowDesc => "tty7 起動時のウィンドウ状態",
        L10nKey::SettingsRememberWindowSize => "ウィンドウサイズと位置を記憶",
        L10nKey::SettingsRememberWindowSizeDesc => {
            "tty7 が最後に終了したときのサイズと位置で開き直します。オフならデフォルトサイズで中央に開きます"
        }
        L10nKey::SettingsRestoreLastLayout => "前回のレイアウトを復元",
        L10nKey::SettingsRestoreLastLayoutDesc => {
            "起動時に前回のウィンドウのタブ、分割、ディレクトリを復元します。オフなら新しいターミナルが 1 つだけ起動します"
        }
        L10nKey::SettingsShowTrayIcon => "システムトレイアイコンを表示",
        L10nKey::SettingsShowTrayIconDesc => {
            "システムトレイ / メニューバーの状態表示：エージェントが入力を必要とするときに通知し、メニューからそのペインへ移動できます"
        }
        L10nKey::SettingsTabs => "タブ",
        L10nKey::SettingsNewTabPosition => "新規タブの表示位置",
        L10nKey::SettingsNewTabPositionDesc => "新しく開いたタブが挿入される場所",
        L10nKey::SettingsTabBarPosition => "タブバーの位置",
        L10nKey::SettingsTabBarPositionDesc => {
            "タブを上部の横一列または左側の縦サイドバーとして表示"
        }
        L10nKey::SettingsSidebarGrouping => "サイドバーのグループ化",
        L10nKey::SettingsSidebarGroupingDesc => {
            "サイドバータブを git リポジトリごとにまとめます。リポジトリ外のタブはスクラッチに、「リポジトリ／フォルダ別」なら作業ディレクトリごとに。左サイドバーのみ"
        }
        L10nKey::SettingsDiffPreviewFromCounts => "サイドバーのカウントから Diff プレビューを開く",
        L10nKey::SettingsDiffPreviewFromCountsDesc => {
            "行の +N −N をクリックすると、オーバーレイでワーキングツリーの Diff を開きます。オフならカウントは表示されたまま、クリックだけできません"
        }
        L10nKey::DocumentDock => "ターミナルの隣にドック",
        L10nKey::DocumentFill => "ウィンドウ全体",
        L10nKey::SettingsNotifications => "通知",
        L10nKey::SettingsNotifyOnCommandFinish => "コマンド終了時に通知",
        L10nKey::SettingsNotifyOnCommandFinishDesc => {
            "長時間のフォアグラウンドコマンドが完了したらデスクトップ通知を表示"
        }
        L10nKey::SettingsNotifyThreshold => "通知閾値（秒）",
        L10nKey::SettingsNotifyThresholdDesc => "「長時間」とみなすのに必要なコマンドの実行時間",
        L10nKey::SettingsWindow => "ウィンドウ",
        L10nKey::NotifyModeNever => "通知しない",
        L10nKey::NotifyModeUnfocused => "非フォーカス時のみ",
        L10nKey::NotifyModeAlways => "常に通知",
        L10nKey::SettingsStartupNormal => "通常サイズ",
        L10nKey::SettingsStartupMaximized => "最大化",
        L10nKey::SettingsStartupFullscreen => "全画面",
        L10nKey::SettingsAfterCurrent => "現在のタブの隣",
        L10nKey::SettingsAtEnd => "末尾",
        L10nKey::SettingsTop => "上部",
        L10nKey::SettingsLeft => "左側",
        L10nKey::SettingsByRepo => "リポジトリ別",
        L10nKey::SettingsByRepoOrFolder => "リポジトリ／フォルダ別",
        L10nKey::SettingsFlat => "フラット表示",
        L10nKey::SettingsPreset => "プリセット",
        L10nKey::SettingsPresetDesc => {
            "tmux では、ペイン/タブの操作をプレフィックスキーの後に行います（例: Ctrl-B の後に C）"
        }
        L10nKey::SettingsPrefix => "プレフィックスキー",
        L10nKey::SettingsPressKeys => "キーを入力…",
        L10nKey::SettingsPauseToSaveEsc => "一時停止して保存 · Esc",
        L10nKey::SettingsKeybindingsIntroDesc => {
            "ショートカットをクリックして新しいキーを押すと、少し間を置いて保存されます。Ctrl-B の後に X のようなシーケンスはキーを続けて入力。Esc でキャンセル、Backspace は最後のキーを削除し、最初に押すとデフォルトに戻します"
        }
        L10nKey::SettingsPrefixNote => {
            "プレフィックスが有効な場合、プレフィックスキーを単独で押すと約 1 秒後にシェルに渡され、プレフィックス + 未割り当てのキーはターミナルへそのまま送信されます"
        }
        L10nKey::SettingsRestoreAllDefaults => "すべてのデフォルトを復元",
        L10nKey::SettingsRestoreAllDefaultsBody => {
            "変更したキーはすべてデフォルトに戻ります。元に戻すことはできません。"
        }
        L10nKey::KeybindGoToTab => "タブ {n} へ移動",
        L10nKey::KeybindGoToWorkspace => "ワークスペース {n} へ移動",
        L10nKey::KeybindInsertNewline => "改行を挿入",
        L10nKey::KeybindForkSessionRight => "右にセッションをフォーク",
        L10nKey::KeybindForkSessionLeft => "左にセッションをフォーク",
        L10nKey::KeybindForkSessionDown => "下にセッションをフォーク",
        L10nKey::KeybindForkSessionUp => "上にセッションをフォーク",
        L10nKey::SettingsAboutDesc1 => {
            "ターミナルワークベンチ: 常駐セッション、リモート作業、エージェント"
        }
        L10nKey::SettingsVersion => "バージョン",
        L10nKey::SettingsUpdates => "アップデート",
        L10nKey::SettingsUpdateAndRelaunch => "更新して再起動",
        L10nKey::SettingsUpdateViewRelease => "リリースページを開く",
        L10nKey::SettingsUpdateChecking => "アップデートを確認中…",
        L10nKey::SettingsUpdateUpToDate => "最新バージョンを使用しています",
        L10nKey::SettingsUpdateDownloadingPercent => {
            "アップデートをダウンロード中… {size} 中 {percent}%"
        }
        L10nKey::SettingsUpdateDownloadingBytes => "アップデートをダウンロード中… {received}",
        L10nKey::SettingsUpdateVerifying => "ダウンロードしたアップデートを検証中…",
        L10nKey::SettingsUpdateInstalling => "アップデートを適用して再起動中…",
        L10nKey::SettingsUpdateCheckNow => "今すぐ確認",
        L10nKey::SettingsUpdateCancel => "ダウンロードを中止",
        L10nKey::SettingsUpdateRetry => "再試行",
        L10nKey::SettingsUpdateDismiss => "閉じる",
        L10nKey::SettingsUpdateDownloadManually => "手動でダウンロード",
        L10nKey::SettingsUpdateFailedTitle => "{version} へのアップデートに失敗しました。",
        L10nKey::SettingsUpdateReady => {
            "{version} のダウンロードと検証が完了し、インストールできます。"
        }
        L10nKey::SettingsUpdateReadyNextLaunch => "次回 tty7 を起動したときに適用されます。",
        L10nKey::SettingsUpdateInstallNow => "インストールして再起動",
        L10nKey::SettingsUpdateDiscard => "破棄",
        L10nKey::SettingsAutoDownload => "アップデートをバックグラウンドでダウンロード",
        L10nKey::SettingsAutoDownloadDesc => {
            "新しいリリースを見つけ次第ダウンロードと検証を済ませ、インストールは再起動するだけにします。確認なしにインストールすることはありません。パッケージは約 30 MB"
        }
        L10nKey::SettingsUpdateChannel => "更新チャンネル",
        L10nKey::SettingsUpdateChannelDesc => {
            "Stable は正式リリースを、Nightly は最新のコードから毎晩ビルドされる版を追いかけます。新しい代わりに、リリース前のテストは経ていません"
        }
        L10nKey::SettingsUpdateChannelStable => "安定版",
        L10nKey::SettingsUpdateChannelNightly => "ナイトリー",
        L10nKey::SettingsDaemonStale => "バックグラウンドサーバーは {build} のままです。",
        L10nKey::SettingsDaemonStaleDesc => {
            "tty7 はその場で更新されました。アプリは新しく、ペインはまだ以前のビルドのサーバーが処理しています。再起動すると新しいビルドに切り替わり、ペインで動いているプロセスはすべて終了します。急ぐ必要はなく、ペインが空いているときにどうぞ"
        }
        L10nKey::UpdateDialogTitle => "アップデートがあります",
        L10nKey::UpdateDialogDetail => {
            "tty7 {version} が利用できます（現在 {current}）。インストールするとアプリが再起動します。バックグラウンドサーバーは動いたままなので、ペインの中身は残ります"
        }
        L10nKey::UpdateDialogDetailWindows => {
            "tty7 {version} が利用できます（現在 {current}）。インストールするとアプリとバックグラウンドサービスが再起動します。ペインのプロセスは終了し、タブとレイアウトは新しいシェルで復元されます"
        }
        L10nKey::UpdateDialogDetailManual => {
            "tty7 {version} が利用できます（現在 {current}）。{hint}"
        }
        L10nKey::UpdateDialogCannotSelfUpdate => "このインストールは自動更新できません。",
        L10nKey::UpdateDialogLater => "後で",
        L10nKey::UpdateDialogNextLaunch => "次回起動時にインストール",
        L10nKey::UpdateDialogNeedsElevation => {
            "tty7 は全ユーザー向けにインストールされているため、インストール前に Windows の管理者承認が一度求められます。tty7 自体が管理者権限で実行されることはありません"
        }
        L10nKey::SettingsUpdateCheckFailed => "アップデートを確認できませんでした: {error}",
        L10nKey::SettingsUpdatePrepareFailed => "アップデートに失敗しました: {error}",
        L10nKey::SettingsUpdateLaunchFailed => "インストーラーを起動できませんでした: {error}",
        L10nKey::SettingsUpdateUnsupportedMacos => {
            "この tty7 は書き込み可能な tty7.app バンドルにないため、自分自身を置き換えられません。「アプリケーション」へ移動するか、リリースページから更新してください"
        }
        L10nKey::SettingsUpdateUnsupportedLinux => {
            "このアーキテクチャ向けの Linux パッケージはリリースにありません。ソースからビルドするか、パッケージマネージャーをご利用ください"
        }
        L10nKey::SettingsUpdateLinuxPackage => {
            "Linux は手動で更新します。リリースページから {name} をダウンロードするか、パッケージマネージャーをご利用ください"
        }
        L10nKey::SettingsUpdateUnsupportedWindows => {
            "この tty7 は認識可能な Inno Setup 版でもポータブル ZIP 版でもないため、自動更新できません。リリースページを開いて手動で更新してください"
        }
        L10nKey::SettingsUpdateWindowsAllUsers => {
            "tty7 はすべてのユーザー向けにインストールされており、置き換えには管理者権限が必要ですが、tty7 は自ら昇格を要求しません。リリースページからインストーラーを実行して更新してください"
        }
        L10nKey::SettingsUpdateUnsupportedPlatform => {
            "このプラットフォームでは自動インストールを利用できません。リリースページを開いてください"
        }
        L10nKey::SettingsUpdateMissingPackage => {
            "このリリースには、現在のインストール形式に合う {name} パッケージがありません。リリースページを開いて別のパッケージを選んでください"
        }
        L10nKey::SettingsUpdateMissingChecksums => {
            "このリリースには checksums.txt がないため、tty7 は自動インストールを行いません"
        }
        L10nKey::SettingsVersionAvailable => "バージョン {version} が利用可能です",
        L10nKey::SettingsCheckUpdatesDesc => {
            "その場で更新できないインストール形式では、代わりにリリースページを開きます"
        }
        L10nKey::SettingsCheckUpdatesOnLaunch => "起動時にアップデートを確認",
        L10nKey::SettingsCommandLine => "コマンドライン",
        L10nKey::SettingsCommandLineDesc => {
            "同梱の tty7 コマンドを PATH に入れ、スクリプトやエージェントが任意のターミナルから tty7 を操作できるようにします（ペイン内ではどちらでも動きます）。自分でビルドした tty7 を優先したい場合はオフに。次回起動時に有効"
        }
        L10nKey::SettingsInstallCliOnPath => "`tty7` コマンドを PATH にインストール",
        L10nKey::SettingsServer => "デーモンサーバー",
        L10nKey::SettingsServerDesc => {
            "シェルを動かし続けているバックグラウンドサーバーを再起動します。このコンピュータ上のすべてのシェルが終了し、タブとレイアウトは新しいシェルで開き直します"
        }
        L10nKey::SettingsRestartServer => "サーバーを再起動…",
        L10nKey::SettingsAppHttpProxy => "アップデート用プロキシ",
        L10nKey::SettingsAppHttpProxyDesc => {
            "tty7 自身の更新チェックとダウンロードにのみ使用し、ペインで実行中のプログラムには影響しません。空欄ならシステムのプロキシに従います"
        }
        L10nKey::SettingsAppHttpProxyInvalid => {
            "プロキシアドレスとして正しくないため、この値は保存されませんでした"
        }
        L10nKey::SettingsAgentClaudeCode => "Claude Code",
        L10nKey::SettingsAgentCodex => "Codex",
        L10nKey::SettingsAgentCopilotCli => "Copilot CLI",
        L10nKey::SettingsAgentOpencode => "OpenCode",
        L10nKey::SettingsAgentPi => "Pi",
        L10nKey::SettingsAgentGrokBuild => "Grok Build",
        L10nKey::SettingsAgentOhMyPi => "Oh My Pi",
        L10nKey::SettingsAgentGemini => "Gemini",
        L10nKey::SettingsAgentDroid => "Droid",
        L10nKey::SettingsAgentQwenCode => "Qwen Code",
        L10nKey::SettingsAgentGoose => "Goose",
        L10nKey::SettingsAgentKimiCode => "Kimi Code",
        L10nKey::SettingsSearchAboutKeywords => {
            "バージョン ライセンス クレジット ビルド 更新 確認 github about version license credits update check"
        }
        L10nKey::SettingsSearchAppHttpProxyKeywords => {
            "プロキシ 通信 ネットワーク ダウンロード アップデート proxy http https socks socks5 clash v2ray network download update"
        }
        L10nKey::SettingsSearchAnsiColorsKeywords => {
            "パレット 16 ANSI カラー ターミナル テーマ ansi colors palette terminal theme colours"
        }
        L10nKey::SettingsSearchBackgroundImageKeywords => {
            "背景画像 壁紙 画像 写真 テーマ background image wallpaper picture theme"
        }
        L10nKey::SettingsSearchImageOpacityKeywords => {
            "背景画像 不透明度 透明度 強さ 壁紙 background image opacity strength fade"
        }
        L10nKey::SettingsSearchArgumentsKeywords => {
            "シェル フラグ ログイン 引数 arguments shell flags login args"
        }
        L10nKey::SettingsSearchBlurKeywords => {
            "透明度 半透明 すりガラス ウィンドウ 背景 blur transparency translucent frosted vibrancy window background"
        }
        L10nKey::SettingsSearchBoldFontKeywords => {
            "タイプフェイス 太字 ウェイト bold font typeface weight"
        }
        L10nKey::SettingsSearchClaudeCodeKeywords => {
            "エージェント 統合 フック インストール アンインストール 状態 セッション タブバー サイドバー バッジ claude agent integration hooks install status working waiting"
        }
        L10nKey::SettingsSearchCodexKeywords => {
            "エージェント 統合 フック インストール openai codex agent integration hooks install"
        }
        L10nKey::SettingsSearchCommandLineToolKeywords => {
            "cli tty7 パス シェル コマンド インストール シンボリックリンク ターミナル iterm エージェント スクリプト command line tool"
        }
        L10nKey::SettingsSearchCopilotCliKeywords => {
            "エージェント 統合 フック インストール github copilot agent integration hooks install"
        }
        L10nKey::SettingsSearchCopyOnSelectKeywords => {
            "クリップボード 選択 コピー マウス copy on select clipboard selection yank mouse"
        }
        L10nKey::SettingsSearchCursorBlinkKeywords => {
            "カーソル 点滅 フラッシュ cursor blink caret blinking flash"
        }
        L10nKey::SettingsSearchCursorShapeKeywords => {
            "カーソル 形状 ブロック バー アンダーライン ビーム cursor shape caret block bar underline beam"
        }
        L10nKey::SettingsSearchCustomThemesKeywords => {
            "テーマ 複製 編集 色 フォルダ 背景画像 壁紙 yaml インポート custom themes duplicate edit colors folder import background image wallpaper"
        }
        L10nKey::SettingsSearchDetectUrlsKeywords => {
            "リンク ハイパーリンク クリック可能 開く detect urls links hyperlink clickable open"
        }
        L10nKey::SettingsSearchDiffPreviewFromCountsKeywords => {
            "diff オーバーレイ プレビュー サイドバー カウント git 変更 クリック ブランチ 行数 diff preview overlay sidebar counts git changes"
        }
        L10nKey::SettingsSearchDimInactivePanesKeywords => {
            "非アクティブ ペイン 暗く フォーカス 分割 fade unfocused inactive split pane focus opacity highlight active dimming"
        }
        L10nKey::SettingsSearchFocusFollowsMouseKeywords => {
            "ペイン ホバー アクティブ focus follows mouse pane hover activate"
        }
        L10nKey::SettingsSearchFontFamilyKeywords => {
            "タイプフェイス 等幅 タイポグラフィ font family monospace typography typeface"
        }
        L10nKey::SettingsSearchFontLigaturesKeywords => {
            "タイポグラフィ グリフ fira font ligatures typography glyph fira"
        }
        L10nKey::SettingsSearchFontSizeKeywords => {
            "タイポグラフィ 文字 拡大 縮小 ズーム font size typography text bigger smaller zoom"
        }
        L10nKey::SettingsSearchForwardSshLoopbackLinksKeywords => {
            "ssh リモート ポート トンネル localhost フォワード リンク forward ssh loopback links tunnel"
        }
        L10nKey::SettingsSearchGrokBuildKeywords => {
            "エージェント 統合 フック インストール xai grok build agent integration hooks install"
        }
        L10nKey::SettingsSearchHideMouseWhileTypingKeywords => {
            "カーソル ポインタ 自動非表示 hide mouse while typing cursor pointer autohide"
        }
        L10nKey::SettingsSearchHistorySearchKeywords => {
            "ctrl-r 逆検索 ファジー検索 履歴 fzf プロンプト history search ctrl-r reverse fuzzy recall prompt"
        }
        L10nKey::SettingsSearchHostsKeywords => {
            "ssh ホスト 接続 保存 プロファイル インポート ssh_config 管理 追加 編集 クイック接続 hosts ssh profile import connect manage"
        }
        L10nKey::SettingsSearchItalicFontKeywords => "タイプフェイス 斜体 italic oblique typeface",
        L10nKey::SettingsSearchKeybindingsKeywords => {
            "ショートカット ホットキー キーボード バインディング コード tmux プリセット 再バインド プレフィックス keybindings shortcut hotkey binding chord prefix"
        }
        L10nKey::SettingsSearchKeybindingsTitle => "キーバインド",
        L10nKey::SettingsSearchLineHeightKeywords => {
            "タイポグラフィ リーディング 行間 line height typography leading spacing"
        }
        L10nKey::SettingsSearchUiFontFamilyKeywords => {
            "インターフェース フォント ファミリー UI 書体 タブ サイドバー interface font family ui typeface"
        }
        L10nKey::SettingsSearchNewTabPositionKeywords => {
            "タブ 順序 末尾 現在のタブの隣 new tab position tabs order end after current"
        }
        L10nKey::SettingsSearchNotifyOnCommandFinishKeywords => {
            "通知 アラート 完了 osc デスクトップ バナー 長い コマンド notify on command finish notification alert desktop"
        }
        L10nKey::SettingsSearchNotifyThresholdKeywords => {
            "通知 アラート 秒 時間 長い コマンド 遅延 notify threshold notification alert seconds duration delay"
        }
        L10nKey::SettingsSearchOpacityKeywords => {
            "透明度 半透明 透ける ウィンドウ alpha opacity transparency translucent window"
        }
        L10nKey::SettingsSearchOpenFilesWithKeywords => {
            "リンク ファイル エディタ コマンド 外部アプリ パス 行 列 open files with editor external app path line column"
        }
        L10nKey::SettingsSearchOpencodeKeywords => {
            "エージェント 統合 プラグイン インストール opencode agent integration plugin install"
        }
        L10nKey::SettingsSearchOptionAsMetaKeywords => {
            "alt キーボード 修飾キー エスケープ macos option meta option acts as meta keyboard modifier"
        }
        L10nKey::SettingsSearchOhMyPiKeywords => {
            "エージェント 統合 拡張 インストール omp oh my pi agent integration extension install"
        }
        L10nKey::SettingsSearchGeminiKeywords => {
            "エージェント 統合 フック インストール gemini google agent integration hooks install"
        }
        L10nKey::SettingsSearchDroidKeywords => {
            "エージェント 統合 フック インストール droid factory agent integration hooks install"
        }
        L10nKey::SettingsSearchQwenCodeKeywords => {
            "エージェント 統合 フック インストール qwen code agent integration hooks install"
        }
        L10nKey::SettingsSearchGooseKeywords => {
            "エージェント 統合 フック プラグイン インストール goose agent integration hooks plugin install"
        }
        L10nKey::SettingsSearchKimiCodeKeywords => {
            "エージェント 統合 フック インストール kimi code moonshot agent integration hooks install"
        }
        L10nKey::SettingsSearchPiKeywords => {
            "エージェント 統合 拡張 インストール pi agent integration extension install"
        }
        L10nKey::SettingsSearchPortForwardingKeywords => {
            "ssh トンネル ローカル リモート ダイナミック socks フォワード ルール port forwarding ssh tunnel local remote dynamic forward rule"
        }
        L10nKey::SettingsSearchProgramKeywords => {
            "シェル バイナリ zsh bash fish nu nushell pwsh powershell 実行可能 起動 program shell binary executable launch"
        }
        L10nKey::SettingsSearchRememberWindowSizeKeywords => {
            "ウィンドウ サイズ 位置 境界 ジオメトリ 起動 記憶 remember window size position bounds geometry launch startup"
        }
        L10nKey::SettingsSearchReportMouseToAppsKeywords => {
            "マウス レポート vim tmux クリック スクロール shift パススルー report mouse to apps vim tmux passthrough"
        }
        L10nKey::SettingsSearchRestoreLastLayoutKeywords => {
            "復元 セッション 前回 タブ 分割 開き直し 起動 レイアウト restore last layout session previous tabs splits reopen launch"
        }
        L10nKey::SettingsSearchScrollSpeedKeywords => {
            "マウス ホイール 倍率 スクロール scroll speed mouse wheel multiplier scrolling"
        }
        L10nKey::SettingsSearchSmoothScrollKeywords => {
            "スムーズ スクロール アニメーション ホイール トラックパッド smooth animation ease wheel trackpad"
        }
        L10nKey::SettingsSearchUpdateChannelKeywords => {
            "更新 チャンネル 安定版 ナイトリー リリース update channel stable nightly release"
        }
        L10nKey::SettingsSearchCheckUpdatesOnLaunchKeywords => {
            "起動時 更新 確認 自動 update check launch startup automatic"
        }
        L10nKey::SettingsSearchAutoDownloadKeywords => {
            "バックグラウンド ダウンロード 更新 インストール 通信量 update download background install metered"
        }
        L10nKey::SettingsSearchScrollbackKeywords => {
            "履歴 バッファ 行数 スクロール scrollback history buffer lines scroll"
        }
        L10nKey::SettingsSearchShowTrayIconKeywords => {
            "トレイ メニューバー ステータス アイコン エージェント 通知 システム tray icon menu bar status system attention"
        }
        L10nKey::SettingsSearchSidebarGroupingKeywords => {
            "タブ グループ リポジトリ git スクラッチ ヘッダー サイドバー フラット フォルダ ディレクトリ sidebar grouping tabs repo repository git scratch header flat folder directory"
        }
        L10nKey::SettingsSearchSmartSelectionKeywords => {
            "ダブルクリック 単語 url パス 選択 セマンティック 括弧 メール smart selection double click word url path bracket email"
        }
        L10nKey::SettingsSearchStartInKeywords => {
            "cwd 作業ディレクトリ 起動 フォルダ パス ホーム 継承 カスタム start in working directory home inherit custom"
        }
        L10nKey::SettingsSearchSyncWithSystemKeywords => {
            "テーマ ダーク ライト 自動 os 外観 モード sync with system theme dark light auto follow appearance"
        }
        L10nKey::SettingsSearchLegiblePaletteKeywords => {
            "可読 コントラスト 明色 パレット パラメーター 修正 legible bright contrast palette parameter"
        }
        L10nKey::SettingsSearchPromptEditorKeywords => {
            "プロンプト エディター ネイティブ シェル 入力 行編集 キーバインド 貼り付け prompt editor native shell input zle readline"
        }
        L10nKey::SettingsSearchTabBarPositionKeywords => {
            "タブ 垂直 サイドバー 左 上 レイアウト レール tab bar position tabs vertical sidebar left top rail"
        }
        L10nKey::SettingsSearchTabCompletionKeywords => {
            "補完 メニュー サジェスト タブ プロンプト tab completion menu suggestions prompt"
        }
        L10nKey::SettingsSearchTerminalBellKeywords => {
            "ベル 可聴 視覚 フラッシュ サウンド サイレント ビープ 両方 ^g terminal bell audible visual flash sound silence beep both"
        }
        L10nKey::SettingsSearchThemeKeywords => {
            "外観 色 配色 ダーク ライト パレット 背景 前景 アクセント 同期 システム os 自動 theme appearance color scheme palette background foreground accent sync auto"
        }
        L10nKey::SettingsSearchTrimTrailingSpacesKeywords => {
            "クリップボード 空白 コピー trim trailing spaces copy whitespace clipboard"
        }
        L10nKey::SettingsSearchVerifyHostKeysKeywords => {
            "ssh セキュリティ known_hosts フィンガープリント mitm ホストキー 検証 verify host keys fingerprint known_hosts"
        }
        L10nKey::SettingsSearchWarnBeforeClosingKeywords => {
            "ssh 確認 閉じる タブ ペイン ライブ セッション セキュリティ warn before closing ssh confirm tab pane live session"
        }
        L10nKey::SettingsSearchStartupWindowKeywords => {
            "起動 開く 最大化 全画面 通常 startup window launch maximized fullscreen normal"
        }
        L10nKey::SwitcherNoMatch => "一致するワークスペースまたはマシンがありません",
        L10nKey::AddSshHost => "SSH ホストを追加…",
        L10nKey::ClickForNewWindow => "クリックで新しいウィンドウを開く",
        L10nKey::RestartServer => "サーバーを再起動",
        L10nKey::OtherMachines => "その他のマシン",
        L10nKey::Ok => "OK",
        L10nKey::SftpNoTransfers => "転送はまだありません",
        L10nKey::SftpPanelTitleFiles => "ファイル",
        L10nKey::SftpTooltipRefresh => "更新",
        L10nKey::SftpTooltipMore => "その他",
        L10nKey::SftpMenuNewFolder => "新しいフォルダ",
        L10nKey::SftpMenuNewFile => "新しいファイル",
        L10nKey::SftpMenuUpload => "アップロード…",
        L10nKey::SftpMenuGotoShellCwd => "シェルの作業ディレクトリへ移動",
        L10nKey::SftpMenuHideTransferHistory => "転送履歴を非表示",
        L10nKey::SftpMenuTransferHistory => "転送履歴",
        L10nKey::SftpEditNewFolder => "新しいフォルダ",
        L10nKey::SftpEditNewFile => "新しいファイル",
        L10nKey::SftpEditRename => "名前を変更",
        L10nKey::SftpEditPermissions => "権限 · {mode}",
        L10nKey::SftpLoading => "読み込み中…",
        L10nKey::SftpEmptyDirectory => "空のディレクトリです",
        L10nKey::SftpContextOpen => "開く",
        L10nKey::SftpContextEdit => "編集",
        L10nKey::SftpContextFollowSymlink => "シンボリックリンクを辿る",
        L10nKey::SftpContextRename => "名前を変更",
        L10nKey::SftpContextChmod => "chmod…",
        L10nKey::SftpTransferSummaryRunning => "{count} 件転送中 · {pct}%",
        L10nKey::SftpTransferSummaryFailed => "{count} 件失敗",
        L10nKey::SftpTransferSummaryIdle => "転送",
        L10nKey::SftpTransferProgress => "{done} / {total} ({pct}%)",
        L10nKey::SftpTransferDone => "完了",
        L10nKey::SftpTransferCancelled => "キャンセル済み",
        L10nKey::SftpTransferError => "エラー",
        L10nKey::SftpTransferListFailed => "転送状況を取得できませんでした: {error}",
        L10nKey::SftpImagePasteUploadFailed => {
            "貼り付けた画像を {host} にアップロードできませんでした: {error}"
        }
        L10nKey::LinkFileOpenFailed => "{path} を開けませんでした: {error}",
        L10nKey::ForwardPanelTitle => "ポートフォワード",
        L10nKey::ForwardDisconnected => "切断済み",
        L10nKey::ForwardDisconnectedFrom => "{host} から切断されました",
        L10nKey::SshEditProfile => "接続を編集…",
        L10nKey::ForwardTooltipAdd => "フォワードを追加",
        L10nKey::ForwardTooltipRemove => "削除",
        L10nKey::ForwardLocal => "ローカル",
        L10nKey::ForwardRemote => "リモート",
        L10nKey::ForwardDynamic => "ダイナミック",
        L10nKey::ForwardBindLabel => "バインド",
        L10nKey::ForwardToLabel => "転送先",
        L10nKey::ForwardSocksLabel => "SOCKS",
        L10nKey::ForwardAdd => "追加",
        L10nKey::ForwardRequestFailed => "セッションに届きませんでした。何も変更していません",
        L10nKey::FileTreePlaceholderFileName => "ファイル名",
        L10nKey::FileTreePlaceholderFolderName => "フォルダ名",
        L10nKey::FileTreePlaceholderNewName => "新しい名前",
        L10nKey::FileTreeDeleteTitle => "「{name}」を削除しますか？",
        L10nKey::FileTreeDeleteFolderBody => "フォルダとその中のすべての項目が削除されます",
        L10nKey::FileTreeDeleteFileBody => "ファイルが削除されます",
        L10nKey::SftpDeleteFolderBody => {
            "{host} 上でフォルダとその中身がすべて削除されます。リモート側にゴミ箱はありません。"
        }
        L10nKey::SftpDeleteFileBody => {
            "{host} 上でファイルが削除されます。リモート側にゴミ箱はありません。"
        }
        L10nKey::FileTreeDeleteFailed => "{name} を削除できませんでした",
        L10nKey::FileTreeCreateFailed => "{name} を作成できませんでした",
        L10nKey::FileTreeRenameFailed => "{name} の名前を変更できませんでした",
        L10nKey::FileTreeContextOpen => "開く",
        L10nKey::FileTreeContextCdHere => "ここで cd",
        L10nKey::FileTreeContextInsertPath => "ターミナルにパスを挿入",
        L10nKey::FileTreeContextAttachAgent => "エージェントをアタッチ",
        L10nKey::FileTreeContextNewFile => "新しいファイル",
        L10nKey::FileTreeContextNewFolder => "新しいフォルダ",
        L10nKey::FileTreeContextRename => "名前を変更",
        L10nKey::FileTreeContextCopyPath => "パスをコピー",
        L10nKey::FileTreeContextHideDotfiles => "ドットファイルを非表示",
        L10nKey::FileTreeContextShowDotfiles => "ドットファイルを表示",
        L10nKey::FileDropIntoItself => "フォルダを自分自身の中にはコピーできません",
        L10nKey::FileDropNotHere => "このマシンにはありません",
        L10nKey::FileDropNameTaken => "同じドロップ内の別の項目がすでにこの名前を使っています",
        L10nKey::FileDropTooDeep => "フォルダの入れ子が {n} 階層を超えています",
        L10nKey::FileDropTooLarge => "{limit} MB を超えています。SFTP で転送してください",
        L10nKey::FileDropNoWorkingName => "隣に空いている一時的な名前がなく、先にコピーできません",
        L10nKey::FileDropLeftAside => {
            "新しいコピーを所定の位置に移せませんでした。元のものは同じフォルダの「{name}」になっています"
        }
        L10nKey::FileDropReplaceTitle => "「{name}」を置き換えますか？",
        L10nKey::FileDropReplaceManyTitle => "{n} 項目を置き換えますか？",
        L10nKey::FileDropReplaceBody => {
            "このフォルダには同じ名前のものがすでにあります。置き換えると元に戻せません"
        }
        L10nKey::FileDropReplace => "置き換える",
        L10nKey::FileDropFailed => "{name} をコピーできませんでした",
        L10nKey::FileDropFailedMany => "{name} をコピーできませんでした。他に {n} 件も失敗しました",
        L10nKey::SshPromptNewKey => "新しいキー {fingerprint}",
        L10nKey::SshPromptOldKey => "以前のキー {old_fingerprint}",
        L10nKey::SshPromptHostKeyNewAlgorithm => {
            "このホストはすでに {previous_algorithm} キーで登録されています。これはそれを置き換えるものではなく、新しい {algorithm} キーです"
        }
        L10nKey::SshPromptTypeYesToOverride => "「yes」を入力すると「上書き」が有効になります",
        L10nKey::EditorCantOpen => "{path} を開けません: {e}",
        L10nKey::EditorCantRead => "{path} を読み取れません: {e}",
        L10nKey::EditorNotUtf8 => "「{path}」は有効な UTF-8 ではありません",
        L10nKey::EditorSaveFailed => "{name} を保存できませんでした",
        L10nKey::EditorUnsavedChanges => "「{name}」には保存されていない変更があります",
        L10nKey::EditorDiscard => "破棄",
        L10nKey::EditorNoFileOpen => "開かれているファイルはありません",
        L10nKey::EditorBackToTerminal => "ターミナルに戻る (Esc)",
        L10nKey::EditorLnCol => "行 {line}, 列 {column}",
        L10nKey::EditorEdit => "編集",
        L10nKey::EditorPreview => "プレビュー",
        L10nKey::EditorWrapOn => "折り返し: オン",
        L10nKey::EditorWrapOff => "折り返し: オフ",
        L10nKey::EditorFileTooLarge => "「{path}」はエディタで開くには大きすぎます（{size} MB）",
        L10nKey::EditorBinaryFile => "「{path}」はバイナリファイルのようです",
        L10nKey::PanelInfoTitle => "情報",
        L10nKey::PanelChangesTitle => "ソース管理",
        L10nKey::PanelScmTitle => "ソース管理",
        L10nKey::PanelFilesTitle => "ファイル",
        L10nKey::PanelNoSession => "アクティブなセッションがありません",
        L10nKey::PanelNoSessionHint => {
            "タブを開くと、そのシェル、ディレクトリ、プロセスがここに表示されます"
        }
        L10nKey::PanelNoWorkingDirectory => "作業ディレクトリがありません",
        L10nKey::PanelNoWorkingDirectoryHint => {
            "このペインはまだ作業ディレクトリを報告していません"
        }
        L10nKey::PanelLoading => "読み込み中…",
        L10nKey::PanelNotAGitRepo => "git リポジトリではありません",
        L10nKey::PanelNotAGitRepoHint => {
            "git リポジトリ内に移動すると、このタブに未コミットの変更が一覧表示されます"
        }
        L10nKey::PanelNoChanges => "未コミットの変更はありません",
        L10nKey::PanelNoChangesHint => "ワーキングツリーはクリーンです",
        L10nKey::PanelSessionSubtitle => "セッション",
        L10nKey::PanelConversationSubtitle => "会話",
        L10nKey::PanelTurnAltScreenNow => {
            "全画面プログラムがこのペインを占有している間は、戻る先がありません"
        }
        L10nKey::PanelTurnNoScrollback => {
            "このターンは代替画面に描かれたため、スクロールバックに残っていません"
        }
        L10nKey::PanelProcessesSubtitle => "プロセス",
        L10nKey::PanelPortsSubtitle => "ポート",
        L10nKey::PanelCwd => "作業ディレクトリ",
        L10nKey::PanelShell => "シェル",
        L10nKey::PanelSsh => "ssh",
        L10nKey::PanelBranch => "ブランチ",
        L10nKey::PanelChangesRow => "変更",
        L10nKey::PanelAgentWorking => "作業中",
        L10nKey::PanelAgentWaiting => "待機中",
        L10nKey::PanelAgentDone => "完了",
        L10nKey::PanelRevealInFinder => "Finder で表示",
        L10nKey::PanelOpenFolder => "フォルダを開く",
        L10nKey::PanelOpenInBrowser => "ブラウザで開く",
        L10nKey::ScmGroupMerge => "マージの競合",
        L10nKey::ScmGroupStaged => "ステージされた変更",
        L10nKey::ScmGroupChanges => "変更",
        L10nKey::ScmGroupUntracked => "未追跡",
        L10nKey::ScmCommitPlaceholder => "何を変えたか書いてみましょう…",
        L10nKey::ScmCommitButton => "コミット",
        L10nKey::ScmCommitAllButton => "すべてコミット",
        L10nKey::ScmCommitAmendButton => "コミット（修正）",
        L10nKey::ScmCommitAndPush => "コミットしてプッシュ",
        L10nKey::ScmCommitAndSync => "コミットして同期",
        L10nKey::ScmAmendLastCommit => "直前のコミットを修正",
        L10nKey::ScmCommitStaged => "ステージ済みをコミット",
        L10nKey::ScmStashAll => "すべてスタッシュ",
        L10nKey::ScmNothingToCommit => "コミットするものがありません",
        L10nKey::ScmNetworkBusy => "このリポジトリでは別のネットワーク操作が実行中です",
        L10nKey::ScmCommitNeedsMessage => "先にコミットメッセージを入力してください",
        L10nKey::ScmDetailFilesFailed => "ファイル一覧を読み込めませんでした",
        L10nKey::ScmTimeNow => "今",
        L10nKey::ScmTimeMinutes => "{n}分",
        // 「{n}時」は時刻に読めるので「時間」のまま。
        L10nKey::ScmTimeHours => "{n}時間",
        L10nKey::ScmTimeDays => "{n}日",
        L10nKey::ScmTimeMonths => "{n}か月",
        L10nKey::ScmTimeYears => "{n}年",
        L10nKey::ScmResetHardConfirm => {
            "ブランチをこのコミットへリセットしますか?それ以降のコミットはブランチから外れ、\
             未コミットの変更は破棄されます。"
        }
        L10nKey::ScmReset => "リセット",
        L10nKey::ScmChipStaged => "ステージ済み",
        L10nKey::ScmStage => "変更をステージ",
        L10nKey::ScmStageAll => "すべての変更をステージ",
        L10nKey::ScmUnstage => "ステージを取り消す",
        L10nKey::ScmUnstageAll => "すべてのステージを取り消す",
        L10nKey::ScmDiscard => "変更を破棄",
        L10nKey::ScmDiscardAll => "すべての変更を破棄",
        L10nKey::ScmDiscardConfirm => "{path} の変更を破棄しますか？元に戻せません。",
        L10nKey::ScmOpenConflict => "競合を解決",
        L10nKey::ScmMarkResolved => "解決済みにする",
        L10nKey::ScmUnrepresentablePath => {
            "このパスは正しい UTF-8 ではないため git に渡せません — 閲覧のみです。"
        }
        L10nKey::ScmPublishBranch => "ブランチを公開",
        L10nKey::ScmDetached => "デタッチ",
        L10nKey::ScmPushDetached => {
            "HEAD がデタッチされています — ブランチをチェックアウトしてからプッシュしてください"
        }
        L10nKey::ScmPushNoCommits => "プッシュするコミットがまだありません",
        L10nKey::ScmAmendBadge => "修正",
        L10nKey::ScmSync => "変更を同期",
        L10nKey::ScmPush => "プッシュ",
        L10nKey::ScmPull => "プル",
        L10nKey::ScmFetch => "フェッチ",
        L10nKey::ScmCheckoutBranch => "チェックアウト…",
        L10nKey::ScmCreateBranch => "ブランチを作成…",
        L10nKey::ScmSearchBranches => "ブランチを検索…",
        L10nKey::ScmStashAndSwitch => "スタッシュして切り替え",
        L10nKey::ScmGraphTitle => "履歴",
        L10nKey::ScmGraphLoadMore => "さらに読み込む",
        L10nKey::ScmGraphFilterPlaceholder => "コミットを絞り込む…",
        L10nKey::ScmGraphAllBranches => "すべてのブランチ",
        L10nKey::ScmGraphEmpty => "まだコミットがありません",
        L10nKey::ScmGraphCurrentBranch => "現在のブランチ",
        L10nKey::ScmCheckoutCommit => "このコミットをチェックアウト",
        L10nKey::ScmCreateBranchHere => "ここにブランチを作成…",
        L10nKey::ScmResetSoft => "リセット（ソフト）",
        L10nKey::ScmResetMixed => "リセット（ミックス）",
        L10nKey::ScmResetHard => "リセット（ハード）",
        L10nKey::ScmCommitDetailTitle => "コミット",
        L10nKey::ScmCopyCommitSha => "コミット SHA をコピー",
        L10nKey::ScmCherryPick => "チェリーピック",
        L10nKey::ScmRevertCommit => "コミットを取り消す",
        L10nKey::ScmResetToCommit => "このコミットにリセット",
        L10nKey::ScmRefresh => "更新",
        L10nKey::ScmBackToChanges => "戻る",
        L10nKey::ScmCommitParents => "親コミット",
        L10nKey::ScmShowMore => "続きを表示",
        L10nKey::ScmShowLess => "折りたたむ",
        L10nKey::ScmCommitNotFound => "このリポジトリにそのコミットはありません。",
        L10nKey::ScmTooManyChanges => {
            "変更が多いため、{total} 件のうち先頭 {shown} 件のみ表示しています。"
        }
        L10nKey::ScmOpenChanges => "変更を開く",
        L10nKey::ScmDiscardAllConfirm => {
            "未ステージの変更と未追跡ファイルをすべて破棄しますか？ステージ済みの変更は残ります。元に戻せません。"
        }
        L10nKey::ScmAmendConfirm => {
            "直前のコミットを修正しますか？新しいコミットに置き換わるため、すでに取得した人は対応が必要になります。"
        }
        L10nKey::ScmOpMerge => "マージ中",
        L10nKey::ScmOpRebase => "リベース中",
        L10nKey::ScmOpCherryPick => "チェリーピック中",
        L10nKey::ScmOpRevert => "リバート中",
        L10nKey::ScmOpBisect => "二分探索中",
        L10nKey::ScmOpAm => "パッチ適用中",
        L10nKey::ScmSwitchRepository => "リポジトリを切り替え",
        L10nKey::WindowStop => "停止",
        L10nKey::WindowDelete => "削除",
        L10nKey::WindowThisWorkspace => "このワークスペース",
        L10nKey::WindowConfirmTitle => "ワークスペース「{name}」を{verb}しますか？",
        L10nKey::WindowStopUnreachable => {
            "そのマシンに到達できませんでした。シェルを停止する前に再接続してください"
        }
        L10nKey::WindowDeleteUnreachable => {
            "そのマシンに到達できませんでした。削除する前に再接続してください。マシンが確認するまでブックマークは保持されます"
        }
        L10nKey::WindowReconnectToStop => {
            "停止または削除する前に、このワークスペースに再接続してください"
        }
        L10nKey::WindowOperationIncomplete => "ワークスペースの操作が完了しませんでした",
        L10nKey::WindowStopShells => "{count} 個の実行中シェルが終了します",
        L10nKey::WindowDeleteShells => "{count} 個の実行中シェルが終了し、レイアウトが消去されます",
        L10nKey::DiffReading => "Diff を読み込み中…",
        L10nKey::DiffNotARepo => "git リポジトリではありません",
        L10nKey::DiffReadFailed => {
            "ワーキングツリーの Diff を読み込めませんでした — 次の更新で再試行します"
        }
        L10nKey::DiffWorkingTreeClean => "ワーキングツリーはクリーンです",
        L10nKey::DiffCloseTooltip => "Diff を閉じる (Esc)",
        L10nKey::DiffChangedFiles => "変更されたファイル {count} 個",
        L10nKey::DiffUntrackedCount => " · 未追跡 {count} 件",
        L10nKey::DiffMoreFiles => {
            "… さらに変更されたファイル {count} 個 — ターミナルで `git diff` を実行して確認してください"
        }
        L10nKey::DiffOversizedNotice => {
            "このワーキングツリーは大きすぎて描画できません（{summary}）。すべて折りたたんであります — 個別に展開するか、ターミナルで `git diff` を実行してください"
        }
        L10nKey::DiffTruncatedPerFile => {
            "Diff は {limit} 行で切り詰められました — 残りはターミナルで `git diff` を実行してください"
        }
        L10nKey::DiffTruncatedBudget => {
            "内容は読み込まれていません — tty7 の Diff 予算を超えています。ターミナルで `git diff` を実行してください"
        }
        L10nKey::DiffUntrackedHeader => "未追跡ファイル ({count})",
        L10nKey::DiffMoreUntracked => {
            "… さらに {count} 個 — ターミナルで `git status` を実行して確認してください"
        }
        L10nKey::DiffLines => "{count} 行の Diff",
        L10nKey::DiffChangedLines => {
            "変更行 {total} 件、上限 {cap} までに読み込んだ Diff 行 {loaded} 件"
        }
        L10nKey::DiffBudgetAndCap => "tty7 の予算とファイルごとの上限",
        L10nKey::DiffBudget => "tty7 の予算",
        L10nKey::DiffPerFileCap => "ファイルごとの上限",
        L10nKey::DiffUntrackedSummary => "未追跡 {count}",
        L10nKey::DiffViewSplit => "左右分割",
        L10nKey::DiffViewUnified => "統合",
        L10nKey::PendingConnecting => "{machine} に接続中…",
        L10nKey::PendingUnreachable => "{machine} に到達できませんでした",
        L10nKey::WorktreePromptNeedsName => "ワークツリーには名前が必要です",
        L10nKey::WorktreePromptTitle => "新しいワークツリータブ",
        L10nKey::WorktreePromptName => "ワークツリー名",
        L10nKey::WorktreePromptBranch => "新しいブランチ",
        L10nKey::WorktreePromptBase => "開始地点",
        L10nKey::WorktreePromptCreating => "作成中…",
        L10nKey::WorktreePromptCreate => "作成",
        L10nKey::AppNewWorktreeFailed => "新しいワークツリーを作成できませんでした: {error}",
        L10nKey::HomeTimeJustNow => "たった今",
        L10nKey::HomeTimeMinutesAgo => "{count} 分前",
        L10nKey::HomeTimeHourAgo => "1 時間前",
        L10nKey::HomeTimeHoursAgo => "{count} 時間前",
        L10nKey::HomeTimeYesterday => "昨日",
        L10nKey::HomeTimeDaysAgo => "{count} 日前",
        L10nKey::HomeTimeWeeksAgo => "{count} 週間前",
        L10nKey::HomeTimeMonthsAgo => "{count} か月前",
        L10nKey::HomeTimeOverYearAgo => "1 年以上前",
        L10nKey::HomeReopenNamed => "「{name}」をもう一度開く",
        L10nKey::RemoteStripDisconnected => "{machine} に未接続です",
        L10nKey::RemoteStripConnecting => "{machine} に接続中…",
        L10nKey::RemoteStripReconnecting => "{machine} に再接続中…",
        L10nKey::RemoteStripReconnectingAttempt => "{machine} に再接続中…（{count} 回目の試行）",
        L10nKey::RemoteStripReconnectingWhy => "{machine} に再接続中…（前回の失敗: {error}）",
        L10nKey::RemoteStripReconnectingAttemptWhy => {
            "{machine} に再接続中…（{count} 回目の試行、前回の失敗: {error}）"
        }
        L10nKey::RemoteStripPreempted => "このワークスペースは {by} で開かれました",
        L10nKey::RemoteStripFailed => "{machine} に未接続です — {error}",
        L10nKey::RemoteStripRouteLost => "{machine} の接続設定は存在しません — 再接続できません",
        L10nKey::RemoteRouteParkedHint => {
            "接続設定が存在しないため、自動再接続しません。リモートのセッションは残っています — 新しいプロファイルで接続すると、ワークスペース一覧に戻ります。"
        }
        L10nKey::RemoteNoticePreempted => "別の場所で開かれました — 入力しても反映されません",
        L10nKey::RemoteNoticeDisconnected => "未接続です — 入力しても反映されません",
        L10nKey::RemoteActionRetryNow => "今すぐ再試行",
        L10nKey::RemoteActionTakeBack => "取り戻す",
        L10nKey::RemoteActionConnect => "接続",
        L10nKey::RemoteActionRetry => "再試行",
        L10nKey::RemoteActionRemoveEntry => "エントリを削除",
        L10nKey::RemoteNoConnectionDetails => {
            "このウィンドウは {machine} 上のワークスペースですが、tty7 に接続情報がありません。SSH プロファイルか ~/.ssh/config の項目が残っているか確認してください"
        }
        L10nKey::RemoteThisComputer => "このコンピュータ",
        L10nKey::RemoteProfileGone => "削除されたプロファイル",
        L10nKey::RemoteRestartTitle => "「{machine}」上の tty7 サーバーを再起動しますか？",
        L10nKey::RemoteRestartBody => {
            "{machine} のサーバーに端末がなく、安全な保守に対応している場合だけ再起動します。セッション実行中、端末作成中、または未対応の旧版の場合は、セッションを終了せず再起動を延期します"
        }
        L10nKey::RemoteReplaceBody => {
            "tty7 は {machine} に対応するサーバーを準備します。元のサーバーが端末を持たず、安全に停止できると確認した場合だけ切り替えます。セッション実行中や未対応の旧版では更新を延期し、強制終了しません"
        }
        L10nKey::RemoteRestartFailedTitle => {
            "「{machine}」上の tty7 サーバーは再起動されませんでした"
        }
        L10nKey::RemoteRestartFailedBody => {
            "{error}\n\nそこで実行中のセッションは古いビルドのままです。古いサーバーはセッションが終わっても自動では終了しないため、古いサーバー プロセス自体が終了してから再接続すると、このビルドのサーバーが起動します"
        }
        L10nKey::RemoteLegacyStopTitle => "「{machine}」の古いサーバーは停止するしかありません",
        L10nKey::RemoteLegacyStopBody => {
            "{machine} で動いているサーバーは古く、「アイドル時に再起動」の要求を理解できません。退かすには直接停止するしかありません。\n\n{versions}続行すると古いサーバーを停止し、そこで動いているすべてのセッションが切断されます。{machine} 上のデータには触れません。\n「{keep}」を選ぶと現状のまま、今回は何も変更しません。"
        }
        L10nKey::RemoteLegacyStopVersions => "稼働中: {running}\nインストール予定: {wanted}\n\n",
        L10nKey::RemoteLegacyStopConfirm => "セッションを切断して{action}",
        L10nKey::RemoteLegacyStopKeep => "旧サーバーを保持",
        L10nKey::RemoteHostUnreachable => "{machine} に到達できませんでした: {error}",
        L10nKey::RemoteInstallTitle => "「{machine}」に tty7 サーバーをインストールしますか？",
        L10nKey::RemoteInstallDetail => {
            "tty7 はサーバーバイナリを {machine} に書き込み、{machine} でワークスペースをホストできるようにします。{machine} 上の他のものには触れず、sudo も使いません。\n\n{path_label}\u{2003}{path}\n{version_label}\u{2003}{version}\n{size_label}\u{2003}{size}\n{from_label}\u{2003}{from}\n{sha_label}\u{2003}{sha256}\n\n{silent_upgrades}"
        }
        L10nKey::RemoteInstallPathLabel => "パス",
        L10nKey::RemoteInstallVersionLabel => "バージョン",
        L10nKey::RemoteInstallSizeLabel => "サイズ",
        L10nKey::RemoteInstallFromLabel => "取得元",
        L10nKey::RemoteInstallShaLabel => "SHA-256",
        L10nKey::RemoteInstallSilentUpgrades => {
            "このマシンでの今後のアップグレードはサイレントにインストールされます"
        }
        L10nKey::RemoteInstallBytes => "バイト",
        L10nKey::RemoteMismatchTitle => "「{machine}」上の tty7 サーバーを更新しますか？",
        L10nKey::RemoteMismatchDetail => {
            "{machine} はサーバー {running} で動いていますが、このクライアント（{wanted}）はそのプロトコルを話せません。対応するサーバーは準備済みで、既存セッションは元のサーバー上にあります。\n\n{replace_server}\u{2003}端末がなく安全な保守に対応している場合だけ {wanted} に切り替えます。それ以外はセッションを保持して延期します。\n{cancel}\u{2003}{machine} はそのままです。このウィンドウは接続しません"
        }
        L10nKey::RemoteMismatchReplaceServer => "サーバーを更新",
        L10nKey::RemoteMismatchDowngradeServer => "サーバーを置き換え",
        L10nKey::RemoteMismatchUnknownBuild => "不明なビルド",
        L10nKey::RemoteMismatchUnknownBuildFromExe => "不明なビルド（{exe} から）",
        L10nKey::RemoteServerOutdated => {
            "{machine} の tty7 サーバーが古く（{build}）、この tty7 からは通信できません。更新すると接続できます"
        }
        L10nKey::RemoteServerTooNew => {
            "{machine} の tty7 サーバー（{build}）は、この tty7 より新しいバージョンです。このコンピューターの tty7 を更新するか、向こうのサーバーを対応するものに置き換えてください"
        }
        L10nKey::RemoteDaemonStartFailed => {
            "tty7 のローカルサーバーを起動できませんでした: {error}"
        }
        L10nKey::RemoteDaemonUnreachable => {
            "tty7 のローカルサーバーに到達できませんでした: {error}"
        }
        L10nKey::RemoteDaemonTooOld => {
            "このマシンのデーモンは古いビルドのため、{machine} 上のサーバーを再起動できません。tty7 を終了（デーモンも停止します）して開き直し、再試行してください"
        }
        L10nKey::RemoteProfileMissing => "その保存済み SSH プロファイルはもう存在しません",
        L10nKey::RemoteAliasMissing => "`{alias}` は ~/.ssh/config にありません",
        L10nKey::RemoteWslNoSsh => "WSL ワークスペースには SSH 接続がありません",
        L10nKey::RemoteLocalStdioNoSsh => {
            "ローカルの --stdio ワークスペースには SSH 接続がありません"
        }
        L10nKey::RemoteHostNotTty7 => {
            "{machine} は応答しましたが、tty7 サーバーとしては応答しませんでした: {error}"
        }
        L10nKey::RemoteWorkspaceListFailed => {
            "{machine} に接続しましたが、ワークスペースの一覧を取得できませんでした: {error}"
        }
        L10nKey::RemoteServerRestartFailed => {
            "{machine} 上の tty7 サーバーを再起動できませんでした: {error}"
        }
        L10nKey::RemoteNoRouteToHost => "tty7 は {machine} に到達する手段を失いました",
        L10nKey::RemoteMachineTreeUnexpectedReply => {
            "サーバーがマシンツリーに {reply} で応答しました"
        }
        L10nKey::RemoteMismatchVersionFromExe => "{version}（{exe} から）",
        L10nKey::AppNoRunningCodingAgent => {
            "実行中のコーディングエージェントが見つかりません — 先にペインでコーディングエージェントを起動してください（claude、codex など）"
        }
        L10nKey::SwitcherThisComputer => "このコンピュータ",
        L10nKey::SwitcherRestartingServer => "tty7 のサーバーを再起動中…",
        L10nKey::SwitcherDownloadingServerWithTotal => {
            "tty7 のサーバーをダウンロード中… {done} / {total}"
        }
        L10nKey::SwitcherDownloadingServerNoTotal => "tty7 のサーバーをダウンロード中… {done}",
        L10nKey::SwitcherCopyingServer => "tty7 のサーバーをコピー中… {done} / {total}",
        L10nKey::SwitcherThisWindow => "このウィンドウ",
        L10nKey::SwitcherOpen => "開く",
        L10nKey::SwitcherDisconnect => "切断",
        L10nKey::SwitcherEditHost => "ホストを編集…",
        L10nKey::SwitcherSaveAsHost => "SSH ホストとして保存…",
        L10nKey::SshSaveDroppedJumpHost => {
            "踏み台ホストは引き継がれません — 保存済みホストの踏み台は別の保存済みホストである必要があります"
        }
        L10nKey::SwitcherOpenInNewWindow => "新しいウィンドウで開く",
        L10nKey::SwitcherRename => "名前を変更…",
        L10nKey::SwitcherPickAWorkspace => "ワークスペースを選ぶとタブが表示されます",
        L10nKey::SwitcherNoTabs => "このワークスペースにタブはありません",
        L10nKey::SwitcherNoTabMatch => "一致するタブがありません",
        L10nKey::SwitcherTabsAfterOpening => "このワークスペースを開くとタブが表示されます",
        L10nKey::SwitcherOpenToManage => "このワークスペースを開くと名前の変更や停止ができます",
        L10nKey::SwitcherConnectToUse => "このマシンに接続するとワークスペースを作成できます",
        L10nKey::SwitcherOrphanPanes => {
            "バックグラウンドペイン — どのウィンドウにも属さずに実行中のシェル:"
        }
        L10nKey::SwitcherTabCount => "{n} 個のタブ",
        L10nKey::SwitcherTabCountOne => "1 個のタブ",
        L10nKey::SwitcherActiveTab => "アクティブ",
        L10nKey::SwitcherHoldToSwitch => "Tab で移動 · 離して切り替え",
        L10nKey::SwitcherTabToCrossColumns => "Tab で列を移動",
        L10nKey::SwitcherLocalHost => "ローカル",
        L10nKey::SwitcherConnectingTo => "{machine} に接続中…",
        L10nKey::SwitcherFormName => "名前",
        L10nKey::SwitcherFormHost => "ホスト",
        L10nKey::SwitcherFormNamePlaceholder => "任意",
        L10nKey::SwitcherFormBack => "戻る",
        L10nKey::SwitcherFormCreateHint => "Enter で作成 · Esc で戻る",
        L10nKey::SwitcherFormPickHint => "↑↓ で選択 · Enter で決定 · Esc で閉じる",
        L10nKey::SshPromptPasswordFor => "{user}@{host} のパスワード",
        L10nKey::SshPromptPassphraseFor => "{key_path} のパスフレーズ",
        L10nKey::SshPromptTwoFactor => "二要素認証",
        L10nKey::SshPromptUnknownHost => "未知のホスト {host}",
        L10nKey::SshPromptHostKeyChanged => {
            "ホストキーが変更されました — 中間者攻撃の可能性があります"
        }
        L10nKey::SshPromptHostKeyChangedBody => {
            "ホストキーが以前に信頼したものと異なります。攻撃の可能性があります"
        }
        L10nKey::SshPromptConnect => "接続",
        L10nKey::SshPromptUnlock => "ロック解除",
        L10nKey::SshPromptSubmit => "送信",
        L10nKey::HostOpsError => "{context}: {error}",
        L10nKey::IoDenied => "権限がありません。",
        L10nKey::IoGone => "もう存在しません。",
        L10nKey::IoNoSpace => "ディスクに空き容量がありません。",
        L10nKey::IoReadOnly => "その場所は読み取り専用です。",
        L10nKey::IoBusy => "他のプログラムが使用中です。",
        L10nKey::IoTimedOut => "時間内に応答がありませんでした。",
        L10nKey::TreeWindowOpenedEmpty => {
            "サーバーがこのウィンドウのタブを渡さなかったため、空のまま開きました。失われたものはなく、応答すれば戻ります。戻らない場合はコマンドパレットの「サーバーを再起動」を実行してください"
        }
        L10nKey::CmdGroupTabsPanes => "タブとペイン",
        L10nKey::CmdGroupWorkspaces => "ワークスペース",
        L10nKey::CmdGroupView => "表示",
        L10nKey::CmdGroupGit => "Git",
        L10nKey::CmdGroupTerminal => "ターミナル",
        L10nKey::CmdGroupSsh => "SSH",
        L10nKey::CmdGroupAgents => "エージェント",
        L10nKey::CmdGroupApplication => "アプリケーション",
        L10nKey::CmdNewTab => "新しいタブ",
        L10nKey::CmdNewWorktreeTab => "新しいワークツリータブ…",
        L10nKey::CmdNewWorktreeTabSubtitle => "新しいブランチでの独立したチェックアウト",
        L10nKey::CmdRenameTab => "タブの名前を変更…",
        L10nKey::CmdSplitRight => "右に分割",
        L10nKey::CmdSplitDown => "下に分割",
        L10nKey::CmdZoomPane => "ペインを拡大",
        L10nKey::CmdNextPane => "次のペイン",
        L10nKey::CmdPreviousPane => "前のペイン",
        L10nKey::CmdFocusPaneLeft => "左のペインにフォーカス",
        L10nKey::CmdFocusPaneRight => "右のペインにフォーカス",
        L10nKey::CmdFocusPaneUp => "上のペインにフォーカス",
        L10nKey::CmdFocusPaneDown => "下のペインにフォーカス",
        L10nKey::CmdResizePaneLeft => "ペインを左にリサイズ",
        L10nKey::CmdResizePaneRight => "ペインを右にリサイズ",
        L10nKey::CmdResizePaneUp => "ペインを上にリサイズ",
        L10nKey::CmdResizePaneDown => "ペインを下にリサイズ",
        L10nKey::CmdSwapPaneNext => "次のペインと入れ替え",
        L10nKey::CmdSwapPanePrevious => "前のペインと入れ替え",
        L10nKey::CmdNextTab => "次のタブ",
        L10nKey::CmdPreviousTab => "前のタブ",
        L10nKey::CmdCopyWorkingDirectory => "作業ディレクトリをコピー",
        L10nKey::CmdCopySessionId => "セッション ID をコピー",
        L10nKey::CmdCopySessionIdSubtitle => "コーディングエージェント自身のセッション ID",
        L10nKey::CmdForkSession => "セッションをフォーク",
        L10nKey::CmdForkSessionSubtitle => "このエージェントのセッションを新しいタブにフォーク",
        L10nKey::CmdMarkTabAsUnread => "タブを未読としてマーク",
        L10nKey::CmdClosePaneTab => "ペイン / タブを閉じる",
        L10nKey::CmdCloseOtherTabs => "他のタブを閉じる",
        L10nKey::CmdCloseTabsToTheRight => "右側のタブを閉じる",
        L10nKey::CmdReopenClosedTab => "閉じたタブをもう一度開く",
        L10nKey::CmdNewWorkspace => "新しいワークスペース…",
        L10nKey::CmdSwitchWorkspace => "ワークスペースを切り替える…",
        L10nKey::CmdRenameWorkspace => "ワークスペースの名前を変更…",
        L10nKey::CmdStopWorkspace => "ワークスペースを停止…",
        L10nKey::CmdStopWorkspaceSubtitle => "シェルを終了し、レイアウトを保持",
        L10nKey::CmdDeleteWorkspace => "ワークスペースを削除…",
        L10nKey::CmdDeleteWorkspaceSubtitle => "シェルを終了し、レイアウトを消去",
        L10nKey::CmdShowLeftSidebar => "左サイドバーを表示",
        L10nKey::CmdHideLeftSidebar => "左サイドバーを非表示",
        L10nKey::CmdHideRightPanel => "右パネルを非表示",
        L10nKey::CmdShowRightPanel => "右パネルを表示",
        L10nKey::CmdShowCodePanel => "コードパネルを表示",
        L10nKey::CmdTabBarMoveToTop => "タブバー: 上部へ移動",
        L10nKey::CmdTabBarMoveToLeftSidebar => "タブバー: 左サイドバーへ移動",
        L10nKey::CmdRightPanelInfo => "右パネル: 情報",
        L10nKey::CmdRightPanelChanges => "右パネル: 変更",
        L10nKey::CmdRightPanelFiles => "右パネル: ファイル",
        L10nKey::CmdChangeTheme => "テーマを変更…",
        L10nKey::CmdResetFontSize => "フォントサイズをリセット",
        L10nKey::CmdEnterFullScreen => "全画面表示",
        L10nKey::CmdToggleDiffViewMode => "統合 / 左右分割の差分表示を切り替え",
        L10nKey::CmdDocumentDock => "ドキュメント: ターミナルの隣にドック",
        L10nKey::CmdDocumentFill => "ドキュメント: ウィンドウ全体",
        L10nKey::CmdToggleDocumentFill => "ドキュメントのフィル / ドックを切り替え",
        L10nKey::CmdDocumentWidthThird => "ドキュメント: 幅3分の1",
        L10nKey::CmdDocumentWidthHalf => "ドキュメント: 幅半分",
        L10nKey::CmdDocumentWidthTwoThirds => "ドキュメント: 幅3分の2",
        L10nKey::CmdGitCommit => "Git: コミット",
        L10nKey::CmdGitStageAll => "Git: すべての変更をステージ",
        L10nKey::CmdGitUnstageAll => "Git: すべてのステージを取り消す",
        L10nKey::CmdGitDiscardAll => "Git: すべての変更を破棄",
        L10nKey::CmdGitDiscardAllSubtitle => "ワークツリーの未コミットの変更をすべて捨てます。",
        L10nKey::CmdGitCheckoutTo => "Git: チェックアウト…",
        L10nKey::CmdGitCreateBranch => "Git: ブランチを作成…",
        L10nKey::CmdGitSync => "Git: 同期",
        L10nKey::CmdGitSyncSubtitle => "プルしてからプッシュします。",
        L10nKey::CmdGitPush => "Git: プッシュ",
        L10nKey::CmdGitPull => "Git: プル",
        L10nKey::CmdGitFetch => "Git: フェッチ",
        L10nKey::CmdGitToggleGraph => "Git: コミット履歴の表示切替",
        L10nKey::CmdClearScrollback => "スクロールバックをクリア",
        L10nKey::CmdFindInTerminal => "ターミナル内を検索…",
        L10nKey::CmdFindNext => "次を検索",
        L10nKey::CmdFindPrevious => "前を検索",
        L10nKey::CmdCopy => "コピー",
        L10nKey::CmdCut => "切り取り",
        L10nKey::CmdPaste => "貼り付け",
        L10nKey::CmdAlternatePaste => "貼り付け（全画面アプリを除く）",
        L10nKey::CmdSelectAll => "すべて選択",
        L10nKey::CmdSshAddConnection => "SSH: 接続を追加…",
        L10nKey::CmdSshManageProfiles => "SSH: プロファイルを管理…",
        L10nKey::CmdSshReconnect => "SSH: 再接続",
        L10nKey::CmdSshRemoteFiles => "SSH: リモートファイル",
        L10nKey::CmdSshPortForwarding => "SSH: ポートフォワーディング",
        L10nKey::CmdSshSaveConnection => "SSH: この接続をホストとして保存…",
        L10nKey::CmdSshSaveConnectionSubtitle => "この接続を保存済みホストとして残します",
        L10nKey::CmdSshConnectWithInput => "SSH: {input} に接続",
        L10nKey::CmdAgentSendSelection => "エージェント: 選択範囲を送信",
        L10nKey::CmdAgentSendSelectionSubtitle => "選択範囲 → 実行中のコーディングエージェント",
        L10nKey::CmdAgentSendGitDiffForReview => "エージェント: レビュー用に Git Diff を送信",
        L10nKey::CmdAgentSendGitDiffSubtitle => "git diff → 実行中のコーディングエージェント",
        L10nKey::CmdSettings => "設定…",
        L10nKey::CmdKeyboardShortcuts => "キーボードショートカット",
        L10nKey::CmdAboutTty7 => "tty7 について",
        L10nKey::CmdCheckForUpdates => "アップデートを確認…",
        L10nKey::CmdDocumentation => "ドキュメント",
        L10nKey::CmdJoinDiscord => "Discord に参加",
        L10nKey::CmdReportIssue => "問題を報告…",
        L10nKey::CmdRestartServer => "サーバーを再起動…",
        L10nKey::CmdRestartServerSubtitle => "実行中のすべてのシェルを終了し、レイアウトは保持",
        L10nKey::CmdQuitTty7 => "tty7 を終了",
        L10nKey::CmdQuitTty7Subtitle => "シェルは実行を継続",
        L10nKey::CmdQuickConnect => "「{target}」に接続",
        L10nKey::CmdQuickConnectSaveProfile => "「{target}」をプロファイルとして保存…",
        L10nKey::CmdRecent => "最近",
        L10nKey::AppRestartServerTitle => "サーバーを再起動しますか？",
        L10nKey::AppRestartServerFailed => {
            "バックグラウンドサーバーを再起動できませんでした: {error}"
        }
        L10nKey::AppRestartServerMismatchDetail => {
            "サーバーはプロトコル {protocol}（ビルド v{build}）、このアプリは {ours} のため、タブを取り出せません。\n\n終了：何も変わりません。サーバーもシェルも動き続けます。\n再起動：タブは新しいシェルで戻り、いま実行中のものは終了します"
        }
        L10nKey::AppRestartServerDialectDetail => {
            "サーバーは制御方言 v{dialect}（ビルド v{build}）、このアプリは v{ours} のため、ウィンドウはどれも空で開きます。\n\n終了：何も変わりません。サーバーもシェルも動き続けます。\n再起動：タブは新しいシェルで戻り、いま実行中のものは終了します"
        }
        L10nKey::AppRestartServerDialectNewerDetail => {
            "サーバーは制御方言 v{dialect}（ビルド v{build}）、このアプリは v{ours} のため、ウィンドウはどれも空で開きます。\n\n終了して新しいビルドを入れる：根本的な解決で、シェルはそのまま残ります。\n再起動：タブは新しいシェルで戻り、いま実行中のものは終了します"
        }
        L10nKey::AppRestartServerOldDetail => {
            "サーバーはバージョン照合より前のもので、何を話すか分かりません。\n\n終了：何も変わりません。サーバーもシェルも動き続けます。\n再起動：タブは新しいシェルで戻り、いま実行中のものは終了します"
        }
        L10nKey::AppRestart => "再起動",
        L10nKey::AppRestartServerNoServer => {
            "{label} には再起動できるサーバーがありません。このコンピュータが --stdio で実行しているプログラムです。代わりにワークスペースを止めてください"
        }
        L10nKey::AppRestartServerBody => {
            "このコンピュータのシェルはすべて終了します。タブとレイアウトは保持され、新しいシェルで開きます"
        }
        L10nKey::ConfigQuarantinedStartup => {
            "config.json を解析できませんでした。デフォルト設定で実行しており、内容は config.json.corrupt として残しました。直せば自動で再読み込みされます。それまで設定の変更は保存されません"
        }
        L10nKey::ConfigQuarantinedReload => {
            "編集された config.json を解析できませんでした。実行中の設定を保持し、内容は config.json.corrupt として残しました。直せば自動で再読み込みされます。それまでに設定を保存すると上書きされます"
        }
        L10nKey::ConfigUnreadableStartup => {
            "config.json を読み込めませんでした。デフォルト設定で実行しており、ファイルはそのままです。権限か内容を直せば自動で再読み込みされます。それまで設定の変更は保存されません"
        }
        L10nKey::ConfigUnreadableReload => {
            "config.json を読み込めませんでした。実行中の設定を保持し、ファイルもそのままです。権限か内容を直せば自動で再読み込みされます。それまでに設定を保存すると上書きされます"
        }
        L10nKey::AppWorktreeRemoveDetailDirty => {
            "閉じたタブの {path} にあるワークツリーには未コミットの変更があります"
        }
        L10nKey::AppWorktreeRemoveDetailClean => {
            "閉じたタブの {path} にあるワークツリーはクリーンです"
        }
        L10nKey::AppWorktreeRemoveTitle => "ワークツリー「{branch}」を削除しますか？",
        L10nKey::AppWorktreeDiscardAndRemove => "変更を破棄して削除",
        L10nKey::AppWorktreeRemove => "ワークツリーを削除",
        L10nKey::AppWorktreeKeep => "保持",
        L10nKey::AppReopenTabFailed => "タブを開き直せませんでした: ターミナルが起動しませんでした",
        L10nKey::AppOpenTerminalFailed => "ターミナルを開けませんでした: {error}",
        L10nKey::AppTabsNotRestored => "前回のタブ {count} 個を開き直せませんでした",
        L10nKey::LaunchWorkspacesLeftRunning => {
            "このウィンドウだけを復元しました — あと {count} 個のワークスペースがバックグラウンドで実行中です。サイドバーから開き直せます。"
        }
        L10nKey::AppSshConnectionFailed => "SSH 接続に失敗しました: {error}",
        L10nKey::AppSshReconnectFailed => "SSH 再接続に失敗しました: {error}",
        L10nKey::AppSplitPaneFailed => "ペインを分割できませんでした: {error}",
        L10nKey::PaneDragHandleTooltip => "ドラッグしてこのペインを移動",
        L10nKey::AppWorktreeRemoved => "ワークツリー「{branch}」を削除しました",
        L10nKey::AppWorktreeRemoveFailed => "ワークツリーの削除に失敗しました: {error}",
        L10nKey::AppForkStillConnecting => "フォークできませんでした: ペインはまだ接続中です",
        L10nKey::AppPaneNoCodingAgent => "このペインはコーディングエージェントを実行していません",
        L10nKey::AppForkNoCommand => "tty7 には {name} 用のフォークコマンドがありません",
        L10nKey::AppForkLocalOnly => {
            "{name} のセッションはローカルペインからしかフォークできません"
        }
        L10nKey::AppForkNoSessionId => {
            "tty7 はこのペインで {name} のセッション ID を確認できていません — 設定 → エージェントでフックをインストールしてください"
        }
        L10nKey::AppForkSessionIdNotToken => {
            "{name} のセッション ID はプレーンなトークンではありません"
        }
        L10nKey::AppForkMidTurn => {
            "{name} は処理の途中です — 進行中のターンはフォークに含まれません"
        }
        L10nKey::AppTabNoWorkingDirectory => "このタブにはまだ作業ディレクトリがありません",
        L10nKey::AppNothingSelected => {
            "選択されているものはありません — 先にターミナルの出力を選択してください"
        }
        L10nKey::AppPaneNoKnownDirectory => "このペインには既知のディレクトリがありません",
        L10nKey::AppNoUncommittedChanges => {
            "{cwd} に未コミットの変更はありません（または git リポジトリではありません）"
        }
        L10nKey::AppCmdSshProfileTitle => "SSH: {title}",
        L10nKey::AppCmdSwitchToTab => "タブに切り替え: {label}",
        L10nKey::AppPlaceholderDescription => "説明",
        L10nKey::AppPlaceholderSshQuickConnect => "user@host  または  user@host:port",
        L10nKey::AppPlaceholderLoginShell => "ログインシェル",
        L10nKey::AppPlaceholderNone => "なし",
        L10nKey::AppPlaceholderOpenInDefaultApp => "デフォルトのアプリで開く",
        L10nKey::AppThemeColorBackground => "背景",
        L10nKey::AppThemeColorForeground => "前景",
        L10nKey::AppThemeColorAccent => "アクセント",
        L10nKey::AppThemeColorCursor => "カーソル",
        L10nKey::AppThemeColorSelection => "選択範囲",
        L10nKey::AppThemeAnsiBlack => "黒",
        L10nKey::AppThemeAnsiRed => "赤",
        L10nKey::AppThemeAnsiGreen => "緑",
        L10nKey::AppThemeAnsiYellow => "黄",
        L10nKey::AppThemeAnsiBlue => "青",
        L10nKey::AppThemeAnsiMagenta => "マゼンタ",
        L10nKey::AppThemeAnsiCyan => "シアン",
        L10nKey::AppThemeAnsiWhite => "白",
        L10nKey::AppThemeAnsiBrightBlack => "明るい黒",
        L10nKey::AppThemeAnsiBrightRed => "明るい赤",
        L10nKey::AppThemeAnsiBrightGreen => "明るい緑",
        L10nKey::AppThemeAnsiBrightYellow => "明るい黄",
        L10nKey::AppThemeAnsiBrightBlue => "明るい青",
        L10nKey::AppThemeAnsiBrightMagenta => "明るいマゼンタ",
        L10nKey::AppThemeAnsiBrightCyan => "明るいシアン",
        L10nKey::AppThemeAnsiBrightWhite => "明るい白",
        L10nKey::AppAgentHooksThisComputer => "このコンピュータ",
        L10nKey::AppAgentHooksRemoteMachine => "リモートマシン",
        L10nKey::AppAgentHooksNoHomeDir => {
            "tty7 はこのコンピュータのホームディレクトリを特定できなかったため、インストール先がありません"
        }
        L10nKey::AppAgentHooksOffline => {
            "このマシンに接続されていないため、エージェントの設定を読み書きできません。そのマシンでワークスペースを開いてから戻ってください"
        }
        L10nKey::AppAgentHooksHomeDirUnresolved => "ホームディレクトリを解決できません",
        L10nKey::AppAgentHooksInstalled => "インストール済み",
        L10nKey::AppAgentHooksInstalledEnableCodexThere => {
            "インストール済み — そのマシンで `codex features enable hooks` を一度実行してください"
        }
        L10nKey::AppAgentHooksInstalledCodexEnableFailed => {
            "インストール済みですが `codex features enable hooks` を実行できませんでした ({error}) — 手動で一度実行してください"
        }
        L10nKey::AppAgentHooksRemoved => "削除済み",
        L10nKey::AppAgentHooksNothingInstalled => {
            "インストールされていないため、削除するものはありません"
        }
        L10nKey::AppAgentHooksNoTty7Hooks => {
            "tty7 のフックが見つからないため、削除するものはありません"
        }
        L10nKey::AppAgentHooksInstallFailed => "フックをインストールできませんでした: {error}",
        L10nKey::AppAgentHooksRemoveFailed => "フックを削除できませんでした: {error}",
        L10nKey::AppKeybindingDisplacedNote => {
            "{action} が {previous} からショートカットを奪いました。{previous} は現在未設定です"
        }
        L10nKey::AppLocalServerName => "ローカルサーバー",
        L10nKey::AppSshParseUnbalancedQuotes => "SSH コマンド内の引用符が閉じていません",
        L10nKey::AppSshParseNoRemoteCommands => "ここではリモートコマンドをサポートしていません",
        L10nKey::AppSshParseFlagNeedsValue => "-{flag} には値が必要です",
        L10nKey::AppSshParseInvalidPort => "無効なポート「{value}」",
        L10nKey::AppSshParseUnsupportedOption => "サポートされていないオプション「{option}」",
        L10nKey::AppSshParseEnterHost => "接続先のホストを入力してください",
        L10nKey::AppSshParseBadHost => "ホスト「{host}」を解析できません",
        L10nKey::AppMenuMinimize => "最小化",
        L10nKey::AppMenuZoom => "ズーム",
        L10nKey::SwitcherStatusRestarting => "再起動中…",
        L10nKey::SwitcherStatusInstalling => "インストール中…",
        L10nKey::SwitcherStatusConnecting => "接続中…",
        L10nKey::SwitcherStatusConnectFailed => "接続できませんでした",
        L10nKey::SwitcherStatusNotConnected => "未接続",
        L10nKey::SwitcherStatusReconnecting => "再接続中…",
        L10nKey::SwitcherStatusTakenOver => "他のクライアントが使用中",
        L10nKey::SettingsFontDefault => "デフォルト（メインに合わせる）",
        L10nKey::SettingsUiFontDefault => "デフォルト（システム UI フォント）",
        L10nKey::ForwardDescriptionPlaceholder => "用途",
        L10nKey::SettingsShellDefaultLoginShell => "あなたのログインシェル",
        L10nKey::SettingsShellDetected => "tty7 が見つけたシェル",
        L10nKey::SftpErrorUnexpectedReply => "予期しない応答: {reply}",
        L10nKey::SftpErrorUnsafeRemoteName => "安全でないリモート名 {name} を拒否しました",
        L10nKey::SftpErrorNoFreeLocalName => {
            "ダウンロードフォルダに {name} の空き名がありません。古いコピーを移動または削除してください"
        }
        L10nKey::SftpReplaceTitle => "既にあるファイルを置き換えますか？",
        L10nKey::SftpReplaceBody => {
            "{names} はこのフォルダに既に存在します。アップロードすると上書きされます。"
        }
        L10nKey::Replace => "置き換える",
        L10nKey::SftpErrorInvalidOctalMode => "無効な 8 進数モードです",
        L10nKey::SettingsDaemonStaleDescInPlace => {
            "tty7 はその場で更新されました。アプリは新しく、ペインはまだ前のビルドで動いています。サーバーは停止せずに新しいビルドへ置き換えられるので、シェルはそのまま引き継がれます。tty7 内蔵の SSH クライアントを使うペインだけは例外で、その接続は閉じられ、開き直しが必要です"
        }
        L10nKey::AppRestartServerBodyInPlace => {
            "サーバーは停止せずに自分自身をこのビルドへ置き換えます。シェルは動いたままで、ウィンドウはすぐに再接続します。tty7 内蔵の SSH クライアントを使うペインだけは例外で、その接続は閉じられ、開き直しが必要です"
        }
        L10nKey::PaneRestoredScreenBanner => {
            "復元された画面 — 以下は新しいシェルで、これより上のものは動いていません"
        }
        L10nKey::SettingsPerPaneHistory => "ペインごとに独自のシェル履歴を持たせる",
        L10nKey::SettingsPerPaneHistoryDescription => {
            "上キーでたどるのは、全ペインが混ざったものではなくこのペインで実行したコマンドです。新しいペインは既存の履歴から始まり、追加分は閉じるときに書き戻されます。対象は tty7 が設定できる bash と zsh のペインで、独自の引数で起動したシェルはそのままです"
        }
        L10nKey::IntegrationNoticeBlocked => {
            "“{wrapper}”がこのペインのシェルレポートを横取りしているため、インライン補完と Ctrl+R メニューは使えません。シェル独自の履歴検索は引き続き使えます。"
        }
        L10nKey::IntegrationNoticeNotEngaged => {
            "このペインでは tty7 シェル統合が有効になっておらず、インライン補完と Ctrl+R メニューは使えません。よくある原因は、独自の引数で起動したシェル、PTY ラッパー、未対応のシェルです。"
        }
        L10nKey::PaneTitleDisconnected => "{title} — 切断されました",
        L10nKey::PaneTitleProcessExited => "{title} — プロセスが終了しました",
        L10nKey::LoopbackForwardFailed => ":{port} を転送できませんでした — {error}",
        L10nKey::TrayTooltipAgents => "tty7: {parts}",
        L10nKey::TrayAgentSep => "、",
        L10nKey::CursorShapeBlock => "ブロック",
        L10nKey::CursorShapeBar => "バー",
        L10nKey::CursorShapeUnderline => "下線",
        L10nKey::PaletteTryDifferentSearch => "別のキーワードを試してください。",
        L10nKey::CompletionListingRemote => "リモートを一覧しています…",
        L10nKey::CompletionRemoteListingFailed => "リモートの一覧に失敗しました — {error}",
        L10nKey::PanelMoreChangedFiles => {
            "… さらに変更されたファイル {count} 個 — 表示するには `git diff` を実行してください"
        }
        L10nKey::ScmFilesChanged => "{count} 個のファイルが変更されました",
        L10nKey::ScmStagedFileCount => "{count} 個のファイルがステージされました",
        L10nKey::AppMenuAbout => "tty7 について",
        L10nKey::AppMenuCheckForUpdates => "アップデートを確認…",
        L10nKey::AppMenuSettings => "設定…",
        L10nKey::AppMenuServices => "サービス",
        L10nKey::AppMenuHideApp => "tty7 を非表示",
        L10nKey::AppMenuHideOthers => "ほかを非表示",
        L10nKey::AppMenuShowAll => "すべて表示",
        L10nKey::AppMenuQuit => "tty7 を終了",
        L10nKey::AppMenuFile => "ファイル",
        L10nKey::AppMenuEdit => "編集",
        L10nKey::AppMenuView => "表示",
        L10nKey::AppMenuWindow => "ウィンドウ",
        L10nKey::AppMenuHelp => "ヘルプ",
        L10nKey::AppMenuNewTab => "新規タブ",
        L10nKey::AppMenuNewWorkspace => "新規ワークスペース…",
        L10nKey::AppMenuNewWorktreeTab => "新規ワークツリータブ…",
        L10nKey::AppMenuSplitRight => "右に分割",
        L10nKey::AppMenuSplitLeft => "左に分割",
        L10nKey::AppMenuSplitDown => "下に分割",
        L10nKey::AppMenuSplitUp => "上に分割",
        L10nKey::AppMenuRenameTab => "タブの名前を変更…",
        L10nKey::AppMenuCopyWorkingDirectory => "作業ディレクトリをコピー",
        L10nKey::AppMenuCopySessionId => "セッション ID をコピー",
        L10nKey::AppMenuForkSession => "セッションをフォーク",
        L10nKey::AppMenuClosePaneTab => "ペイン / タブを閉じる",
        L10nKey::AppMenuCloseOtherTabs => "他のタブを閉じる",
        L10nKey::AppMenuCloseTabsRight => "右側のタブを閉じる",
        L10nKey::AppMenuReopenClosedTab => "閉じたタブをもう一度開く",
        L10nKey::AppMenuRenameWorkspace => "ワークスペースの名前を変更…",
        L10nKey::AppMenuStopWorkspace => "ワークスペースを停止…",
        L10nKey::AppMenuDeleteWorkspace => "ワークスペースを削除…",
        L10nKey::AppMenuUndo => "元に戻す",
        L10nKey::AppMenuRedo => "やり直す",
        L10nKey::AppMenuCut => "切り取り",
        L10nKey::AppMenuCopy => "コピー",
        L10nKey::AppMenuPaste => "貼り付け",
        L10nKey::AppMenuSelectAll => "すべて選択",
        L10nKey::AppMenuFind => "検索…",
        L10nKey::AppMenuFindNext => "次を検索",
        L10nKey::AppMenuFindPrevious => "前を検索",
        L10nKey::AppMenuCommandPalette => "コマンドパレット…",
        L10nKey::AppMenuIncreaseFontSize => "フォントサイズを拡大",
        L10nKey::AppMenuDecreaseFontSize => "フォントサイズを縮小",
        L10nKey::AppMenuResetFontSize => "フォントサイズをリセット",
        L10nKey::AppMenuLeftSidebar => "左サイドバー",
        L10nKey::AppMenuRightPanel => "右パネル",
        L10nKey::AppMenuCodePanel => "コードパネル",
        L10nKey::AppMenuTabBarPosition => "タブバーの位置",
        L10nKey::AppMenuFocusNextPane => "次のペインにフォーカス",
        L10nKey::AppMenuFocusPreviousPane => "前のペインにフォーカス",
        L10nKey::AppMenuZoomPane => "ペインを拡大",
        L10nKey::AppMenuClearScrollback => "スクロールバックをクリア",
        L10nKey::AppMenuDocumentation => "tty7 ドキュメント",
        L10nKey::AppMenuKeyboardShortcuts => "キーボードショートカット",
        L10nKey::AppMenuJoinDiscord => "Discord に参加",
        L10nKey::AppMenuReportIssue => "問題を報告…",
        L10nKey::AppMenuRestartServer => "サーバーを再起動…",
        L10nKey::WindowUntitled => "無題",
        L10nKey::TrayShowTty7 => "tty7 を表示",
        L10nKey::TrayNotifications => "通知",
        L10nKey::TrayAgentNeedsInput => "入力が必要",
        L10nKey::AgentStatusWorking => "実行中",
        L10nKey::AgentStatusWaiting => "入力が必要",
        L10nKey::AgentStatusDone => "完了",
        L10nKey::NotifyCommandFinished => "コマンドが {secs} 秒で完了しました",
        L10nKey::NotifyCommandFinishedWithCommand => "{command} — {secs} 秒で完了しました",
        L10nKey::NotifyAgentFinished => "{secs} 秒で完了しました",
        L10nKey::NotifyAgentWaiting => "入力を待っています",
        L10nKey::NotifyTurnFinished => "ターンが完了しました",
        L10nKey::TabTooltipMore => "その他",
        L10nKey::TabTooltipShowSidebar => "サイドバーを表示",
        L10nKey::TabTooltipHideSidebar => "サイドバーを非表示",
        L10nKey::TabTooltipHideDetailPanel => "詳細パネルを非表示",
        L10nKey::TabTooltipShowDetailPanel => "詳細パネルを表示",
        L10nKey::TabMenuLocalShells => "ローカル",
        L10nKey::TabMenuAddHost => "SSH ホストを追加…",
        L10nKey::TabMenuAllHosts => "すべての SSH ホスト…",
        L10nKey::TabMenuSplitHint => "{key} を押しながら選ぶと分割",
        L10nKey::TabUnnamedShell => "シェル {n}",
        L10nKey::ShellDefault => "デフォルト",
        L10nKey::SidebarScratchGroup => "スクラッチ",
        L10nKey::SidebarProjectsHeading => "プロジェクト",
        L10nKey::SidebarTabsHeading => "タブ",
        L10nKey::ProjectNewTooltip => "フォルダをプロジェクトにする",
        L10nKey::ProjectRename => "プロジェクト名を変更…",
        L10nKey::ProjectSetFolder => "フォルダを変更…",
        L10nKey::ProjectDelete => "プロジェクトを削除",
        L10nKey::ProjectNewTab => "ここで新規タブ",
        L10nKey::ProjectEmpty => "ここでタブを開く",
        L10nKey::TabProjectAddTo => "プロジェクトへ移動",
        L10nKey::TabProjectRemove => "プロジェクトから外す",
        L10nKey::TabProjectFromFolder => "このフォルダで新規プロジェクト",
        L10nKey::ProjectNoFolder => "このタブにはプロジェクトにできるフォルダがありません",
        L10nKey::ProjectLimitReached => {
            "このワークスペースには既に {max} 個のプロジェクトがあります"
        }
        L10nKey::TabContextCloseTab => "タブを閉じる",
        L10nKey::TabContextCloseTabsBelow => "下のタブを閉じる",
        L10nKey::AppAgentHooksOpFailed => "失敗: {error}",
        L10nKey::AppMenuEnterFullscreen => "全画面表示",
        L10nKey::HomeTimeOverWeekAgo => "1 週間以上前",
        L10nKey::Search => "検索",
        L10nKey::SettingsDaemonStaleRestart => "サービスを再起動",
        L10nKey::SettingsNoneLower => "なし",
        L10nKey::SettingsSearchCommandLineToolTitle => "コマンドラインツール",
        L10nKey::TabContextMarkUnread => "未読としてマーク",
    })
}

pub fn translate_variant_ja(key: L10nKey, branch: &'static str) -> Option<&'static str> {
    let res = match (key, branch) {
        (L10nKey::SettingsDeleteProfileCascade, "one") => {
            "{endpoint} を参照しているリモートワークスペースのエントリが 1 件あり、\
             プロファイルと一緒にこのコンピュータから削除されます。リモートマシン上のセッションは\
             維持され、新しいプロファイルで接続すればワークスペース一覧に再表示されます。"
        }
        (L10nKey::SettingsDeleteProfileCascade, "other") => {
            "{endpoint} を参照しているリモートワークスペースのエントリが {count} 件あり、\
             プロファイルと一緒にこのコンピュータから削除されます。リモートマシン上のセッションは\
             維持され、新しいプロファイルで接続すればワークスペース一覧に再表示されます。"
        }
        (L10nKey::SettingsAliasesLinked, "zero") => "エイリアスはまだリンクされていません",
        (L10nKey::SettingsAliasesLinked, "one") => "エイリアス 1 件がリンクされています",
        (L10nKey::SettingsAliasesLinked, "other") => "エイリアス {count} 件がリンクされています",
        (L10nKey::SettingsImportSummary, "zero") => {
            "新しいホストはありません — {updated} 件を更新、{unchanged} 件は変更なし"
        }
        (L10nKey::SettingsImportSummary, "one") => {
            "ホスト 1 件を追加 — {updated} 件を更新、{unchanged} 件は変更なし"
        }
        (L10nKey::SettingsImportSummary, "other") => {
            "ホスト {count} 件を追加 — {updated} 件を更新、{unchanged} 件は変更なし"
        }
        (L10nKey::SettingsImportIgnored, "zero") => {
            "ファイル内のすべてのオプションに tty7 側の設定があります"
        }
        (L10nKey::SettingsImportIgnored, "one") => {
            "tty7 に設定のないオプションが 1 件あり、ファイルに残されています: {options}"
        }
        (L10nKey::SettingsImportIgnored, "other") => {
            "tty7 に設定のないオプションが {count} 件あり、ファイルに残されています: {options}"
        }
        (L10nKey::SettingsRulesOpenedWithConnection, "zero") => "接続と同時に開くルール 0 件",
        (L10nKey::SettingsRulesOpenedWithConnection, "one") => "接続と同時に開くルール 1 件",
        (L10nKey::SettingsRulesOpenedWithConnection, "other") => {
            "接続と同時に開くルール {count} 件"
        }
        (L10nKey::SettingsOfflineMachines, "zero") => {
            "未接続の保存済みマシンはもうありません — いずれかでワークスペースを開くと、そこにフックをインストールできます"
        }
        (L10nKey::SettingsOfflineMachines, "one") => {
            "未接続の保存済みマシンがもう 1 台あります — そのマシンでワークスペースを開くと、そこにフックをインストールできます"
        }
        (L10nKey::SettingsOfflineMachines, "other") => {
            "未接続の保存済みマシンがさらに {count} 台あります — いずれかでワークスペースを開くと、そこにフックをインストールできます"
        }
        (L10nKey::SettingsForgetPasswordSharedBody, "one") => {
            "他にも 1 件のホストプロファイルが {endpoint} を使っているため、その接続でもパスワードの再入力が必要になります"
        }
        (L10nKey::SettingsForgetPasswordSharedBody, "other") => {
            "他にも {count} 件のホストプロファイルが {endpoint} を使っているため、それらの接続でもパスワードの再入力が必要になります"
        }
        (L10nKey::SftpReplaceBody, "one") => {
            "{names} はこのフォルダに既に存在します。アップロードすると上書きされます。"
        }
        (L10nKey::SftpReplaceBody, "other") => {
            "{names} はこのフォルダに既に存在します。アップロードすると上書きされます。"
        }
        (L10nKey::AppTabsNotRestored, "one") => "前回のタブ 1 個を開き直せませんでした",
        (L10nKey::AppTabsNotRestored, "other") => "前回のタブ {count} 個を開き直せませんでした",
        (L10nKey::LaunchWorkspacesLeftRunning, "one") => {
            "このウィンドウだけを復元しました — あと 1 個のワークスペースがバックグラウンドで実行中です。サイドバーから開き直せます。"
        }
        (L10nKey::LaunchWorkspacesLeftRunning, "other") => {
            "このウィンドウだけを復元しました — あと {count} 個のワークスペースがバックグラウンドで実行中です。サイドバーから開き直せます。"
        }
        (L10nKey::ScmFilesChanged, "zero") => "変更されたファイルはありません",
        (L10nKey::ScmFilesChanged, "one") => "1 個のファイルが変更されました",
        (L10nKey::ScmFilesChanged, "other") => "{count} 個のファイルが変更されました",
        (L10nKey::ScmStagedFileCount, "zero") => "ステージされた変更はありません",
        (L10nKey::ScmStagedFileCount, "one") => "1 個のファイルがステージされました",
        (L10nKey::ScmStagedFileCount, "other") => "{count} 個のファイルがステージされました",
        (L10nKey::PanelMoreChangedFiles, "zero") => {
            "… さらに変更されたファイル 0 個 — 表示するには `git diff` を実行してください"
        }
        (L10nKey::PanelMoreChangedFiles, "one") => {
            "… さらに変更されたファイル 1 個 — 表示するには `git diff` を実行してください"
        }
        (L10nKey::PanelMoreChangedFiles, "other") => {
            "… さらに変更されたファイル {count} 個 — 表示するには `git diff` を実行してください"
        }
        (L10nKey::DiffChangedFiles, "zero") => "変更されたファイル 0 個",
        (L10nKey::DiffChangedFiles, "one") => "変更されたファイル 1 個",
        (L10nKey::DiffChangedFiles, "other") => "変更されたファイル {count} 個",
        (L10nKey::DiffUntrackedCount, "zero") => " · 未追跡 0 件",
        (L10nKey::DiffUntrackedCount, "one") => " · 未追跡 1 件",
        (L10nKey::DiffUntrackedCount, "other") => " · 未追跡 {count} 件",
        (L10nKey::DiffMoreFiles, "zero") => {
            "… さらに変更されたファイル 0 個 — ターミナルで `git diff` を実行して確認してください"
        }
        (L10nKey::DiffMoreFiles, "one") => {
            "… さらに変更されたファイル 1 個 — ターミナルで `git diff` を実行して確認してください"
        }
        (L10nKey::DiffMoreFiles, "other") => {
            "… さらに変更されたファイル {count} 個 — ターミナルで `git diff` を実行して確認してください"
        }
        (L10nKey::DiffUntrackedHeader, "zero") => "未追跡ファイル (0)",
        (L10nKey::DiffUntrackedHeader, "one") => "未追跡ファイル (1)",
        (L10nKey::DiffUntrackedHeader, "other") => "未追跡ファイル ({count})",
        (L10nKey::DiffMoreUntracked, "zero") => {
            "… さらに 0 個 — ターミナルで `git status` を実行して確認してください"
        }
        (L10nKey::DiffMoreUntracked, "one") => {
            "… さらに 1 個 — ターミナルで `git status` を実行して確認してください"
        }
        (L10nKey::DiffMoreUntracked, "other") => {
            "… さらに {count} 個 — ターミナルで `git status` を実行して確認してください"
        }
        (L10nKey::DiffUntrackedSummary, "zero") => "未追跡 0",
        (L10nKey::DiffUntrackedSummary, "one") => "未追跡 1",
        (L10nKey::DiffUntrackedSummary, "other") => "未追跡 {count}",
        (L10nKey::HomeTimeMinutesAgo, "one") => "1 分前",
        (L10nKey::HomeTimeMinutesAgo, "other") => "{count} 分前",
        (L10nKey::HomeTimeHoursAgo, "one") => "1 時間前",
        (L10nKey::HomeTimeHoursAgo, "other") => "{count} 時間前",
        (L10nKey::HomeTimeDaysAgo, "one") => "1 日前",
        (L10nKey::HomeTimeDaysAgo, "other") => "{count} 日前",
        (L10nKey::HomeTimeWeeksAgo, "one") => "1 週間前",
        (L10nKey::HomeTimeWeeksAgo, "other") => "{count} 週間前",
        (L10nKey::HomeTimeMonthsAgo, "one") => "1 か月前",
        (L10nKey::HomeTimeMonthsAgo, "other") => "{count} か月前",
        (L10nKey::WindowStopShells, "zero") => "レイアウトと作業ディレクトリは消去されます",
        (L10nKey::WindowStopShells, "one") => "実行中のシェル 1 個が終了します",
        (L10nKey::WindowStopShells, "other") => "実行中のシェル {count} 個が終了します",
        (L10nKey::WindowDeleteShells, "zero") => "レイアウトと作業ディレクトリは消去されます",
        (L10nKey::WindowDeleteShells, "one") => {
            "実行中のシェル 1 個が終了し、レイアウトが消去されます"
        }
        (L10nKey::WindowDeleteShells, "other") => {
            "{count} 個の実行中シェルが終了し、レイアウトが消去されます"
        }
        _ => return None,
    };
    Some(res)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn japanese_covers_every_key() {
        assert_eq!(translate_ja(L10nKey::SearchTabs), Some("タブを検索…"));
        assert!(translate_variant_ja(L10nKey::WindowDeleteShells, "other").is_some());
    }
}

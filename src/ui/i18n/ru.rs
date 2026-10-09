use super::L10nKey;

pub fn translate_ru(key: L10nKey) -> Option<&'static str> {
    Some(match key {
        L10nKey::SettingsNoMatchesShort => "Нет совпадений",
        L10nKey::SettingsModifiedTitle => "Изменено",
        L10nKey::SettingsMatchCount => "Совпадений: {count}",
        L10nKey::SettingsNothingModified => "Настройки по умолчанию не изменены",
        L10nKey::SettingsKeyThen => "затем",
        L10nKey::SettingsStartupRestore => "Запуск и восстановление",
        L10nKey::SettingsSettingsFile => "Файл конфигурации",
        L10nKey::SettingsReveal => "Показать в папке",
        L10nKey::SettingsThemeModeSystem => "Системная",
        L10nKey::SettingsThemeModeSystemDesc => {
            "Следовать системной теме, переключаясь между светлой и тёмной."
        }
        L10nKey::SettingsThemeModeLightDesc => "Всегда использовать светлую тему.",
        L10nKey::SettingsThemeModeDarkDesc => "Всегда использовать тёмную тему.",
        L10nKey::SettingsThemeSlotLightDesc => "Используется при светлой системной теме.",
        L10nKey::SettingsThemeSlotDarkDesc => "Используется при тёмной системной теме.",
        L10nKey::SettingsThemeSlotDesc => "Тема терминала и интерфейса.",
        L10nKey::SettingsLightThemeLabel => "Светлая тема",
        L10nKey::SettingsDarkThemeLabel => "Тёмная тема",
        L10nKey::SettingsNoThemesMatch => "Темы не найдены",
        L10nKey::SettingsRestoreChanged => "Сбросить {count} изменений",
        L10nKey::SettingsShortcutsHint => {
            "Нажмите на сочетание и введите новые клавиши. Чтобы задать последовательность, сразу нажмите второе сочетание. Esc отменяет, ⌫ очищает."
        }
        L10nKey::SettingsShortcutsHintTmux => {
            "Профиль tmux: команды панелей и вкладок вызываются через префикс. Нажмите на сочетание, чтобы изменить его. Esc отменяет, ⌫ очищает."
        }
        L10nKey::SettingsShortcutConflict => {
            "{keys} уже назначено команде «{action}». При замене сочетание будет снято с неё."
        }
        L10nKey::SettingsReplace => "Заменить",
        L10nKey::SettingsNoActionsMatch => "Нет команд по запросу «{query}»",
        L10nKey::SettingsSearchShortcuts => "Поиск команд и клавиш",
        L10nKey::SettingsHostsDesc => "Недавние хосты. Поиск находит любой сохранённый хост.",
        L10nKey::SettingsAddHost => "Добавить хост",
        L10nKey::SettingsNoHostsMatch => "Нет хостов по запросу «{query}»",
        L10nKey::SettingsHostsFromFiles => "Хостов: {count}, источников: {files}",
        L10nKey::SettingsMoreHosts => "Ещё хостов: {count}",
        L10nKey::SettingsShowRecentOnly => "Только недавние",
        L10nKey::SettingsShowAll => "Показать все",
        L10nKey::SettingsUnsaved => "Не сохранено",
        L10nKey::SettingsNever => "Никогда",
        L10nKey::SettingsDefinedIn => "Задан в",
        L10nKey::SettingsConnectInNewTab => "Подключиться в новой вкладке",
        L10nKey::SettingsCopied => "Скопировано",
        L10nKey::SettingsNavMobile => "Телефон",
        L10nKey::SettingsMobileAccess => "Разрешить доступ с телефона",
        L10nKey::SettingsMobileAccessDesc => {
            "Подключённые телефоны видят панели этого компьютера и могут вводить в них текст, даже если все окна закрыты. Соединения защищены сквозным шифрованием."
        }
        L10nKey::SettingsMobileStatusStarting => "Запуск…",
        L10nKey::SettingsMobileStatusFailed => "Не работает: {error}",
        L10nKey::SettingsMobileStartFailed => "Не удалось включить доступ с телефона: {error}",
        L10nKey::SettingsMobileNoAnswer => {
            "сервер tty7 не включил доступ. Перезапустите сервер в разделе «Настройки → О программе» и повторите попытку."
        }
        L10nKey::SettingsMobilePair => "Подключить телефон",
        L10nKey::SettingsMobileShowCode => "Показать код",
        L10nKey::SettingsMobilePairDesc => {
            "Показывает одноразовый код для приложения tty7 на телефоне."
        }
        L10nKey::SettingsMobilePairNeedsAccess => "Сначала включите доступ с телефона.",
        L10nKey::SettingsMobilePairScan => {
            "Отсканируйте код в приложении tty7 на телефоне или скопируйте его и вставьте там."
        }
        L10nKey::SettingsMobilePairValid => "Действует ещё {time}. Подходит для одного телефона.",
        L10nKey::SettingsMobileNewCode => "Новый код",
        L10nKey::SettingsMobilePairExpired => {
            "Срок действия кода истёк. Создайте новый код для подключения."
        }
        L10nKey::SettingsMobilePairTried => {
            "Код уже вводили, и он больше не действует: возможно, его ввели с ошибкой или прервалась связь. Создайте новый код для подключения."
        }
        L10nKey::SettingsMobilePairReplaced => {
            "Этот код заменён более новым. Создайте новый код для подключения."
        }
        L10nKey::SettingsMobileCopyCode => "Копировать код",
        L10nKey::SettingsMobilePaired => "Подключено к {name}.",
        L10nKey::SettingsMobilePhones => "Подключённые телефоны",
        L10nKey::SettingsMobileNoPhones => "Телефоны ещё не подключены.",
        L10nKey::SettingsMobileUnpair => "Отключить",
        L10nKey::SettingsSearchMobileKeywords => {
            "телефон мобильный айфон андроид айпад подключение код удалённый доступ phone mobile iphone android ipad pair qr code remote access"
        }
        L10nKey::SettingsCopySshCommand => "Копировать команду ssh",
        L10nKey::SettingsStoredInTty7 => "Сохранён в настройках tty7",
        L10nKey::SettingsClickAgainToRemove => "Нажмите ещё раз, чтобы удалить",
        L10nKey::SettingsRemoveHost => "Удалить хост",
        L10nKey::SettingsEditHost => "Изменить",
        L10nKey::SettingsLive => "Подключён",
        L10nKey::SettingsPressKeysShort => "Нажмите клавиши…",
        L10nKey::SettingsSearchAgents => "Поиск агентов",
        L10nKey::SettingsConnectedMachines => "Подключены",
        L10nKey::SettingsInstalledCount => "Установлено: {count}",
        L10nKey::SettingsMachine => "Компьютер",
        L10nKey::SettingsMachineLocalDesc => {
            "Хуки устанавливаются на каждом компьютере отдельно. Удалённые компьютеры видны здесь, пока подключены."
        }
        L10nKey::SettingsMachineRemoteDesc => "Установка через текущее соединение с {name}.",
        L10nKey::SettingsAgentsInstalledSummary => "Установлено {count} из {total}",
        L10nKey::SettingsNoAgentsInstalled => "На этом компьютере ещё нет хуков агентов.",
        L10nKey::SettingsNoAgentsMatch => "Нет агентов по запросу «{query}»",
        L10nKey::SettingsMoreAgents => "Ещё доступно агентов: {count}",
        L10nKey::SettingsShowInstalledOnly => "Только установленные",
        L10nKey::SettingsWorking => "Выполняется…",
        L10nKey::SettingsUpdateAvailable => "Доступно обновление",
        L10nKey::SettingsRevealHookFile => "Показать файл хука",
        L10nKey::SettingsSaveError => "Не удалось сохранить изменения: {error}",
        L10nKey::SettingsRetrySave => "Повторить сохранение",
        L10nKey::SettingsNavGeneral => "Общие",
        L10nKey::SettingsEditShortcuts => "Изменить сочетания…",
        L10nKey::SettingsModifiedOnly => "Только изменённые",
        L10nKey::SettingsModified => "Изменено",
        L10nKey::SettingsResetValue => "Сбросить",
        L10nKey::SettingsSearchResults => "Результаты поиска",
        L10nKey::SettingsOpenSetting => "Открыть настройку",
        L10nKey::SettingsNoModified => "Нет изменённых настроек по этому фильтру.",
        L10nKey::SettingsTerminalFontGroup => "Текст терминала",
        L10nKey::SettingsInterfaceFontGroup => "Текст интерфейса",
        L10nKey::SettingsUnsavedTitle => "Сохранить изменения перед выходом?",
        L10nKey::SettingsUnsavedBody => {
            "Сохранить изменения, отменить их или продолжить редактирование."
        }
        L10nKey::SettingsSaveChanges => "Сохранить",
        L10nKey::SettingsThemeDraft => "Изменения темы показываются до сохранения.",
        L10nKey::SearchTabs => "Поиск вкладок…",
        L10nKey::SearchFiles => "Поиск файлов…",
        L10nKey::PanelSearchPlaceholder => "Поиск в файлах…",
        L10nKey::PanelSearchWholeWord => "Слово целиком",
        L10nKey::PanelSearchIdle => "Поиск в содержимом всех файлов в:",
        L10nKey::PanelSearchNoFolder => "Нет папки для поиска.",
        L10nKey::PanelSearchNoFolderHint => "Поиск выполняется в проекте активной вкладки.",
        L10nKey::PanelSearchSshPane => "Поиск недоступен в файлах SSH-панели.",
        L10nKey::PanelSearchSshPaneHint => {
            "Откройте хост как удалённую рабочую область, чтобы искать в нём, или просматривайте его в разделе «Файлы»."
        }
        L10nKey::PanelSearchSearching => "Поиск…",
        L10nKey::PanelSearchNoMatches => "Нет результатов по запросу «{query}».",
        L10nKey::PanelSearchBadPattern => "Неверное регулярное выражение: {e}",
        L10nKey::PanelSearchServerTooOld => {
            "Версия tty7-server на этом компьютере слишком старая для поиска в содержимом файлов."
        }
        L10nKey::PanelSearchServerTooOldHint => {
            "Обновите сервер на этом хосте, чтобы пользоваться поиском."
        }
        L10nKey::PanelSearchFailed => "Ошибка поиска: {e}",
        L10nKey::PanelSearchResultCount => "Результатов: {count}",
        L10nKey::PanelSearchFileCount => "Файлов: {count}",
        L10nKey::PanelSearchSummary => "{results}, файлов: {files}",
        L10nKey::PanelSearchTruncated => {
            "Показаны не все совпадения. Уточните запрос, чтобы увидеть остальные."
        }
        L10nKey::PanelSearchLineTooltip => "Строка {line}, столбец {column}",
        L10nKey::PanelSearchHostGone => "Компьютер с этим проектом не подключён.",
        L10nKey::SearchThemes => "Поиск тем…",
        L10nKey::SearchSettings => "Поиск настроек…",
        L10nKey::FilterHosts => "Фильтр хостов…",
        L10nKey::SearchTheme => "Поиск…",
        L10nKey::SearchTabAll => "Все",
        L10nKey::SearchTabActions => "Команды",
        L10nKey::SearchTabTerminals => "Терминалы",
        L10nKey::SearchTabHosts => "Хосты",
        L10nKey::SearchTabSymbols => "Символы",
        L10nKey::SearchPlaceholderAll => "Поиск файлов, команд, терминалов и хостов…",
        L10nKey::SearchPlaceholderActions => "Поиск команд…",
        L10nKey::SearchPlaceholderTerminals => "Поиск открытых вкладок, оболочек и агентов…",
        L10nKey::SearchPlaceholderHosts => "Поиск хостов или user@host для подключения…",
        L10nKey::SearchPlaceholderSymbols => "Перейти к символу в этом файле…",
        L10nKey::SearchSymbolsNone => "В файле нет символов",
        L10nKey::SearchSymbolsNoneHint => {
            "Символы доступны для Rust, Go, Python, JavaScript, TypeScript, C, C++, Java, Ruby, оболочки и Markdown."
        }
        L10nKey::SearchTabFiles => "Файлы",
        L10nKey::SearchPlaceholderFiles => {
            "Перейти к файлу по имени. Добавьте :line, чтобы перейти к строке…"
        }
        L10nKey::SearchFilesNoRoots => "Нет проекта для поиска",
        L10nKey::SearchFilesNoRootsHint => {
            "Файлы ищутся в проекте, где находится терминал. Перейдите в него командой cd."
        }
        L10nKey::SearchFilesIndexing => "Индексация файлов…",
        L10nKey::SearchFilesFailed => "Не удалось получить список файлов проекта.",
        L10nKey::SearchFilesGoToLine => "строка {line}",
        L10nKey::SearchFilesCapped => "Большой проект. Поиск только в первых {count} файлах",
        L10nKey::CmdGoToFile => "Перейти к файлу…",
        L10nKey::SearchTabSessions => "Сеансы",
        L10nKey::SearchPlaceholderSessions => "Поиск прошлых сеансов агентов…",
        L10nKey::SearchSessionsEmptyHint => {
            "Здесь показаны прошлые сеансы агентов на этом компьютере."
        }
        L10nKey::SearchSectionSessionsHere => "В {dir}",
        L10nKey::SearchSectionSessionsRecent => "Недавние",
        L10nKey::AppSessionNotResumable => "{name} не поддерживает продолжение сеанса по ID.",
        L10nKey::AppSessionDirectoryGone => "Папка сеанса больше не существует: {path}",
        L10nKey::SearchSessionActions => "Выберите действие с этим сеансом…",
        L10nKey::SessionActionsHint => "действия",
        L10nKey::SessionActionResume => "Продолжить",
        L10nKey::SessionActionResumeSubtitle => "продолжить в новой вкладке в прежней папке",
        L10nKey::SessionActionHide => "Убрать из списка",
        L10nKey::SessionActionHideSubtitle => "история самого агента сохранится",
        L10nKey::SearchMoreIn => "Ещё {count} в {tab}",
        L10nKey::SearchNoResults => "Нет результатов",
        L10nKey::SearchSectionNewTerminal => "Новый терминал",
        L10nKey::Search => "Поиск",
        L10nKey::SearchWorkspacesAndMachines => "Поиск рабочих областей, вкладок и компьютеров…",
        L10nKey::SearchFonts => "Поиск шрифтов…",
        L10nKey::SearchFind => "Найти…",
        L10nKey::SearchMatchCase => "С учётом регистра",
        L10nKey::SearchUseRegex => "Регулярное выражение",
        L10nKey::NewFolderName => "Имя новой папки",
        L10nKey::NewFileName => "Имя нового файла",
        L10nKey::HomeNewTab => "Новая вкладка",
        L10nKey::HomeReopenClosedTab => "Снова открыть закрытую вкладку",
        L10nKey::HomeSwitchWorkspace => "Сменить рабочую область…",
        L10nKey::HomeSearchEverywhere => "Поиск…",
        L10nKey::HomeSplitRight => "Разделить вправо",
        L10nKey::HomeSplitDown => "Разделить вниз",
        L10nKey::HomeSettings => "Настройки…",
        L10nKey::TrayQuitStopServer => "Выйти и остановить сервер…",
        L10nKey::Reconnect => "Переподключиться",
        L10nKey::None => "Нет.",
        L10nKey::TryAgain => "Повторить",
        L10nKey::Refreshing => "обновление…",
        L10nKey::Binary => "двоичный",
        L10nKey::Delete => "Удалить",
        L10nKey::ConnectSshHint => "Чтобы подключиться по SSH, введите user@host.",
        L10nKey::EditHint => "изменить",
        L10nKey::OpenFileFromTree => "Открыть файл из дерева файлов",
        L10nKey::TreeDirLoading => "Чтение…",
        L10nKey::TreeDirEmpty => "Пусто",
        L10nKey::TreeDirHiddenOnly => "Только скрытые файлы",
        L10nKey::TreeDirUnreadable => "Не удалось прочитать",
        L10nKey::TreeSearchCapped => "Первые {n} совпадений",
        L10nKey::TreeSearchFailed => "Ошибка поиска",
        L10nKey::FileChangedOnDisk => "Файл изменён на диске",
        L10nKey::Reload => "Обновить",
        L10nKey::KeepMine => "Оставить мой",
        L10nKey::Dismiss => "Закрыть",
        L10nKey::StoredPasswordRejected => "Сохранённый пароль отклонён. Введите новый.",
        L10nKey::StoredPassphraseRejected => {
            "Сохранённая парольная фраза не подошла к ключу. Введите правильную."
        }
        L10nKey::Trust => "Доверять",
        L10nKey::Abort => "Прервать",
        L10nKey::HostKeyOverrideMessage => {
            "Введите \"yes\", чтобы принять новый ключ, или нажмите Esc для отмены."
        }
        L10nKey::Override => "Принять",
        L10nKey::RememberKeychain => "Запомнить (связка ключей)",
        L10nKey::Cancel => "Отмена",
        L10nKey::Close => "Закрыть",
        L10nKey::QuitStopServerTitle => "Выйти и остановить сервер tty7?",
        L10nKey::QuitStopServerBody => {
            "tty7 закроется, сервер tty7 остановится, все процессы в оболочках завершатся. При следующем запуске вкладки и расположение восстановятся с новыми оболочками. Закрытие окна лишь сворачивает tty7 в трей, оболочки продолжают работать."
        }
        L10nKey::QuitAndStop => "Выйти и остановить",
        L10nKey::CloseSshConnectionTitle => "Закрыть это SSH-соединение?",
        L10nKey::CloseSshConnectionBody => "Соединение активно. Закрытие завершит его.",
        L10nKey::ClosePaneBusyTitle => "Закрыть эту панель?",
        L10nKey::CloseTabBusyTitle => "Закрыть эту вкладку?",
        L10nKey::CloseBusyCommandBody => "{what} ещё работает. Закрытие завершит процесс.",
        L10nKey::CloseBusyAgentBody => "{agent} ещё работает. Закрытие прервёт его ответ.",
        L10nKey::CloseIdleBody => "Оболочка тоже завершится.",
        L10nKey::CloseTabsTitle => "Закрыть {count} вкладок?",
        L10nKey::CloseTabsBody => "Оболочки в них тоже завершатся.",
        L10nKey::Keep => "Оставить",
        L10nKey::SettingsNavAppearance => "Внешний вид",
        L10nKey::SettingsNavTerminal => "Терминал",
        L10nKey::SettingsNavInput => "Клавиатура и мышь",
        L10nKey::SettingsNavSsh => "SSH",
        L10nKey::SettingsNavAgents => "Интеграции",
        L10nKey::SettingsNavKeybindings => "Сочетания клавиш",
        L10nKey::SettingsNavAbout => "О программе",
        L10nKey::SettingsHeader => "НАСТРОЙКИ",
        L10nKey::SettingsWindowTitle => "Настройки",
        L10nKey::Reset => "Сбросить",
        L10nKey::Save => "Сохранить",
        L10nKey::Connect => "Подключиться",
        L10nKey::Download => "Скачать",
        L10nKey::Link => "Связать",
        L10nKey::SettingsThemeIntroTitle => "Тема",
        L10nKey::SettingsThemeIntroDesc => "Каждая тема задаёт светлое или тёмное оформление.",
        L10nKey::SettingsTypography => "Шрифты",
        L10nKey::SettingsFontSize => "Размер шрифта терминала",
        L10nKey::SettingsFontSizeDesc => "Размер текста терминала в пунктах.",
        L10nKey::SettingsUiFontSize => "Размер шрифта интерфейса",
        L10nKey::SettingsUiFontSizeDesc => "Размер текста вкладок, панелей и настроек.",
        L10nKey::SettingsUiFontFamily => "Шрифт интерфейса",
        L10nKey::SettingsUiFontFamilyDesc => "Для вкладок, боковых панелей, диалогов и настроек.",
        L10nKey::SettingsLineHeight => "Высота строки",
        L10nKey::SettingsLineHeightDesc => "Межстрочный интервал относительно размера шрифта.",
        L10nKey::SettingsFontFamily => "Шрифт терминала",
        L10nKey::SettingsFontFamilyDesc => "Выбор из установленных в системе шрифтов.",
        L10nKey::SettingsBoldFont => "Полужирный шрифт",
        L10nKey::SettingsBoldFontDesc => {
            "Полужирное начертание. По умолчанию берётся из основного шрифта."
        }
        L10nKey::SettingsItalicFont => "Курсивный шрифт",
        L10nKey::SettingsItalicFontDesc => {
            "Курсивное начертание. По умолчанию берётся из основного шрифта."
        }
        L10nKey::SettingsFontLigatures => "Лигатуры",
        L10nKey::SettingsFontLigaturesDesc => "Лигатуры для программирования в тексте терминала.",
        L10nKey::SettingsFontThicken => "Утолщение штрихов",
        L10nKey::SettingsFontThickenDesc => "Слегка утолщать текст. Требуется перезапуск.",
        L10nKey::SettingsCursor => "Курсор",
        L10nKey::SettingsCursorShape => "Форма курсора",
        L10nKey::SettingsCursorShapeDesc => "Вид курсора терминала.",
        L10nKey::SettingsPromptCursorShape => "Курсор в приглашении",
        L10nKey::SettingsPromptCursorShapeDesc => "Курсор в приглашении оболочки.",
        L10nKey::SettingsCursorBlink => "Мигание курсора",
        L10nKey::SettingsCursorBlinkDesc => "Мигать курсором, когда терминал в фокусе.",
        L10nKey::SettingsLanguage => "Язык",
        L10nKey::SettingsLanguageDesc => "Язык интерфейса tty7.",
        L10nKey::SettingsLanguageEnglish => "English",
        L10nKey::SettingsLanguageChinese => "简体中文",
        L10nKey::SettingsLanguageJapanese => "日本語",
        L10nKey::SettingsLanguageRussian => "Русский",
        L10nKey::SettingsSearchLanguageKeywords => {
            "язык локаль русский английский китайский японский language locale english chinese"
        }
        L10nKey::SettingsTransparency => "Прозрачность",
        L10nKey::SettingsOpacity => "Непрозрачность",
        L10nKey::SettingsOpacityDesc => "При значении ниже 100 % виден рабочий стол.",
        L10nKey::SettingsBlur => "Размытие",
        L10nKey::SettingsBlurDesc => {
            if cfg!(target_os = "macos") {
                "Размывать фон прозрачного окна."
            } else {
                "Размывать фон прозрачного окна. Нужна поддержка композитора: KDE Plasma её предоставляет, GNOME и обычный X11 оставляют окно прозрачным."
            }
        }
        L10nKey::SettingsBlurAutoDesc => {
            "Размывать фон прозрачного окна. Только для материала «Авто»."
        }
        L10nKey::SettingsBackdrop => "Материал фона",
        L10nKey::SettingsBackdropDesc => "Системный фон прозрачного окна.",
        L10nKey::SettingsSearchBackdropKeywords => {
            "материал фон слюда акрил размытие окно material backdrop mica acrylic blur frosted window background"
        }
        L10nKey::SettingsBackdropAuto => "Авто",
        L10nKey::SettingsBackdropBlur => "Размытие",
        L10nKey::SettingsBackdropMica => "Mica",
        L10nKey::SettingsBackdropMicaAlt => "Mica Alt",
        L10nKey::SettingsBackdropAcrylic => "Acrylic",
        L10nKey::SettingsBackdropOff => "Выкл.",
        L10nKey::FollowTheme => "Как в теме",
        L10nKey::SettingsDimInactivePanes => "Затемнять неактивные панели",
        L10nKey::SettingsDimInactivePanesDesc => "Приглушать неактивные панели, выделяя текущую.",
        L10nKey::SettingsAutoHideTitlebarButtons => "Кнопки заголовка при наведении",
        L10nKey::SettingsAutoHideTitlebarButtonsDesc => {
            "Показывать кнопки новой вкладки и боковой панели лишь при наведении на заголовок."
        }
        L10nKey::SettingsOpenThemesFolder => "Открыть папку тем",
        L10nKey::SettingsChangeThemeImage => "Изменить…",
        L10nKey::SettingsChooseThemeImage => "Выбрать…",
        L10nKey::SettingsRemoveThemeImage => "Удалить",
        L10nKey::SettingsImageOpacity => "Непрозрачность изображения",
        L10nKey::SettingsImageOpacityDesc => "Насколько заметно изображение поверх цвета фона.",
        L10nKey::SettingsEditTheme => "Изменить тему",
        L10nKey::SettingsEditThemeIntro => {
            "Редактируется копия. Изменения сразу видны и сохраняются в её файл."
        }
        L10nKey::SettingsBackgroundImage => "Фоновое изображение",
        L10nKey::SettingsBackgroundImageDesc => "Поверх цвета фона, под текстом.",
        L10nKey::SettingsAnsiColors => "Цвета ANSI",
        L10nKey::SettingsCustomThemes => "Свои темы",
        L10nKey::SettingsThemesRejected => "Не загружены из папки тем",
        L10nKey::ThemeDuplicateFailed => "Не удалось создать копию темы",
        L10nKey::ThemeSaveFailed => "Не удалось сохранить тему",
        L10nKey::OpenInFileManagerFailed => "Не удалось открыть {path}",
        L10nKey::ExplorerMenuOpenIn => "Открыть в tty7",
        L10nKey::ExplorerMenuOpenHere => "Открыть tty7 здесь",
        L10nKey::SettingsCustomThemesIntro => {
            "Создайте копию темы, чтобы изменить её, или положите файл YAML либо .itermcolors в папку тем."
        }
        L10nKey::SettingsDuplicateToEdit => "Создать копию",
        L10nKey::SettingsHosts => "Хосты",
        L10nKey::SettingsDefaults => "По умолчанию",
        L10nKey::SettingsInheritedByEveryHost => "Наследуется всеми хостами",
        L10nKey::SettingsNoSavedHosts => "Сохранённых хостов ещё нет.",
        L10nKey::SettingsNothingMatches => "Нет совпадений для {query}.",
        L10nKey::SettingsInTty7 => "Настройки tty7",
        L10nKey::SettingsImportFromSshConfig => "Импорт из ~/.ssh/config",
        L10nKey::SettingsExpandAllGroups => "Развернуть все группы",
        L10nKey::SettingsNoHostsYet => "Хостов ещё нет",
        L10nKey::SettingsNothingSelected => "Ничего не выбрано",
        L10nKey::SettingsTypeAddressToConnect => {
            "Введите адрес, чтобы подключиться. После этого tty7 предложит сохранить его."
        }
        L10nKey::SettingsMoreInSshConfig => "Ещё {count} в ~/.ssh/config",
        L10nKey::SettingsAliasesLinked => "Связано псевдонимов: {count}.",
        L10nKey::SettingsImportAliases => "Импорт псевдонимов",
        L10nKey::SettingsImportAliasesDesc => "Перечитать файл и добавить новые записи.",
        L10nKey::SettingsImportNow => "Импортировать",
        L10nKey::SettingsImportUnreadable => {
            "Не удалось прочитать {path}. Ничего не импортировано."
        }
        L10nKey::SettingsImportNoHosts => {
            "В {path} нет хостов для импорта, только шаблоны или правила Match."
        }
        L10nKey::SettingsImportSummary => {
            "Добавлено хостов: {count}. Обновлено: {updated}, уже актуальны: {unchanged}"
        }
        L10nKey::SettingsImportIgnored => {
            "В tty7 нет настроек для {count} параметров. Они оставлены в файле: {options}"
        }
        L10nKey::SettingsImportMoreOptions => "ещё +{count}",
        L10nKey::SettingsDefaultsIntro => {
            "Общие настройки всех хостов. Для каждого их можно изменить в разделе «Дополнительно»."
        }
        L10nKey::SettingsCopyAddress => "Копировать адрес",
        L10nKey::SettingsDuplicate => "Создать копию",
        L10nKey::SettingsForgetPassword => "Забыть пароль",
        L10nKey::SettingsForgetPasswordTitle => "Забыть сохранённый пароль для {endpoint}?",
        L10nKey::SettingsForgetPasswordBody => {
            "При следующем подключении потребуется снова ввести пароль. Остальные настройки хоста не изменятся."
        }
        L10nKey::SettingsForgetPasswordSharedBody => {
            "Другие профили ({count}) тоже используют {endpoint}. Для этих подключений также потребуется снова ввести пароль."
        }
        L10nKey::SettingsForgotPasswordFor => "Сохранённый пароль для {endpoint} забыт",
        L10nKey::SettingsDeleteProfileBody => {
            "Сохранённый пароль тоже удалится, если этот адрес не используется другим подключением."
        }
        L10nKey::SettingsDeleteProfileCascade => {
            "Сохранённые удалённые рабочие области ({count}) с адресом {endpoint} тоже удалятся. Сеансы на удалённом компьютере продолжат работать и снова появятся в списке после подключения с новым профилем."
        }
        L10nKey::SettingsCouldntForgetPassword => {
            "Не удалось забыть пароль для {endpoint}: {error}"
        }
        L10nKey::SettingsSecurity => "Безопасность",
        L10nKey::SettingsSecurityIntro => {
            "Для каждого хоста эти настройки можно изменить в разделе «Дополнительно»."
        }
        L10nKey::SettingsVerifyHostKeys => "Проверять ключи хостов",
        L10nKey::SettingsVerifyHostKeysDesc => {
            "Сверять ключи серверов с known_hosts. Без проверки подмена сервера останется незамеченной."
        }
        L10nKey::WarnBeforeClosing => "Предупреждать перед закрытием",
        L10nKey::SettingsWarnBeforeClosingDesc => {
            "Запрашивать подтверждение при закрытии вкладки с активным SSH-сеансом."
        }
        L10nKey::SettingsNewHost => "Новый хост",
        L10nKey::SettingsDiscardChangesTitle => "Отменить несохранённые изменения?",
        L10nKey::SettingsDiscardChangesBody => {
            "В редактируемом подключении есть несохранённые изменения."
        }
        L10nKey::SettingsKeepEditing => "Продолжить редактирование",
        L10nKey::SettingsName => "Имя",
        L10nKey::SettingsHost => "Хост",
        L10nKey::SettingsHostRequired => "Укажите хост, иначе профиль не сохранится.",
        L10nKey::SettingsPortInvalid => {
            "Порт должен быть от 1 до 65535. Если поле пустое, используется 22."
        }
        L10nKey::SettingsUser => "Пользователь",
        L10nKey::SettingsAuth => "Способ входа",
        L10nKey::SettingsAuthDesc => "«Авто» пробует все подходящие способы.",
        L10nKey::SettingsAuthModeAuto => "Авто",
        L10nKey::SettingsAuthModePassword => "Пароль",
        L10nKey::SettingsAuthModeKey => "Ключ",
        L10nKey::SettingsAuthModeAgent => "Агент",
        L10nKey::SettingsAuthMode2Fa => "2FA",
        L10nKey::SettingsNameHint => "Необязательное имя",
        L10nKey::SettingsHostHint => "имя хоста или IP",
        L10nKey::SettingsUserHint => "определяется при подключении",
        L10nKey::SettingsPassword => "Пароль",
        L10nKey::SettingsPasswordDesc => {
            "Хранится в системной связке ключей, а не в файле настроек."
        }
        L10nKey::SettingsPasswordHint => "Запрашивать при входе",
        L10nKey::SettingsKeyPassphrase => "Парольная фраза ключа",
        L10nKey::SettingsKeyPassphraseDesc => {
            "Открывает указанный выше ключ. Хранится в системной связке ключей."
        }
        L10nKey::SettingsPassphraseNeedsKey => {
            "Сначала укажите файл ключа: парольная фраза хранится вместе с ключом, который она открывает."
        }
        L10nKey::SettingsBrowseKey => "Обзор…",
        L10nKey::SettingsCouldntSavePassword => {
            "Не удалось сохранить пароль для {endpoint}: {error}"
        }
        L10nKey::SettingsCouldntSavePassphrase => {
            "Не удалось сохранить парольную фразу для {key}: {error}"
        }
        L10nKey::SettingsJumpHost => "Промежуточный хост",
        L10nKey::SettingsJumpHostDesc => {
            "Профиль, через который идёт подключение. Если поле пустое, подключение прямое."
        }
        L10nKey::SettingsJumpHostUnknown => {
            "Профиля хоста {jump_name} нет, поэтому профиль не сохранится."
        }
        L10nKey::SettingsJumpHostSelf => {
            "Хост не может быть промежуточным для самого себя, поэтому профиль не сохранится."
        }
        L10nKey::SettingsNoneSummary => "(нет)",
        L10nKey::SettingsNoneLower => "нет",
        L10nKey::SettingsPortForwarding => "Проброс портов",
        L10nKey::SettingsRulesOpenedWithConnection => "1 правило, запускается при подключении",
        L10nKey::SettingsAddRule => "+ Добавить правило",
        L10nKey::SettingsRemoveRule => "Удалить правило",
        L10nKey::SettingsFwdLegendLocal => "L: локальный порт ведёт на удалённый компьютер",
        L10nKey::SettingsFwdLegendRemote => "R: удалённый порт ведёт на этот компьютер",
        L10nKey::SettingsFwdLegendDynamic => "D: динамический прокси SOCKS",
        L10nKey::SettingsFwdNeedsBoth => {
            "Укажите порт прослушивания и хост:порт назначения, иначе правило не сохранится."
        }
        L10nKey::SettingsFwdNeedsListen => {
            "Укажите порт прослушивания, иначе правило не сохранится."
        }
        L10nKey::SettingsAdvanced => "Дополнительно",
        L10nKey::SettingsAdvancedSummary => "алгоритмы / keepalive / прокси / X11 / сценарии входа",
        L10nKey::SettingsGroupAuthentication => "Аутентификация",
        L10nKey::SettingsGroupProxies => "Прокси",
        L10nKey::SettingsGroupAlgorithms => "Алгоритмы",
        L10nKey::SettingsGroupConnection => "Соединение",
        L10nKey::SettingsGroupSession => "Сеанс",
        L10nKey::SettingsGroupSecurity => "Безопасность",
        L10nKey::SettingsRemoteClipboardWrite => "Изображения в буфер обмена с хоста",
        L10nKey::SettingsRemoteClipboardWriteDesc => {
            "Разрешить хосту помещать изображения в буфер обмена (OSC 5522)."
        }
        L10nKey::SettingsIdentityFiles => "Файлы ключей",
        L10nKey::SettingsIdentityFilesDesc => {
            "Пути закрытых ключей, по одному на строку (%h/%r подставляются)."
        }
        L10nKey::SettingsAgentForwarding => "Проброс агента",
        L10nKey::SettingsAgentForwardingDesc => "Передавать локальный ssh-agent в соединение.",
        L10nKey::SettingsProxyCommand => "ProxyCommand",
        L10nKey::SettingsProxyCommandDesc => "Команда транспорта (%h/%p/%r подставляются).",
        L10nKey::SettingsSocks5Proxy => "Прокси SOCKS5",
        L10nKey::SettingsSocks5ProxyDesc => "хост:порт. Если поле пустое, прокси не используется.",
        L10nKey::SettingsHttpProxy => "Прокси HTTP",
        L10nKey::SettingsHttpProxyDesc => "хост:порт. Если поле пустое, прокси не используется.",
        L10nKey::SettingsProxyOverridden => "Не используется: {winner} имеет приоритет.",
        L10nKey::SettingsTestConnection => "Проверить",
        L10nKey::SettingsTestRunning => "Проверка соединения…",
        L10nKey::SettingsTestReached => "Подключение и вход выполнены за {time}.",
        L10nKey::SettingsTestNeedsPassword => {
            "Сервер доступен и запросил пароль. Подключитесь, чтобы ввести его."
        }
        L10nKey::SettingsTestNeedsPassphrase => {
            "Сервер доступен, но закрытый ключ требует парольную фразу. Подключитесь, чтобы ввести её."
        }
        L10nKey::SettingsTestNeedsInteractive => {
            "Сервер доступен и запросил интерактивный ответ. Подключитесь, чтобы ответить."
        }
        L10nKey::SettingsTestNeedsHostKey => {
            "Сервер доступен, но его ключ ещё не принят. Подключитесь, чтобы проверить ключ."
        }
        L10nKey::SettingsTestHostKeyChanged => {
            "Сервер доступен, но его ключ изменился. Подключитесь, чтобы проверить изменение."
        }
        L10nKey::SettingsTestFailed => "Не удалось подключиться: {reason}",
        L10nKey::SettingsProxyPortInvalid => {
            "Порт от 1 до 65535. Без порта используется значение по умолчанию."
        }
        L10nKey::SettingsKexAlgorithms => "Алгоритмы KEX",
        L10nKey::SettingsKexAlgorithmsDesc => {
            "Через запятую. Если поле пустое, используются значения библиотеки."
        }
        L10nKey::SettingsCiphers => "Шифры",
        L10nKey::SettingsCiphersDesc => {
            "Через запятую. Если поле пустое, используются значения по умолчанию."
        }
        L10nKey::SettingsMacs => "Алгоритмы MAC",
        L10nKey::SettingsMacsDesc => {
            "Через запятую. Если поле пустое, используются значения по умолчанию."
        }
        L10nKey::SettingsHostKeyAlgorithms => "Алгоритмы ключа хоста",
        L10nKey::SettingsHostKeyAlgorithmsDesc => {
            "Через запятую. Если поле пустое, используются значения по умолчанию."
        }
        L10nKey::SettingsCompression => "Алгоритмы сжатия",
        L10nKey::SettingsJumpHostVia => "через {jump_name}",
        L10nKey::SettingsConnected => "подключён",
        L10nKey::SettingsProfileCopied => "{name} (копия)",
        L10nKey::SettingsCompressionDesc => {
            "Через запятую. Если поле пустое, используются значения по умолчанию."
        }
        L10nKey::SettingsKeepaliveInterval => "Интервал keepalive (с)",
        L10nKey::SettingsKeepaliveIntervalDesc => {
            "Если поле пустое, используется значение библиотеки."
        }
        L10nKey::SettingsKeepaliveCountMax => "Лимит пропусков keepalive",
        L10nKey::SettingsKeepaliveCountMaxDesc => "Число пропущенных проверок до разрыва.",
        L10nKey::SettingsConnectTimeout => "Тайм-аут подключения (с)",
        L10nKey::SettingsConnectTimeoutDesc => {
            "Если поле пустое, используется значение библиотеки."
        }
        L10nKey::SettingsX11Forwarding => "Проброс X11",
        L10nKey::SettingsX11ForwardingDesc => {
            if cfg!(target_os = "macos") {
                "Запрашивать проброс X11 (нужен XQuartz)."
            } else if cfg!(target_os = "windows") {
                "Запрашивать проброс X11 (нужен работающий X-сервер, например VcXsrv или X410)."
            } else {
                "Запрашивать проброс X11."
            }
        }
        L10nKey::SettingsShellIntegration => "Интеграция оболочки",
        L10nKey::SettingsShellIntegrationDesc => {
            "Удалённая оболочка сообщает приглашения, коды выхода и текущую папку."
        }
        L10nKey::SettingsLoginScripts => "Сценарии входа",
        L10nKey::SettingsLoginScriptsDesc => "Команды после запуска оболочки, по одной на строку.",
        L10nKey::SettingsSkipBanner => "Скрывать баннер",
        L10nKey::SettingsSkipBannerDesc => "Не показывать приветствие сервера при входе.",
        L10nKey::SettingsDefaultFollowsDefaults => {
            "По умолчанию используется общее значение: {value}."
        }
        L10nKey::SettingsValueOn => "вкл.",
        L10nKey::SettingsValueOff => "выкл.",
        L10nKey::SettingsDefault => "По умолчанию",
        L10nKey::SettingsOn => "Вкл.",
        L10nKey::SettingsOff => "Выкл.",
        L10nKey::SettingsShell => "Оболочка",
        L10nKey::SettingsShellIntro => {
            "Программа для новых терминалов. Если поле пустое, используется {default}."
        }
        L10nKey::SettingsProgram => "Программа оболочки",
        L10nKey::SettingsProgramDesc => "Имя в PATH или полный путь, например zsh, fish.",
        L10nKey::SettingsArguments => "Аргументы оболочки",
        L10nKey::SettingsArgumentsDesc => "Как в командной строке, например -l или -c \"echo hi\".",
        L10nKey::SettingsArgumentsInvalid => "Непарные кавычки. Значение не сохранено.",
        L10nKey::SettingsStartIn => "Начальная папка",
        L10nKey::SettingsStartInDesc => "Папка запуска, домашняя папка или заданный путь.",
        L10nKey::SettingsCustomPath => "Свой путь",
        L10nKey::SettingsCustomPathDesc => "Папка для запуска новых оболочек.",
        L10nKey::SettingsWdInherit => "Наследовать",
        L10nKey::SettingsWdHome => "Домашняя",
        L10nKey::SettingsWdCustom => "Другая",
        L10nKey::SettingsWdPathInvalid => "Папка не существует. Значение не сохранено.",
        L10nKey::SettingsShellFooter => {
            "Только для оболочек без наследуемой папки, например первой вкладки окна. Новые вкладки и разделённые панели берут папку активной панели."
        }
        L10nKey::SettingsScrolling => "Прокрутка",
        L10nKey::SettingsScrollback => "Буфер прокрутки",
        L10nKey::SettingsScrollbackDesc => {
            "Число строк истории в каждой панели. Для новых панелей."
        }
        L10nKey::SettingsScrollSpeed => "Скорость прокрутки",
        L10nKey::SettingsScrollSpeedDesc => "Множитель скорости прокрутки колёсиком мыши.",
        L10nKey::SettingsSmoothScroll => "Плавная прокрутка",
        L10nKey::SettingsSmoothScrollDesc => {
            "Плавно прокручивать на каждый шаг колёсика. На тачпад не влияет."
        }
        L10nKey::SettingsMouse => "Мышь",
        L10nKey::SettingsFocusFollowsMouse => "Фокус вслед за мышью",
        L10nKey::SettingsFocusFollowsMouseDesc => "Наведение на панель переводит фокус без щелчка.",
        L10nKey::SettingsHideMouseWhileTyping => "Скрывать указатель при вводе",
        L10nKey::SettingsHideMouseWhileTypingDesc => {
            "Скрывать указатель при вводе. Движение мыши возвращает его."
        }
        L10nKey::SettingsMouseZoom => "Масштаб колёсиком",
        L10nKey::SettingsMouseZoomDesc => {
            "Удерживайте клавишу и прокручивайте колёсико, чтобы менять размер шрифта терминала."
        }
        L10nKey::SettingsMouseZoomOff => "Выкл.",
        L10nKey::SettingsReportMouseToApps => "Передавать мышь приложениям",
        L10nKey::SettingsReportMouseToAppsDesc => {
            "Передавать щелчки vim, tmux и другим приложениям. Shift оставляет их в tty7."
        }
        L10nKey::SettingsBell => "Сигнал",
        L10nKey::SettingsTerminalBell => "Сигнал терминала",
        L10nKey::SettingsTerminalBellDesc => "Как показывать сигнал (^G).",
        L10nKey::SettingsLinks => "Ссылки",
        L10nKey::DetectUrls => "Распознавать URL",
        L10nKey::SettingsDetectUrlsDesc => {
            "Подчёркивать ссылки при наведении. {modifier}+щелчок открывает их."
        }
        L10nKey::ForwardSshLoopbackLinks => "Проброс удалённых портов",
        L10nKey::SettingsForwardSshLoopbackLinksDesc => {
            "Открывать здесь localhost-ссылки удалённой панели."
        }
        L10nKey::OpenFilesWith => "Открывать файлы в",
        L10nKey::SettingsOpenFilesWithDesc => {
            "Можно использовать {path}, {line}, {column}. Если поле пустое, файл откроется в приложении по умолчанию."
        }
        L10nKey::SettingsOpenFilesInternal => "Встроенный редактор",
        L10nKey::SettingsOpenFilesSystem => "Приложение по умолчанию",
        L10nKey::SettingsOpenFilesCommand => "Команда",
        L10nKey::SettingsOpenFilesModeDesc => {
            "Что открывать по {modifier}+щелчку на ссылке файла. Только встроенный редактор переходит к строкам и открывает удалённые файлы."
        }
        L10nKey::LinkFileNotUnder => "{path}: такого имени нет в {dir}",
        L10nKey::LinkFileNoDirectory => {
            "{path}: панель ещё не сообщила свою папку, поэтому относительный путь не к чему привязать"
        }
        L10nKey::LinkFileMissing => "{path}: по этому пути ничего нет",
        L10nKey::LinkDirOutsideTree => {
            "{path}: на другом компьютере, вне всех папок, открытых в панели «Файлы»"
        }
        L10nKey::SettingsBellModeOff => "Выкл.",
        L10nKey::SettingsBellModeVisual => "Визуальный",
        L10nKey::SettingsBellModeAudible => "Звуковой",
        L10nKey::SettingsBellModeBoth => "Оба",
        L10nKey::SettingsPrompt => "Приглашение и история команд",
        L10nKey::SettingsPromptIntro => {
            "Редактор и меню tty7 в строке приглашения. Отключите пункт, чтобы вместо него работала оболочка."
        }
        L10nKey::SettingsPromptEditor => "Редактор ввода tty7",
        L10nKey::SettingsPromptEditorDesc => {
            "Выделение, отмена и меню в строке ввода. При отключении управление возвращается ZLE, readline или fish."
        }
        L10nKey::SettingsNeedsPromptEditor => {
            "Нужен редактор ввода. Без него эта клавиша уже принадлежит оболочке."
        }
        L10nKey::SettingsTabCompletion => "Дополнение по Tab",
        L10nKey::SettingsTabCompletionDesc => {
            "Tab открывает меню дополнения tty7. При отключении Tab передаётся оболочке."
        }
        L10nKey::SettingsHistorySearch => "Поиск в истории команд",
        L10nKey::SettingsHistorySearchDesc => {
            "⌃R открывает нечёткий поиск по истории tty7. При отключении ⌃R передаётся оболочке."
        }
        L10nKey::SettingsSelectionClipboard => "Выделение и буфер обмена",
        L10nKey::SettingsSmartSelection => "Умное выделение",
        L10nKey::SettingsSmartSelectionDesc => {
            "Двойной щелчок выделяет URL, путь или пару скобок целиком."
        }
        L10nKey::SettingsCopyOnSelect => "Копировать при выделении",
        L10nKey::SettingsCopyOnSelectDesc => {
            if cfg!(target_os = "macos") {
                "Выделение мышью сразу копирует текст в буфер обмена. ⌘C не требуется."
            } else {
                "Выделение мышью сразу копирует текст в буфер обмена. Ctrl+Shift+C не требуется."
            }
        }
        L10nKey::SettingsTrimTrailingSpaces => "Убирать пробелы в конце строк",
        L10nKey::SettingsTrimTrailingSpacesDesc => {
            "Убирать конечные пробелы из каждой копируемой строки."
        }
        L10nKey::SettingsKeyboard => "Клавиатура",
        L10nKey::SettingsOptionAsMeta => "Option (⌥) как Meta",
        L10nKey::SettingsOptionAsMetaDesc => {
            "⌥+клавиша передаёт Meta, например ⌥B перемещает на слово назад."
        }
        L10nKey::SettingsAgentsIntro => "Хуки агентов",
        L10nKey::SettingsAgentsIntroDesc => {
            "Хуки показывают состояние агента во вкладках: работает, ждёт, закончил."
        }
        L10nKey::SettingsReadingAgentConfig => "Чтение настроек агентов на этом компьютере…",
        L10nKey::SettingsStatusNotInstalled => "Не установлен",
        L10nKey::SettingsStatusInstalled => "Установлен",
        L10nKey::SettingsStatusOutdated => "Устарел",
        L10nKey::SettingsInstall => "Установить",
        L10nKey::SettingsReinstall => "Переустановить",
        L10nKey::SettingsUpdate => "Обновить",
        L10nKey::SettingsUninstall => "Удалить",
        L10nKey::SettingsOfflineMachines => {
            "Другие сохранённые компьютеры ({count}) не подключены. Откройте рабочую область на нужном компьютере, чтобы установить там хуки."
        }
        L10nKey::SettingsSyncWithSystem => "Внешний вид",
        L10nKey::SettingsSyncWithSystemDesc => "Разные темы для светлого и тёмного режима системы.",
        L10nKey::SettingsLegiblePalette => "Читаемые яркие цвета",
        L10nKey::SettingsLegiblePaletteDesc => "Исправлять плохо читаемые яркие цвета ANSI.",
        L10nKey::SettingsSearchLegiblePaletteKeywords => {
            "читаемость контраст яркая палитра параметр legible contrast bright palette psreadline parameter readable"
        }
        L10nKey::SettingsChangeTheme => "Сменить тему",
        L10nKey::SettingsThemes => "Темы",
        L10nKey::SettingsThemesCloseTooltip => "Закрыть темы (Esc)",
        L10nKey::SettingsThemePanelManual => "Выберите текущую тему.",
        L10nKey::SettingsThemePanelLight => "Выберите тему для светлого режима.",
        L10nKey::SettingsThemePanelDark => "Выберите тему для тёмного режима.",
        L10nKey::SettingsCustom => "Своя",
        L10nKey::SettingsCustomValue => "Своё ({value})",
        L10nKey::SettingsBuiltIn => "Встроенная",
        L10nKey::SettingsDark => "Тёмная",
        L10nKey::SettingsLight => "Светлая",
        L10nKey::SettingsLightMode => "Светлый режим",
        L10nKey::SettingsDarkMode => "Тёмный режим",
        L10nKey::SettingsActive => "Активная",
        L10nKey::SettingsStartupWindow => "Окно при запуске",
        L10nKey::SettingsStartupWindowDesc => "Состояние окна при запуске tty7.",
        L10nKey::SettingsRememberWindowSize => "Запоминать размер и положение окна",
        L10nKey::SettingsRememberWindowSizeDesc => {
            "Открывать окно там, где оно было при последнем выходе из tty7."
        }
        L10nKey::SettingsRestoreLastLayout => "Восстанавливать расположение",
        L10nKey::SettingsRestoreLastLayoutDesc => {
            "Открывать вкладки, панели и папки прошлого сеанса."
        }
        L10nKey::SettingsShowTrayIcon => "Значок в трее",
        L10nKey::SettingsShowTrayIconDesc => {
            "Уведомляет, когда агент ждёт ввода, и переходит к его панели."
        }
        L10nKey::SettingsTabs => "Вкладки",
        L10nKey::SettingsNewTabPosition => "Место новой вкладки",
        L10nKey::SettingsNewTabPositionDesc => "Куда вставлять новую вкладку.",
        L10nKey::SettingsConfirmClose => "Подтверждать закрытие",
        L10nKey::SettingsConfirmCloseDesc => {
            "Спрашивать перед закрытием вкладки или панели. Для SSH-хостов с предупреждением о закрытии подтверждение запрашивается всегда."
        }
        L10nKey::ConfirmCloseNever => "Никогда",
        L10nKey::ConfirmCloseWhenBusy => "Если что-то запущено",
        L10nKey::ConfirmCloseAlways => "Всегда",
        L10nKey::SettingsTabBarPosition => "Положение вкладок",
        L10nKey::SettingsTabBarPositionDesc => "Полоса сверху или боковая панель слева.",
        L10nKey::SettingsSidebarGrouping => "Автогруппировка",
        L10nKey::SettingsSidebarGroupingDesc => {
            "Группировать незакреплённые вкладки по репозиторию Git, SSH-вкладки по хосту."
        }
        L10nKey::DocumentDock => "Рядом с терминалом",
        L10nKey::DocumentFill => "На всё окно",
        L10nKey::SettingsNotifications => "Уведомления",
        L10nKey::SettingsWindow => "Запуск и восстановление",
        L10nKey::SettingsNotifyOnCommandFinish => "Уведомлять о завершении команды",
        L10nKey::SettingsNotifyOnCommandFinishDesc => {
            "Уведомлять о завершении долгой команды на переднем плане."
        }
        L10nKey::SettingsNotifyThreshold => "Минимальное время команды",
        L10nKey::SettingsNotifyThresholdDesc => {
            "Уведомлять, только если команда работала не меньше указанного времени."
        }
        L10nKey::NotifyModeNever => "Никогда",
        L10nKey::NotifyModeUnfocused => "Когда окно неактивно",
        L10nKey::NotifyModeAlways => "Всегда",
        L10nKey::ProgramNotesDropped => "Другие уведомления этой панели не показаны",
        L10nKey::SettingsStartupNormal => "Обычное",
        L10nKey::SettingsStartupMaximized => "Развёрнутое",
        L10nKey::SettingsStartupFullscreen => "На весь экран",
        L10nKey::SettingsAfterCurrent => "После текущей",
        L10nKey::SettingsAtEnd => "В конце",
        L10nKey::SettingsTop => "Сверху",
        L10nKey::SettingsLeft => "Слева",
        L10nKey::SettingsPreset => "Профиль",
        L10nKey::SettingsPresetDesc => {
            "В tmux команды панелей и вкладок используют префикс (Ctrl-B C)."
        }
        L10nKey::SettingsPrefix => "Префикс",
        L10nKey::SettingsPressKeys => "Нажмите клавиши… · ⌫ снимает сочетание",
        L10nKey::SettingsPauseToSaveEsc => "пауза сохраняет · Esc",
        L10nKey::SettingsKeybindingsIntroDesc => {
            "Нажмите на сочетание и введите новые клавиши. Для последовательности вроде Ctrl-B X нажимайте клавиши подряд. Esc отменяет, Backspace очищает."
        }
        L10nKey::SettingsPrefixNote => {
            "Одиночная клавиша префикса передаётся оболочке примерно через 1 с."
        }
        L10nKey::SettingsRestoreAllDefaults => "Сбросить все сочетания",
        L10nKey::SettingsRestoreAllDefaultsBody => {
            "Все изменённые сочетания вернутся к исходным значениям. Отменить это нельзя."
        }
        L10nKey::KeybindGoToTab => "Перейти к вкладке {n}",
        L10nKey::KeybindGoToWorkspace => "Перейти к рабочей области {n}",
        L10nKey::KeybindInsertNewline => "Вставить новую строку",
        L10nKey::KeybindForkSessionRight => "Форк сеанса вправо",
        L10nKey::KeybindForkSessionLeft => "Форк сеанса влево",
        L10nKey::KeybindForkSessionDown => "Форк сеанса вниз",
        L10nKey::KeybindForkSessionUp => "Форк сеанса вверх",
        L10nKey::SettingsAboutDesc1 => {
            "Терминал для длительной работы. Сеансы живут после закрытия окна, удалённые хосты доступны как локальные, а состояние агентов видно прямо на боковой панели."
        }
        L10nKey::SettingsDefaultTerminal => "Терминал по умолчанию",
        L10nKey::SettingsDefaultTerminalDesc => {
            "Открывать исполняемые файлы Unix, SSH-ссылки и man-страницы в tty7."
        }
        L10nKey::SettingsDefaultTerminalSet => "Сделать терминалом по умолчанию",
        L10nKey::SettingsDefaultTerminalSetSuccess => {
            "tty7 теперь открывает поддерживаемые файлы и ссылки терминала."
        }
        L10nKey::SettingsDefaultTerminalSetFailed => {
            "Не удалось назначить tty7 терминалом по умолчанию: {error}"
        }
        L10nKey::SettingsVersion => "Версия",
        L10nKey::SettingsUpdates => "Обновления",
        L10nKey::SettingsUpdateAndRelaunch => "Обновить и перезапустить",
        L10nKey::SettingsUpdateViewRelease => "Что нового",
        L10nKey::SettingsUpdateChecking => "Проверка обновлений…",
        L10nKey::SettingsUpdateUpToDate => "Установлена последняя версия.",
        L10nKey::SettingsUpdateDownloadingPercent => "Загрузка обновления… {percent}% из {size}",
        L10nKey::SettingsUpdateDownloadingBytes => "Загрузка обновления… {received}",
        L10nKey::SettingsUpdateVerifying => "Проверка загруженного обновления…",
        L10nKey::SettingsUpdateInstalling => "Перезапуск с обновлением…",
        L10nKey::SettingsUpdateCheckNow => "Проверить",
        L10nKey::SettingsUpdateCancel => "Отменить загрузку",
        L10nKey::SettingsUpdateRetry => "Повторить",
        L10nKey::SettingsUpdateDismiss => "Закрыть",
        L10nKey::SettingsUpdateDownloadManually => "Скачать вручную",
        L10nKey::SettingsUpdateFailedTitle => "Не удалось обновить до {version}.",
        L10nKey::SettingsUpdateReady => "Версия {version} скачана и готова к установке.",
        L10nKey::SettingsUpdateReadyNextLaunch => "Она установится при следующем запуске tty7.",
        L10nKey::SettingsUpdateInstallNow => "Установить и перезапустить",
        L10nKey::SettingsUpdateDiscard => "Удалить загрузку",
        L10nKey::SettingsAutoDownload => "Скачивать обновления в фоне",
        L10nKey::SettingsAutoDownloadDesc => {
            "Скачивать обновления в фоне, чтобы для установки хватило перезапуска. Установка всегда требует подтверждения."
        }
        L10nKey::SettingsUpdateChannel => "Канал обновлений",
        L10nKey::SettingsUpdateChannelDesc => {
            "Ночной канал собирается каждую ночь из свежего кода, без тестирования."
        }
        L10nKey::SettingsUpdateChannelStable => "Стабильный",
        L10nKey::SettingsUpdateChannelNightly => "Ночной",
        L10nKey::SettingsDaemonStale => "Сервер tty7 ещё работает на версии {build}.",
        L10nKey::SettingsDaemonStaleDesc => {
            "tty7 обновлён, но панели используют старый сервер. Перезапуск загрузит новую версию и завершит все процессы в панелях."
        }
        L10nKey::SettingsDaemonStaleRestart => "Перезапустить сервер tty7",
        L10nKey::UpdateDialogTitle => "Доступно обновление",
        L10nKey::UpdateDialogDetail => {
            "Доступна версия tty7 {version}, установлена {current}. Установка перезапустит приложение. Сервер tty7 продолжит работать, панели сохранятся."
        }
        L10nKey::UpdateDialogDetailWindows => {
            "Доступна версия tty7 {version}, установлена {current}. Установка перезапустит приложение и сервер tty7. Процессы в панелях завершатся, вкладки и расположение восстановятся с новыми оболочками."
        }
        L10nKey::UpdateDialogDetailManual => {
            "Доступна версия tty7 {version}, установлена {current}. {hint}"
        }
        L10nKey::UpdateDialogCannotSelfUpdate => "Эта копия tty7 не умеет обновляться сама.",
        L10nKey::UpdateDialogLater => "Позже",
        L10nKey::UpdateDialogNextLaunch => "Установить при следующем запуске",
        L10nKey::UpdateDialogNeedsElevation => {
            "tty7 установлен для всех пользователей. Перед установкой Windows один раз запросит права администратора. Сам tty7 работает без повышенных прав."
        }
        L10nKey::SettingsUpdateCheckFailed => "Не удалось проверить обновления: {error}",
        L10nKey::SettingsUpdatePrepareFailed => "Ошибка обновления: {error}",
        L10nKey::SettingsUpdateLaunchFailed => "Не удалось запустить установщик: {error}",
        L10nKey::SettingsUpdateUnsupportedMacos => {
            "Эта копия лежит вне доступного для записи пакета tty7.app и не может заменить себя. Переместите tty7 в «Программы» или обновите со страницы выпуска."
        }
        L10nKey::SettingsUpdateUnsupportedLinux => {
            "В выпуске нет пакета Linux для этой архитектуры. Соберите из исходников или используйте менеджер пакетов."
        }
        L10nKey::SettingsUpdateLinuxPackage => {
            "В Linux обновление выполняется вручную. Скачайте {name} со страницы выпуска или используйте менеджер пакетов."
        }
        L10nKey::SettingsUpdateUnsupportedWindows => {
            "Эта копия не распознана как установка Inno Setup или переносимый ZIP и не может обновиться сама. Обновите её вручную со страницы выпуска."
        }
        L10nKey::SettingsUpdateWindowsAllUsers => {
            "tty7 установлен для всех пользователей. Для замены нужны права администратора, а tty7 сам их не запрашивает. Скачайте и запустите установщик со страницы выпуска."
        }
        L10nKey::SettingsUpdateUnsupportedPlatform => {
            "На этой платформе автоматическая установка недоступна. Откройте страницу выпуска."
        }
        L10nKey::SettingsUpdateMissingPackage => {
            "В выпуске нет пакета {name} для этой установки. Выберите другой пакет на странице выпуска."
        }
        L10nKey::SettingsUpdateMissingChecksums => {
            "В выпуске нет checksums.txt, поэтому tty7 не будет устанавливать его автоматически."
        }
        L10nKey::SettingsVersionAvailable => "Доступна версия {version}.",
        L10nKey::SettingsCheckUpdatesDesc => {
            "Открывает страницу выпуска, если обновление на месте недоступно."
        }
        L10nKey::SettingsCheckUpdatesOnLaunch => "Проверять обновления при запуске",
        L10nKey::SettingsCommandLine => "Командная строка",
        L10nKey::SettingsCommandLineDesc => {
            "Добавить команду tty7 в PATH. Действует со следующего запуска."
        }
        L10nKey::SettingsInstallCliOnPath => "Добавить команду tty7 в PATH",
        L10nKey::SettingsServer => "Сервер tty7",
        L10nKey::SettingsServerDesc => "Поддерживает сеансы терминала в фоне.",
        L10nKey::SettingsRestartServer => "Перезапустить сервер tty7…",
        L10nKey::SettingsAppHttpProxy => "Прокси обновлений",
        L10nKey::SettingsAppHttpProxyDesc => {
            "Для проверки обновлений tty7. Если поле пустое, используется системный прокси."
        }
        L10nKey::SettingsAppHttpProxyInvalid => "Неверный адрес прокси. Значение не сохранено.",
        L10nKey::SettingsAgentClaudeCode => "Claude Code",
        L10nKey::SettingsAgentCodex => "Codex",
        L10nKey::SettingsAgentTraeCode => "TraeCode",
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
        L10nKey::SettingsAgentQoderCLI => "Qoder CLI",
        L10nKey::SettingsAgentCrush => "Crush",
        L10nKey::SettingsAgentCodeBuddy => "CodeBuddy",
        L10nKey::SettingsAgentCursorCli => "Cursor CLI",
        L10nKey::SettingsAgentPrimeAgent => "Prime Agent",
        L10nKey::SettingsAgentAntigravity => "Antigravity",
        L10nKey::SettingsAgentEmpryo => "Empryo",
        L10nKey::SettingsAgentJcode => "jcode",
        L10nKey::SettingsAgentMuse => "Muse Code",
        L10nKey::SettingsMuseManualInstall => "Требуется ручная установка",
        L10nKey::AppAgentHooksMuseManualInstall => "Выполните на удалённом компьютере: {command}",
        L10nKey::SettingsAgentQoderCn => "Qoder CN CLI",
        L10nKey::SettingsSearchAppHttpProxyKeywords => {
            "прокси сеть загрузка обновление proxy http https socks socks5 clash v2ray network download update"
        }
        L10nKey::SettingsSearchAboutKeywords => {
            "версия лицензия авторы сборка обновление проверка version license credits build update check github"
        }
        L10nKey::SettingsSearchAutoDownloadKeywords => {
            "обновление загрузка фон установка лимитное соединение update download background install metered connection"
        }
        L10nKey::SettingsSearchCheckUpdatesOnLaunchKeywords => {
            "обновление проверка запуск автоматически выпуск update check launch startup automatic release"
        }
        L10nKey::SettingsSearchUpdateChannelKeywords => {
            "обновление канал стабильный ночной выпуск сборка update channel stable nightly release prerelease build"
        }
        L10nKey::SettingsSearchAnsiColorsKeywords => {
            "палитра терминал цвета тема palette 16 terminal colours theme"
        }
        L10nKey::SettingsSearchBackgroundImageKeywords => {
            "фон изображение обои картинка фото тема background image wallpaper picture photo backdrop theme"
        }
        L10nKey::SettingsSearchImageOpacityKeywords => {
            "фон изображение непрозрачность яркость обои background image opacity strength fade wallpaper"
        }
        L10nKey::SettingsSearchArgumentsKeywords => {
            "оболочка флаги вход аргументы shell flags login args"
        }
        L10nKey::SettingsSearchBlurKeywords => {
            "прозрачность размытие окно фон transparency translucent frosted vibrancy window background"
        }
        L10nKey::SettingsSearchBoldFontKeywords => "шрифт начертание жирность typeface weight",
        L10nKey::SettingsSearchClaudeCodeKeywords => {
            "агент интеграция хуки установка удаление состояние сеанс работа ожидание вкладки боковая панель значок agent integration hooks install uninstall status rich session working waiting tab bar sidebar badge claude"
        }
        L10nKey::SettingsSearchCodexKeywords => {
            "агент интеграция хуки установка agent integration hooks install openai codex"
        }
        L10nKey::SettingsSearchTraeCodeKeywords => {
            "агент интеграция хуки установка agent integration hooks install trae code traecli traex"
        }
        L10nKey::SettingsSearchCommandLineToolKeywords => {
            "командная строка утилита оболочка команда установка ссылка терминал агент сценарий command line tool cli tty7 path shell command install symlink terminal iterm agent script"
        }
        L10nKey::SettingsSearchCommandLineToolTitle => "Утилита командной строки",
        L10nKey::SettingsSearchCopilotCliKeywords => {
            "агент интеграция хуки установка agent integration hooks install github copilot"
        }
        L10nKey::SettingsSearchCopyOnSelectKeywords => {
            "буфер обмена выделение мышь clipboard selection yank mouse"
        }
        L10nKey::SettingsSearchCursorBlinkKeywords => "курсор мигание caret blinking flash",
        L10nKey::SettingsSearchCursorShapeKeywords => {
            "курсор блок черта подчёркивание caret block bar underline beam"
        }
        L10nKey::SettingsSearchPromptCursorShapeKeywords => {
            "приглашение курсор форма блок черта оболочка интеграция prompt cursor shape caret block bar underline beam shell integration"
        }
        L10nKey::SettingsSearchCustomThemesKeywords => {
            "тема копия изменение цвета папка импорт фон обои theme duplicate edit colors folder yaml import background image wallpaper"
        }
        L10nKey::SettingsSearchDetectUrlsKeywords => {
            "ссылки открытие links hyperlink clickable open"
        }
        L10nKey::SettingsSearchDimInactivePanesKeywords => {
            "приглушение неактивные панели фокус прозрачность выделение fade unfocused inactive split pane focus opacity highlight active dimming"
        }
        L10nKey::SettingsSearchAutoHideTitlebarButtonsKeywords => {
            "скрывать автоматически заголовок кнопки наведение указатель новая вкладка боковая панель auto hide autohide title bar titlebar buttons chrome hover pointer minimal clean new tab sidebar toggle"
        }
        L10nKey::SettingsSearchDroidKeywords => {
            "агент интеграция хуки установка agent integration hooks install droid factory"
        }
        L10nKey::SettingsSearchFocusFollowsMouseKeywords => {
            "панель наведение активация pane hover activate"
        }
        L10nKey::SettingsSearchFontFamilyKeywords => {
            "шрифт моноширинный типографика typeface monospace typography"
        }
        L10nKey::SettingsSearchFontLigaturesKeywords => {
            "типографика символы лигатуры typography glyph fira"
        }
        L10nKey::SettingsSearchFontThickenKeywords => {
            "шрифт сглаживание утолщение жирность тонкий font smoothing thicken bold weight thin dilation antialiasing AppleFontSmoothing"
        }
        L10nKey::SettingsSearchFontSizeKeywords => {
            "шрифт текст больше меньше масштаб typography text bigger smaller zoom"
        }
        L10nKey::SettingsSearchForwardSshLoopbackLinksKeywords => {
            "удалённый порт туннель проброс ссылки автоопределение ssh remote port tunnel localhost forward links ports autoforward detect"
        }
        L10nKey::SettingsSearchGeminiKeywords => {
            "агент интеграция хуки установка agent integration hooks install gemini google"
        }
        L10nKey::SettingsSearchGooseKeywords => {
            "агент интеграция хуки плагин установка agent integration hooks plugin install goose"
        }
        L10nKey::SettingsSearchGrokBuildKeywords => {
            "агент интеграция хуки установка agent integration hooks install xai grok build"
        }
        L10nKey::SettingsSearchHideMouseWhileTypingKeywords => {
            "курсор указатель автоскрытие cursor pointer autohide"
        }
        L10nKey::SettingsSearchHistorySearchKeywords => {
            "обратный поиск история нечёткий приглашение ctrl-r reverse search fuzzy history recall fzf prompt"
        }
        L10nKey::SettingsSearchHostsKeywords => {
            "хост соединение сохранённый профиль импорт управление добавить изменить быстрое подключение ssh host connection saved profile import ssh_config manage add edit quick connect"
        }
        L10nKey::SettingsSearchItalicFontKeywords => "шрифт наклон курсив typeface oblique",
        L10nKey::SettingsSearchKeybindingsKeywords => {
            "сочетания горячие клавиши клавиатура назначение профиль префикс shortcut hotkey keyboard binding chord tmux preset rebind prefix"
        }
        L10nKey::SettingsSearchKeybindingsTitle => "Сочетания клавиш",
        L10nKey::SettingsSearchKimiCodeKeywords => {
            "агент интеграция хуки установка agent integration hooks install kimi code kimi-code moonshot"
        }
        L10nKey::SettingsSearchLineHeightKeywords => {
            "шрифт интервал строки typography leading spacing"
        }
        L10nKey::SettingsSearchUiFontFamilyKeywords => {
            "интерфейс шрифт семейство боковая панель вкладка interface font family ui typeface typography chrome sidebar tab"
        }
        L10nKey::SettingsSearchNewTabPositionKeywords => {
            "вкладки порядок конец после текущей tabs order end after current"
        }
        L10nKey::SettingsSearchConfirmCloseKeywords => {
            "подтверждение закрытие вкладка панель запрос предупреждение занят работает всегда никогда confirm close closing tab pane ask prompt warn busy running idle always never"
        }
        L10nKey::SettingsSearchNotifyOnCommandFinishKeywords => {
            "уведомление завершение рабочий стол баннер долгая команда notification alert done osc desktop banner long command"
        }
        L10nKey::SettingsSearchNotifyThresholdKeywords => {
            "уведомление секунды длительность долгая команда задержка notification alert seconds duration long command delay"
        }
        L10nKey::SettingsSearchOhMyPiKeywords => {
            "агент интеграция расширение установка agent integration extension install omp oh my pi"
        }
        L10nKey::SettingsSearchOpacityKeywords => {
            "прозрачность окно альфа transparency translucent see through window alpha"
        }
        L10nKey::SettingsSearchOpenFilesWithKeywords => {
            "ссылки файл редактор команда внешнее приложение путь строка столбец links file editor command external app path line column"
        }
        L10nKey::SettingsSearchOpencodeKeywords => {
            "агент интеграция плагин установка agent integration plugin install opencode"
        }
        L10nKey::SettingsSearchOptionAsMetaKeywords => {
            "клавиатура модификатор alt keyboard modifier escape macos option meta option acts as meta"
        }
        L10nKey::SettingsSearchPiKeywords => {
            "агент интеграция расширение установка agent integration extension install pi"
        }
        L10nKey::SettingsSearchPortForwardingKeywords => {
            "туннель локальный удалённый динамический проброс правило ssh tunnel local remote dynamic socks forward rule"
        }
        L10nKey::SettingsSearchProgramKeywords => {
            "оболочка программа запуск shell binary zsh bash fish nu nushell pwsh powershell executable launch"
        }
        L10nKey::SettingsSearchQwenCodeKeywords => {
            "агент интеграция хуки установка agent integration hooks install qwen code qwen-code"
        }
        L10nKey::SettingsSearchQoderCLIKeywords => {
            "агент интеграция хуки установка agent integration hooks install qoder qodercli"
        }
        L10nKey::SettingsSearchCrushKeywords => {
            "агент интеграция хуки установка agent integration hooks install crush"
        }
        L10nKey::SettingsSearchCodeBuddyKeywords => {
            "агент интеграция хуки установка agent integration hooks install codebuddy codebuddy-code cbc tencent"
        }
        L10nKey::SettingsSearchCursorCliKeywords => {
            "агент интеграция хуки установка agent integration hooks install cursor cursor-agent"
        }
        L10nKey::SettingsSearchPrimeAgentKeywords => {
            "агент интеграция расширение установка agent integration extension install prime prime-agent primeintellect"
        }
        L10nKey::SettingsSearchAntigravityKeywords => {
            "агент интеграция хуки установка agent integration hooks install antigravity agy google"
        }
        L10nKey::SettingsSearchEmpryoKeywords => {
            "агент интеграция хуки установка empryo agent integration hooks install"
        }
        L10nKey::SettingsSearchJcodeKeywords => {
            "агент интеграция хуки установка jcode agent integration hooks install"
        }
        L10nKey::SettingsSearchMuseKeywords => {
            "агент интеграция хуки плагины установка muse meta agent integration hooks plugins install"
        }
        L10nKey::SettingsSearchQoderCnKeywords => {
            "агент интеграция хуки установка китайский agent integration hooks install qodercn qoderclicn qoder-cn qoder china"
        }
        L10nKey::SettingsSearchRememberWindowSizeKeywords => {
            "окно размер положение геометрия запуск запомнить window size position bounds geometry launch startup remember"
        }
        L10nKey::SettingsSearchReportMouseToAppsKeywords => {
            "мышь передача щелчок прокрутка mouse reporting vim tmux click scroll shift passthrough"
        }
        L10nKey::SettingsSearchRestoreLastLayoutKeywords => {
            "восстановить сеанс прошлые вкладки панели запуск расположение restore session previous tabs splits reopen launch startup layout"
        }
        L10nKey::SettingsSearchScrollSpeedKeywords => {
            "мышь колёсико множитель прокрутка mouse wheel multiplier scrolling"
        }
        L10nKey::SettingsSearchSmoothScrollKeywords => {
            "плавная анимация колёсико тачпад прокрутка smooth animation ease wheel notch trackpad scrolling"
        }
        L10nKey::SettingsSearchScrollbackKeywords => {
            "история буфер строки прокрутка history buffer lines scroll"
        }
        L10nKey::SettingsSearchShowTrayIconKeywords => {
            "трей меню состояние агент внимание системный значок tray menu bar status item agent attention system icon"
        }
        L10nKey::SettingsSearchSidebarGroupingKeywords => {
            "вкладки группы автоматически репозиторий хост закрепить разгруппировать заголовок боковая панель папка tabs group grouping auto repo repository git ssh host pinned pin ungrouped header sidebar flat folder"
        }
        L10nKey::SettingsSearchSmartSelectionKeywords => {
            "двойной щелчок слово путь выделение скобки почта double click word url path select semantic bracket email"
        }
        L10nKey::SettingsSearchStartInKeywords => {
            "текущая папка путь домашняя наследовать другая cwd working directory start folder path home inherit custom"
        }
        L10nKey::SettingsSearchSyncWithSystemKeywords => {
            "тема тёмная светлая автоматически система оформление режим theme dark light auto follow os appearance mode"
        }
        L10nKey::SettingsSearchPromptEditorKeywords => {
            "приглашение редактор оболочка ввод строка сочетания вставка prompt editor native shell input line editor zle readline fish keybindings ime paste"
        }
        L10nKey::SettingsSearchTabBarPositionKeywords => {
            "вкладки вертикально боковая панель слева сверху расположение tabs vertical sidebar left top layout rail"
        }
        L10nKey::SettingsSearchTabCompletionKeywords => {
            "дополнение меню предложения приглашение complete completion menu suggestions tab prompt"
        }
        L10nKey::SettingsSearchTerminalBellKeywords => {
            "сигнал звуковой визуальный вспышка звук тишина bell audible visual flash sound silence beep both ^g"
        }
        L10nKey::SettingsSearchThemeKeywords => {
            "оформление цвет схема тёмная светлая палитра фон акцент система автоматически appearance color colours scheme dark light palette background foreground accent sync system os auto follow"
        }
        L10nKey::SettingsSearchTrimTrailingSpacesKeywords => {
            "буфер обмена пробелы копировать clipboard whitespace copy"
        }
        L10nKey::SettingsSearchVerifyHostKeysKeywords => {
            "безопасность отпечаток ключ хост проверка ssh security known_hosts fingerprint mitm host key verification"
        }
        L10nKey::SettingsSearchWarnBeforeClosingKeywords => {
            "подтверждение закрытие вкладка панель активный сеанс безопасность ssh confirm close tab pane live session security"
        }
        L10nKey::SettingsSearchStartupWindowKeywords => {
            "запуск открыть развёрнуто на весь экран обычное launch open maximized fullscreen normal"
        }
        L10nKey::SwitcherNoMatch => "Рабочие области и компьютеры не найдены.",
        L10nKey::AddSshHost => "Добавить SSH-хост…",
        L10nKey::RestartServer => "Перезапустить сервер tty7",
        L10nKey::OtherMachines => "Другие компьютеры",
        L10nKey::Ok => "OK",
        L10nKey::SftpNoTransfers => "Передач ещё нет.",
        L10nKey::SftpPanelTitleFiles => "Файлы",
        L10nKey::SftpTooltipRefresh => "Обновить",
        L10nKey::SftpTooltipMore => "Ещё",
        L10nKey::SftpMenuNewFolder => "Новая папка",
        L10nKey::SftpMenuNewFile => "Новый файл",
        L10nKey::SftpMenuUpload => "Загрузить…",
        L10nKey::SftpMenuGotoShellCwd => "Перейти в папку оболочки",
        L10nKey::SftpMenuHideTransferHistory => "Скрыть историю передач",
        L10nKey::SftpMenuTransferHistory => "История передач",
        L10nKey::SftpEditNewFolder => "Новая папка",
        L10nKey::SftpEditNewFile => "Новый файл",
        L10nKey::SftpEditRename => "Переименовать",
        L10nKey::SftpEditPermissions => "Права · {mode}",
        L10nKey::SftpLoading => "Загрузка…",
        L10nKey::SftpEmptyDirectory => "Папка пуста.",
        L10nKey::SftpContextOpen => "Открыть",
        L10nKey::SftpContextEdit => "Изменить",
        L10nKey::SftpContextFollowSymlink => "Перейти по ссылке",
        L10nKey::SftpContextRename => "Переименовать",
        L10nKey::SftpContextChmod => "chmod…",
        L10nKey::SftpTransferSummaryRunning => "Передаётся: {count} · {pct}%",
        L10nKey::SftpTransferSummaryFailed => "С ошибками: {count}",
        L10nKey::SftpTransferSummaryIdle => "Передачи",
        L10nKey::SftpTransferProgress => "{done} / {total} ({pct}%)",
        L10nKey::SftpTransferDone => "готово",
        L10nKey::SftpTransferCancelled => "отменено",
        L10nKey::SftpTransferError => "ошибка",
        L10nKey::SftpTransferListFailed => "Не удалось проверить передачи: {error}",
        L10nKey::SftpPasteUploadFailed => "Не удалось загрузить {name} на {host}: {error}",
        L10nKey::LinkFileOpenFailed => "Не удалось открыть {path}: {error}",
        L10nKey::ForwardDisconnected => "Отключено",
        L10nKey::ForwardDisconnectedFrom => "Отключено от {host}",
        L10nKey::SshEditProfile => "Изменить соединение…",
        L10nKey::ForwardTooltipAdd => "Добавить проброс",
        L10nKey::ForwardTooltipRemove => "Удалить",
        L10nKey::ForwardTooltipTurnOn => "Включить",
        L10nKey::ForwardTooltipTurnOff => "Выключить, сохранив правило",
        L10nKey::ForwardSwitchFailed => "Не удалось переключить проброс: {error}",
        L10nKey::SettingsFwdEnabled => "Запускать правило при подключении",
        L10nKey::ForwardLocal => "Локальный",
        L10nKey::ForwardRemote => "Удалённый",
        L10nKey::ForwardDynamic => "Динамический",
        L10nKey::ForwardBindLabel => "адрес",
        L10nKey::ForwardToLabel => "в",
        L10nKey::ForwardSocksLabel => "SOCKS",
        L10nKey::ForwardAdd => "Добавить",
        L10nKey::ForwardPortLabel => "Удалённый порт",
        L10nKey::ForwardPortHere => "доступен на localhost:{port}",
        L10nKey::ForwardNeedsPort => "Порт: число от 1 до 65535.",
        L10nKey::ForwardAdvancedToggle => "Дополнительно",
        L10nKey::ForwardSimpleToggle => "Простой",
        L10nKey::ForwardRequestFailed => "Сеанс недоступен. Ничего не изменено.",
        L10nKey::FileTreePlaceholderFileName => "имя файла",
        L10nKey::FileTreePlaceholderFolderName => "имя папки",
        L10nKey::FileTreePlaceholderNewName => "новое имя",
        L10nKey::FileTreeDeleteTitle => "Удалить «{name}»?",
        L10nKey::FileTreeDeleteFolderBody => {
            "Папка и всё её содержимое будут удалены. Отменить это нельзя."
        }
        L10nKey::FileTreeDeleteFileBody => "Отменить это нельзя.",
        L10nKey::SftpDeleteFolderBody => {
            "Папка и всё содержимое будут удалены на {host}. На удалённой стороне нет корзины."
        }
        L10nKey::SftpDeleteFileBody => {
            "Файл будет удалён на {host}. На удалённой стороне нет корзины."
        }
        L10nKey::FileTreeDeleteFailed => "Не удалось удалить {name}",
        L10nKey::FileTreeCreateFailed => "Не удалось создать {name}",
        L10nKey::FileTreeRenameFailed => "Не удалось переименовать {name}",
        L10nKey::FileTreeDownloadFailed => "Не удалось скачать {name}",
        L10nKey::FileTreeDownloaded => "Скачано в {path}",
        L10nKey::FileTreeDownloadTooLarge => "Больше {limit} МБ. Скачайте через scp или rsync.",
        L10nKey::FileTreeContextOpen => "Открыть",
        L10nKey::FileTreeContextCdHere => "Перейти сюда (cd)",
        L10nKey::FileTreeContextPinAsGroup => "Закрепить как группу",
        L10nKey::FileTreeContextInsertPath => "Вставить путь в терминал",
        L10nKey::FileTreeContextAttachAgent => "Прикрепить к агенту",
        L10nKey::FileTreeContextNewFile => "Новый файл",
        L10nKey::FileTreeContextNewFolder => "Новая папка",
        L10nKey::FileTreeContextRename => "Переименовать",
        L10nKey::FileTreeContextCopyPath => "Копировать путь",
        L10nKey::FileTreeContextHideDotfiles => "Не показывать скрытые файлы",
        L10nKey::FileTreeContextShowDotfiles => "Показать скрытые файлы",
        L10nKey::FileDropIntoItself => "Нельзя скопировать папку в саму себя.",
        L10nKey::FileDropNotHere => "Не на этом компьютере.",
        L10nKey::FileDropNameTaken => {
            "Среди перетаскиваемых объектов уже есть объект с таким именем."
        }
        L10nKey::FileDropTooDeep => "Вложенность больше {n} папок.",
        L10nKey::FileDropTooLarge => "Больше {limit} МБ. Передайте через SFTP.",
        L10nKey::FileDropNoWorkingName => "Рядом нет свободного имени для временной копии.",
        L10nKey::FileDropLeftAside => {
            "Не удалось поместить копию на место. Прежний объект теперь называется «{name}» и находится в той же папке."
        }
        L10nKey::FileDropReplaceTitle => "Заменить «{name}»?",
        L10nKey::FileDropReplaceManyTitle => "Заменить объекты ({n})?",
        L10nKey::FileDropReplaceBody => {
            "В этой папке уже есть объект с таким именем. Отменить замену нельзя."
        }
        L10nKey::FileDropReplace => "Заменить",
        L10nKey::FileDropFailed => "Не удалось скопировать {name}",
        L10nKey::FileDropFailedMany => "Не удалось скопировать {name} и другие объекты ({n})",
        L10nKey::SshPromptNewKey => "новый {fingerprint}",
        L10nKey::SshPromptOldKey => "старый {old_fingerprint}",
        L10nKey::SshPromptHostKeyNewAlgorithm => {
            "Этот хост уже известен по ключу {previous_algorithm}. Это новый ключ {algorithm}, а не замена прежнего."
        }
        L10nKey::SshPromptTypeYesToOverride => "Введите \"yes\", чтобы разрешить замену.",
        L10nKey::EditorCantOpen => "Не удалось открыть {path}: {e}",
        L10nKey::EditorCantRead => "Не удалось прочитать {path}: {e}",
        L10nKey::EditorNotUtf8 => "«{path}» содержит некорректный UTF-8",
        L10nKey::EditorSaveFailed => "Не удалось сохранить {name}",
        L10nKey::EditorUnsavedChanges => "В «{name}» есть несохранённые изменения",
        L10nKey::EditorDiscard => "Отменить",
        L10nKey::EditorNoFileOpen => "Нет открытого файла",
        L10nKey::EditorStripSearch => "Поиск в открытых файлах: {n}",
        L10nKey::EditorStripAllFiles => "Все открытые файлы",
        L10nKey::EditorStripHidden => "Скрыто · {n}",
        L10nKey::EditorStripInBar => "На полосе вкладок",
        L10nKey::EditorStripNoMatch => "Открытые файлы не найдены",
        L10nKey::EditorStripCloseSaved => "Закрыть сохранённые",
        L10nKey::EditorStripCloseOthers => "Закрыть остальные",
        L10nKey::EditorBackToTerminal => "Вернуться в терминал (Esc)",
        L10nKey::EditorLnCol => "Стр. {line}, стлб. {column}",
        L10nKey::EditorSelections => "(выделений: {n})",
        L10nKey::EditorEdit => "Редактирование",
        L10nKey::EditorPreview => "Просмотр",
        L10nKey::EditorWrapOn => "Перенос: вкл.",
        L10nKey::EditorWrapOff => "Перенос: выкл.",
        L10nKey::EditorFileTooLarge => "«{path}» слишком велик для редактора ({size} МБ)",
        L10nKey::EditorBinaryFile => "«{path}» похож на двоичный файл",
        L10nKey::EditorUntitled => "Без имени-{n}",
        L10nKey::EditorUnsavedChangesMany => "Файлов с несохранёнными изменениями: {count}",
        L10nKey::EditorSaveAll => "Сохранить все",
        L10nKey::EditorSaveConflictTitle => "«{name}» изменён на диске",
        L10nKey::EditorSaveConflictBody => {
            "Другая программа изменила файл после открытия. Перезапись заменит те изменения текущими."
        }
        L10nKey::EditorOverwrite => "Перезаписать",
        L10nKey::EditorEncodeFailedTitle => "Не удаётся сохранить «{name}» в {encoding}",
        L10nKey::EditorEncodeFailedBody => {
            "Символ «{ch}» отсутствует в {encoding}. Сохранить в UTF-8?"
        }
        L10nKey::EditorSaveAsUtf8 => "Сохранить в UTF-8",
        L10nKey::EditorAlreadyOpen => "«{path}» уже открыт в редакторе",
        L10nKey::EditorGoToLine => "Перейти к строке",
        L10nKey::EditorGoToLineAction => "Перейти к строке…",
        L10nKey::EditorGoToMatchingBracket => "Перейти к парной скобке",
        L10nKey::EditorToggleComment => "Переключить комментарий",
        L10nKey::EditorMoveLineUp => "Переместить строку вверх",
        L10nKey::EditorMoveLineDown => "Переместить строку вниз",
        L10nKey::EditorDuplicateLine => "Дублировать строку",
        L10nKey::EditorDeleteLine => "Удалить строку",
        L10nKey::EditorCopyRelativePath => "Копировать относительный путь",
        L10nKey::EditorGitNextChange => "Перейти к следующему изменению",
        L10nKey::EditorGitPrevChange => "Перейти к предыдущему изменению",
        L10nKey::EditorGitRevertChange => "Отменить изменение",
        L10nKey::EditorGitToggleGutter => "Показать или скрыть метки изменений Git",
        L10nKey::EditorGitPeekRevert => "Отменить",
        L10nKey::EditorGitPeekSummary => {
            "Строки: −{removed} +{added} относительно версии в индексе"
        }
        L10nKey::EditorGitPeekAddedOnly => "Это новые строки: в версии в индексе здесь ничего нет.",
        L10nKey::EditorGitPeekChange => "Просмотреть изменение",
        L10nKey::EditorGitPeekKeys => "Enter отменяет изменение · Esc закрывает",
        L10nKey::EditorProblemsTitle => "Проблемы",
        L10nKey::EditorProblemsToggle => "Показать/скрыть проблемы",
        L10nKey::EditorProblemsNone => "В открытых файлах нет проблем.",
        L10nKey::EditorProblemsMore => "…и ещё {n}",
        L10nKey::SettingsEditor => "Редактор",
        L10nKey::SettingsEditorGitGutter => "Метки изменений Git",
        L10nKey::SettingsEditorGitGutterDesc => {
            "Отмечать строки, которые отличаются от версии в индексе, рядом с номерами строк и на полосе прокрутки."
        }
        L10nKey::SettingsEditorLsp => "Языковые серверы",
        L10nKey::SettingsEditorLspDesc => {
            "Запускать языковой сервер для диагностики ошибок, автодополнения и перехода к определению. Только для файлов на этом компьютере."
        }
        L10nKey::SettingsEditorSoftWrap => "Переносить длинные строки",
        L10nKey::SettingsEditorSoftWrapDesc => {
            "Открывать файлы с переносом строк. Кнопка «Перенос» в строке состояния меняет его для одного файла."
        }
        L10nKey::SettingsEditorMarkdownPreview => "Просмотр Markdown при открытии",
        L10nKey::SettingsEditorMarkdownPreviewDesc => {
            "Открывать Markdown в режиме просмотра, а не как исходный текст."
        }
        L10nKey::SettingsSearchEditorGitGutterKeywords => {
            "поля различия изменения метки индекс изменено добавлено удалено git gutter diff changes markers staged index scm vcs modified added deleted"
        }
        L10nKey::SettingsSearchEditorLspKeywords => {
            "языковой сервер диагностика ошибки предупреждения дополнение определение lsp language server diagnostics errors warnings completion rust-analyzer definition"
        }
        L10nKey::SettingsSearchEditorSoftWrapKeywords => {
            "перенос длинные строки редактор wrap soft wrap long lines editor"
        }
        L10nKey::SettingsSearchEditorMarkdownPreviewKeywords => {
            "просмотр разметка markdown preview rendered md readme"
        }
        L10nKey::EditorGoToLinePlaceholder => "Строка или строка:столбец (1-{total})",
        L10nKey::EditorGoToSymbolAction => "Перейти к символу в редакторе…",
        L10nKey::EditorNavigateBack => "Назад",
        L10nKey::EditorNavigateForward => "Вперёд",
        L10nKey::EditorSplitRight => "Разделить редактор вправо",
        L10nKey::EditorFocusLeftGroup => "Перейти к левой группе редактора",
        L10nKey::EditorFocusRightGroup => "Перейти к правой группе редактора",
        L10nKey::EditorSplitSameFile => {
            "Открыт в другой группе. Нажмите, чтобы редактировать здесь"
        }
        L10nKey::CmdEditorGoToSymbol => "Редактор: перейти к символу…",
        L10nKey::SearchHeadingReferences => "Ссылки",
        L10nKey::SearchSectionThisFile => "В этом файле",
        L10nKey::SearchSectionProject => "Проект",
        L10nKey::SearchHeadingDefinitions => "Определения",
        L10nKey::CmdEditorGoBack => "Редактор: назад",
        L10nKey::CmdEditorGoForward => "Редактор: вперёд",
        L10nKey::CmdEditorSplitRight => "Редактор: разделить вправо",
        L10nKey::EditorSaveAs => "Сохранить как",
        L10nKey::EditorSaveAsAction => "Сохранить как…",
        L10nKey::EditorSaveAsPlaceholder => "Полный путь для сохранения",
        L10nKey::EditorReplaceExisting => "«{path}» уже существует. Заменить?",
        L10nKey::EditorReplace => "Заменить",
        L10nKey::EditorNewFile => "Новый файл",
        L10nKey::EditorOrphanAdopted => "Несохранённый «{name}» перенесён сюда из закрытой вкладки",
        L10nKey::EditorFileDeletedOnDisk => "Файл удалён на диске",
        L10nKey::EditorIndentSpaces => "Пробелы: {n}",
        L10nKey::EditorIndentTabs => "Размер табуляции: {n}",
        L10nKey::LspGoToDefinition => "Перейти к определению",
        L10nKey::LspQuickFix => "Быстрое исправление…",
        L10nKey::LspFormatDocument => "Форматировать документ",
        L10nKey::LspRenameSymbol => "Переименовать символ",
        L10nKey::LspRenameSymbolAction => "Переименовать символ…",
        L10nKey::LspRenamePlaceholder => "Новое имя для {name}",
        L10nKey::LspRenameFailed => "Не удалось переименовать {name}",
        L10nKey::LspServerMissing => "{name} не установлен",
        L10nKey::LspServerStarting => "{name} запускается…",
        L10nKey::LspServerDown => "{name} остановлен",
        L10nKey::LspProblemsTooltip => "Ошибок: {errors}, предупреждений: {warnings}",
        L10nKey::SearchTabLocations => "Места",
        L10nKey::SearchPlaceholderLocations => "Фильтр найденных мест…",
        L10nKey::SearchLocationsNone => "Ничего не найдено",
        L10nKey::LspFindReferences => "Найти все ссылки",
        L10nKey::PanelInfoTitle => "Сведения",
        L10nKey::PanelChangesTitle => "Изменения",
        L10nKey::PanelScmTitle => "Изменения",
        L10nKey::PanelFilesTitle => "Файлы",
        L10nKey::PanelSearchTitle => "Поиск",
        L10nKey::PanelGitHubTitle => "GitHub",
        L10nKey::PanelNoSession => "Нет активного сеанса.",
        L10nKey::PanelNoSessionHint => {
            "Откройте вкладку, чтобы увидеть здесь её оболочку, папку и процессы."
        }
        L10nKey::PanelNoWorkingDirectory => "Нет рабочей папки.",
        L10nKey::PanelNoWorkingDirectoryHint => "Панель ещё не сообщила свою папку.",
        L10nKey::PanelLoading => "Загрузка…",
        L10nKey::PanelNotAGitRepo => "Это не репозиторий Git.",
        L10nKey::PanelNotAGitRepoHint => {
            "Перейдите в репозиторий командой cd, и здесь появятся незакоммиченные изменения."
        }
        L10nKey::PanelNoChanges => "Нет незакоммиченных изменений.",
        L10nKey::PanelNoChangesHint => "Рабочее дерево чистое.",
        L10nKey::PanelMoreChangedFiles => {
            "…и ещё изменённые файлы ({count}). Посмотрите их командой git diff."
        }
        L10nKey::PanelSessionSubtitle => "Сеанс",
        L10nKey::PanelProcessesSubtitle => "Процессы",
        L10nKey::PanelProcessesTotal => "Всего",
        L10nKey::PanelPortsSubtitle => "Порты",
        L10nKey::PanelPortsUnsupported => {
            "Версия tty7-server на этом компьютере слишком старая для списка портов."
        }
        L10nKey::PanelPortsProbeFailed => "Не удалось проверить порты этой панели.",
        L10nKey::PanelPortsRestricted => {
            "Здесь есть процессы другого пользователя. Их порты не видны."
        }
        L10nKey::PanelPortsEmpty => "Нет проброшенных портов",
        L10nKey::PanelLatency => "задержка",
        L10nKey::PortAutoForwarded => "Удалённый :{port} доступен на http://localhost:{local}",
        L10nKey::PanelCwd => "папка",
        L10nKey::PanelShell => "оболочка",
        L10nKey::PanelSsh => "ssh",
        L10nKey::PanelBranch => "ветка",
        L10nKey::PanelChangesRow => "изменения",
        L10nKey::PanelAgentWorking => "работает",
        L10nKey::PanelAgentWaiting => "ждёт",
        L10nKey::PanelAgentDone => "готово",
        L10nKey::PanelRevealInFinder => "Показать в Finder",
        L10nKey::PanelOpenFolder => "Открыть папку",
        L10nKey::PanelOpenInBrowser => "Открыть в браузере",
        L10nKey::ScmGroupMerge => "Изменения слияния",
        L10nKey::ScmGroupStaged => "Изменения в индексе",
        L10nKey::ScmGroupChanges => "Изменения",
        L10nKey::ScmGroupUntracked => "Неотслеживаемые",
        L10nKey::ScmCommitPlaceholder => "Что изменилось…",
        L10nKey::ScmCommitButton => "Коммит",
        L10nKey::ScmCommitAllButton => "Коммит всего",
        L10nKey::ScmCommitAmendButton => "Коммит (изменить)",
        L10nKey::ScmCommitAndPush => "Коммит и отправка",
        L10nKey::ScmCommitAndSync => "Коммит и синхронизация",
        L10nKey::ScmAmendLastCommit => "Изменить последний коммит",
        L10nKey::ScmCommitStaged => "Коммит из индекса",
        L10nKey::ScmStashAll => "Спрятать всё",
        L10nKey::ScmNothingToCommit => "Нет изменений для коммита",
        L10nKey::ScmNetworkBusy => "Для этого репозитория ещё выполняется другая сетевая операция",
        L10nKey::ScmCommitNeedsMessage => "Сначала напишите сообщение коммита",
        L10nKey::ScmDetailFilesFailed => "Не удалось прочитать список файлов",
        L10nKey::ScmTimeNow => "сейчас",
        L10nKey::ScmTimeMinutes => "{n} мин",
        L10nKey::ScmTimeHours => "{n} ч",
        L10nKey::ScmTimeDays => "{n} дн.",
        L10nKey::ScmTimeMonths => "{n} мес.",
        L10nKey::ScmTimeYears => "{n} г.",
        L10nKey::ScmResetHardConfirm => {
            "Сбросить ветку до этого коммита? Более поздние коммиты исчезнут из ветки, незакоммиченные изменения будут удалены."
        }
        L10nKey::ScmReset => "Сбросить",
        L10nKey::ScmChipStaged => "В ИНДЕКСЕ",
        L10nKey::ScmStage => "Добавить в индекс",
        L10nKey::ScmStageAll => "Добавить всё в индекс",
        L10nKey::ScmUnstage => "Убрать из индекса",
        L10nKey::ScmUnstageAll => "Очистить индекс",
        L10nKey::ScmDiscard => "Отменить изменения",
        L10nKey::ScmDiscardAll => "Отменить все изменения",
        L10nKey::ScmDiscardConfirm => "Отменить изменения в {path}? Вернуть их будет нельзя.",
        L10nKey::ScmOpenConflict => "Разрешить конфликт",
        L10nKey::ScmMarkResolved => "Отметить как разрешённый",
        L10nKey::ScmUnrepresentablePath => {
            "Путь содержит некорректный UTF-8, Git не может с ним работать. Только чтение."
        }
        L10nKey::ScmPublishBranch => "Опубликовать ветку",
        L10nKey::ScmDetached => "отсоединён",
        L10nKey::ScmPushDetached => "HEAD отсоединён. Перейдите на ветку, чтобы отправить",
        L10nKey::ScmPushNoCommits => "Ещё нет коммитов для отправки",
        L10nKey::ScmAmendBadge => "правка коммита",
        L10nKey::ScmSync => "Синхронизировать",
        L10nKey::ScmPush => "Отправить",
        L10nKey::ScmPull => "Получить",
        L10nKey::ScmFetch => "Извлечь",
        L10nKey::ScmCheckoutBranch => "Перейти на…",
        L10nKey::ScmCreateBranch => "Создать ветку…",
        L10nKey::ScmSearchBranches => "Поиск веток…",
        L10nKey::ScmStashAndSwitch => "Спрятать и перейти",
        L10nKey::ScmGraphTitle => "История",
        L10nKey::ScmGraphLoadMore => "Загрузить ещё",
        L10nKey::ScmGraphFilterPlaceholder => "Фильтр коммитов…",
        L10nKey::ScmGraphAllBranches => "Все ветки",
        L10nKey::ScmGraphEmpty => "Коммитов ещё нет",
        L10nKey::ScmGraphCurrentBranch => "Текущая ветка",
        L10nKey::ScmCheckoutCommit => "Перейти на коммит",
        L10nKey::ScmCreateBranchHere => "Создать ветку здесь…",
        L10nKey::ScmResetSoft => "Сбросить (Soft)",
        L10nKey::ScmResetMixed => "Сбросить (Mixed)",
        L10nKey::ScmResetHard => "Сбросить (Hard)",
        L10nKey::ScmCommitDetailTitle => "Коммит",
        L10nKey::ScmCopyCommitSha => "Копировать SHA коммита",
        L10nKey::ScmCherryPick => "Перенести коммит",
        L10nKey::ScmRevertCommit => "Отменить коммит",
        L10nKey::ScmResetToCommit => "Сбросить до коммита",
        L10nKey::ScmRefresh => "Обновить",
        L10nKey::ScmBackToChanges => "Назад",
        L10nKey::ScmCommitParents => "Родительские коммиты",
        L10nKey::ScmShowMore => "Показать больше",
        L10nKey::ScmShowLess => "Показать меньше",
        L10nKey::ScmCommitNotFound => "В этом репозитории нет такого коммита.",
        L10nKey::ScmTooManyChanges => "Показаны первые {shown} из {total} изменений.",
        L10nKey::ScmFilterChanges => "Фильтр изменённых файлов…",
        L10nKey::ScmViewAsTree => "Показать в виде дерева",
        L10nKey::ScmViewAsList => "Показать в виде списка",
        L10nKey::ScmNoMatchingChanges => "Нет изменённых файлов по фильтру.",
        L10nKey::ScmOpenChanges => "Открыть изменения",
        L10nKey::ScmDiscardAllConfirm => {
            "Отменить все изменения вне индекса и удалить неотслеживаемые файлы? Изменения в индексе сохранятся. Вернуть их будет нельзя."
        }
        L10nKey::ScmAmendConfirm => {
            "Изменить последний коммит? Он будет заменён новым. Тем, кто уже получил прежний, придётся согласовать историю."
        }
        L10nKey::ScmOpMerge => "слияние",
        L10nKey::ScmOpRebase => "перебазирование",
        L10nKey::ScmOpCherryPick => "перенос коммита",
        L10nKey::ScmOpRevert => "отмена коммита",
        L10nKey::ScmOpBisect => "поиск ошибки",
        L10nKey::ScmOpAm => "применение патча",
        L10nKey::ScmSwitchRepository => "Сменить репозиторий",
        L10nKey::ScmFilesChanged => "Изменено файлов: {count}",
        L10nKey::ScmStagedFileCount => "Файлов в индексе: {count}",
        L10nKey::WindowStop => "Остановить",
        L10nKey::WindowDelete => "Удалить",
        L10nKey::WindowThisWorkspace => "эта рабочая область",
        L10nKey::WindowConfirmTitle => "{verb} рабочую область «{name}»?",
        L10nKey::WindowStopUnreachable => {
            "Компьютер недоступен. Все работающие на нём оболочки будут завершены."
        }
        L10nKey::WindowDeleteUnreachable => {
            "Компьютер недоступен. Все работающие на нём оболочки будут завершены, расположение удалится."
        }
        L10nKey::WindowStopShells => "Работающие оболочки ({count}) будут завершены.",
        L10nKey::WindowDeleteShells => {
            "Работающие оболочки ({count}) будут завершены, расположение удалится."
        }
        L10nKey::DiffReading => "Чтение различий…",
        L10nKey::DiffNotARepo => "Не репозиторий Git",
        L10nKey::DiffReadFailed => {
            "Не удалось прочитать различия рабочего дерева. Повтор при следующем обновлении."
        }
        L10nKey::DiffWorkingTreeClean => "Рабочее дерево чистое",
        L10nKey::DiffCloseTooltip => "Закрыть различия (Esc)",
        L10nKey::DiffChangedFiles => "Изменённых файлов: {count}",
        L10nKey::DiffUntrackedCount => " · неотслеживаемых: {count}",
        L10nKey::DiffMoreFiles => {
            "…и ещё изменённые файлы ({count}). Посмотрите их командой git diff в терминале."
        }
        L10nKey::DiffOversizedNotice => {
            "Рабочее дерево слишком велико для отображения ({summary}). Все файлы свёрнуты: раскрывайте их по одному или выполните git diff в терминале."
        }
        L10nKey::DiffTruncatedPerFile => {
            "Различия обрезаны на {limit} строках. Остальное покажет git diff в терминале."
        }
        L10nKey::DiffTruncatedBudget => {
            "Содержимое не загружено: превышен лимит различий tty7. Выполните git diff для этого файла в терминале."
        }
        L10nKey::DiffUntrackedHeader => "Неотслеживаемые файлы ({count})",
        L10nKey::DiffMoreUntracked => {
            "…и ещё ({count}). Посмотрите их командой git status в терминале."
        }
        L10nKey::DiffLines => "Строк различий: {count}",
        L10nKey::DiffChangedLines => {
            "Изменённых строк: {total}, загружено строк различий: {loaded}, остальное обрезано ({cap})"
        }
        L10nKey::DiffBudgetAndCap => "лимит tty7 и лимит на файл",
        L10nKey::DiffBudget => "лимит tty7",
        L10nKey::DiffPerFileCap => "лимит на файл",
        L10nKey::DiffUntrackedSummary => "Неотслеживаемых: {count}",
        L10nKey::DiffViewSplit => "В две колонки",
        L10nKey::DiffViewUnified => "В одну колонку",
        L10nKey::DiffCopySelection => "Копировать выбранные строки",
        L10nKey::PendingConnecting => "Подключение к {machine}…",
        L10nKey::PendingUnreachable => "Не удалось подключиться к {machine}",
        L10nKey::WorktreePromptNeedsName => "Нужно имя worktree",
        L10nKey::WorktreePromptTitle => "Новая вкладка worktree",
        L10nKey::WorktreePromptName => "Имя worktree",
        L10nKey::WorktreePromptBranch => "Новая ветка",
        L10nKey::WorktreePromptBase => "Начать с",
        L10nKey::WorktreePromptCreating => "Создание…",
        L10nKey::WorktreePromptCreate => "Создать",
        L10nKey::AppNewWorktreeFailed => "Не удалось создать worktree: {error}",
        L10nKey::HomeTimeJustNow => "только что",
        L10nKey::HomeTimeMinutesAgo => "{count} мин назад",
        L10nKey::HomeTimeHourAgo => "1 час назад",
        L10nKey::HomeTimeHoursAgo => "{count} часов назад",
        L10nKey::HomeTimeYesterday => "вчера",
        L10nKey::HomeTimeDaysAgo => "{count} дней назад",
        L10nKey::HomeTimeOverWeekAgo => "больше недели назад",
        L10nKey::HomeTimeWeeksAgo => "{count} недель назад",
        L10nKey::HomeTimeMonthsAgo => "{count} месяцев назад",
        L10nKey::HomeTimeOverYearAgo => "больше года назад",
        L10nKey::HomeReopenNamed => "Снова открыть «{name}»",
        L10nKey::AppMenuAbout => "О tty7",
        L10nKey::AppMenuCheckForUpdates => "Проверить обновления…",
        L10nKey::AppMenuSettings => "Настройки…",
        L10nKey::AppMenuServices => "Службы",
        L10nKey::AppMenuHideApp => "Скрыть tty7",
        L10nKey::AppMenuHideOthers => "Скрыть остальные",
        L10nKey::AppMenuShowAll => "Показать все",
        L10nKey::AppMenuQuit => "Выйти из tty7",
        L10nKey::AppMenuFile => "Файл",
        L10nKey::AppMenuEdit => "Правка",
        L10nKey::AppMenuView => "Вид",
        L10nKey::AppMenuWindow => "Окно",
        L10nKey::AppMenuHelp => "Справка",
        L10nKey::AppMenuNewTab => "Новая вкладка",
        L10nKey::AppMenuNewWorkspace => "Новая рабочая область…",
        L10nKey::AppMenuNewWorktreeTab => "Новая вкладка worktree…",
        L10nKey::AppMenuSplitRight => "Разделить вправо",
        L10nKey::AppMenuSplitLeft => "Разделить влево",
        L10nKey::AppMenuSplitDown => "Разделить вниз",
        L10nKey::AppMenuSplitUp => "Разделить вверх",
        L10nKey::AppMenuRenameTab => "Переименовать вкладку…",
        L10nKey::AppMenuCopyWorkingDirectory => "Копировать рабочую папку",
        L10nKey::AppMenuCopySessionId => "Копировать ID сеанса",
        L10nKey::AppMenuForkSession => "Форк сеанса",
        L10nKey::AppMenuSaveAgentLaunchArgs => "Сохранить аргументы запуска по умолчанию",
        L10nKey::AppMenuClosePaneTab => "Закрыть",
        L10nKey::AppMenuCloseOtherTabs => "Закрыть другие вкладки",
        L10nKey::AppMenuCloseTabsRight => "Закрыть вкладки справа",
        L10nKey::AppMenuReopenClosedTab => "Снова открыть закрытую вкладку",
        L10nKey::AppMenuRenameWorkspace => "Переименовать рабочую область…",
        L10nKey::AppMenuStopWorkspace => "Остановить рабочую область…",
        L10nKey::AppMenuDeleteWorkspace => "Удалить рабочую область…",
        L10nKey::AppMenuUndo => "Отменить",
        L10nKey::AppMenuRedo => "Повторить",
        L10nKey::AppMenuCut => "Вырезать",
        L10nKey::AppMenuCopy => "Копировать",
        L10nKey::AppMenuPaste => "Вставить",
        L10nKey::AppMenuSelectAll => "Выделить всё",
        L10nKey::AppMenuFind => "Найти…",
        L10nKey::AppMenuFindNext => "Найти следующее",
        L10nKey::AppMenuFindPrevious => "Найти предыдущее",
        L10nKey::AppMenuSearchEverywhere => "Поиск…",
        L10nKey::AppMenuIncreaseFontSize => "Увеличить шрифт",
        L10nKey::AppMenuDecreaseFontSize => "Уменьшить шрифт",
        L10nKey::AppMenuResetFontSize => "Сбросить размер шрифта",
        L10nKey::AppMenuLeftSidebar => "Левая панель",
        L10nKey::AppMenuRightPanel => "Правая панель",
        L10nKey::AppMenuCodePanel => "Панель кода",
        L10nKey::AppMenuTabBarPosition => "Положение вкладок",
        L10nKey::AppMenuFocusNextPane => "Перейти к следующей панели",
        L10nKey::AppMenuFocusPreviousPane => "Перейти к предыдущей панели",
        L10nKey::AppMenuZoomPane => "Развернуть панель",
        L10nKey::AppMenuClearScrollback => "Очистить историю вывода",
        L10nKey::AppMenuOpenLink => "Открыть",
        L10nKey::AppMenuOpenLinkWithDefaultApp => "Открыть в приложении по умолчанию",
        L10nKey::AppMenuRevealInFinder => "Показать в Finder",
        L10nKey::AppMenuRevealInFolder => "Показать папку файла",
        L10nKey::AppMenuCopyLinkPath => "Копировать путь",
        L10nKey::AppMenuEnterFullscreen => "Перейти в полноэкранный режим",
        L10nKey::AppMenuDocumentation => "Документация tty7",
        L10nKey::AppMenuKeyboardShortcuts => "Сочетания клавиш",
        L10nKey::AppMenuJoinDiscord => "Сообщество в Discord",
        L10nKey::AppMenuReportIssue => "Сообщить о проблеме…",
        L10nKey::AppMenuRestartServer => "Перезапустить сервер tty7…",
        L10nKey::WindowUntitled => "Без имени",
        L10nKey::TrayShowTty7 => "Показать tty7",
        L10nKey::TrayNotifications => "Уведомления",
        L10nKey::TrayAgentNeedsInput => "ждёт ввода",
        L10nKey::AgentStatusWorking => "Работает",
        L10nKey::AgentStatusWaiting => "Ждёт ввода",
        L10nKey::AgentStatusDone => "Готово",
        L10nKey::NotifyCommandFinished => "Команда завершилась за {secs} с",
        L10nKey::NotifyCommandFinishedWithCommand => "{command}: завершено за {secs} с",
        L10nKey::NotifyAgentFinished => "Завершено за {secs} с",
        L10nKey::NotifyAgentWaiting => "Ждёт ответа",
        L10nKey::NotifyTurnFinished => "Ответ завершён",
        L10nKey::TabTooltipMore => "Ещё",
        L10nKey::TabTooltipShowSidebar => "Показать боковую панель",
        L10nKey::TabTooltipHideSidebar => "Скрыть боковую панель",
        L10nKey::TabTooltipHideDetailPanel => "Скрыть панель сведений",
        L10nKey::TabTooltipShowDetailPanel => "Показать панель сведений",
        L10nKey::TabTooltipZoomed => "Панель развёрнута, остальные скрыты",
        L10nKey::TabMenuLocalShells => "Локальные",
        L10nKey::TabMenuAddHost => "Добавить SSH-хост…",
        L10nKey::TabMenuAllHosts => "Все SSH-хосты…",
        L10nKey::TabMenuOtherShells => "Другие оболочки…",
        L10nKey::TabMenuOtherAgents => "Другие агенты…",
        L10nKey::TabMenuSplitHint => "Удерживайте {key}, чтобы разделить",
        L10nKey::TabUnnamedShell => "Оболочка {n}",
        L10nKey::ShellDefault => "по умолчанию",
        L10nKey::SidebarUngroupedGroup => "Без группы",
        L10nKey::SidebarMoveToGroup => "Переместить в группу",
        L10nKey::SidebarNewGroup => "Новая группа…",
        L10nKey::SidebarRemoveFromGroup => "Убрать из группы",
        L10nKey::SidebarNewGroupName => "Новая группа",
        L10nKey::SidebarRenameGroup => "Переименовать группу",
        L10nKey::SidebarPinGroup => "Закрепить группу",
        L10nKey::SidebarUnpinGroup => "Открепить",
        L10nKey::SidebarGroupNewTab => "Новая вкладка",
        L10nKey::SidebarSetGroupFolder => "Выбрать папку…",
        L10nKey::SidebarUseCurrentTabFolder => "Использовать папку текущей вкладки",
        L10nKey::SidebarClearGroupFolder => "Убрать папку",
        L10nKey::SidebarDeleteGroup => "Удалить группу",
        L10nKey::SidebarDropToPin => "Перетащите сюда, чтобы закрепить",
        L10nKey::TabContextCloseTab => "Закрыть вкладку",
        L10nKey::TerminalContextClear => "Очистить",
        L10nKey::TabContextCloseTabsBelow => "Закрыть вкладки ниже",
        L10nKey::TabContextMarkUnread => "Отметить непрочитанной",
        L10nKey::TabContextHibernate => "Перевести в спящий режим",
        L10nKey::TabContextWake => "Возобновить",
        L10nKey::TabTooltipAsleep => "В спящем режиме. Выберите, чтобы возобновить",
        L10nKey::TabWakeFailed => "Не удалось возобновить вкладку: ни одна панель не запустилась",
        L10nKey::RemoteStripDisconnected => "Нет соединения с {machine}",
        L10nKey::RemoteStripConnecting => "Подключение к {machine}…",
        L10nKey::RemoteStripReconnecting => "Переподключение к {machine}…",
        L10nKey::RemoteStripReconnectingAttempt => "Переподключение к {machine}… (попытка {count})",
        L10nKey::RemoteStripReconnectingWhy => {
            "Переподключение к {machine}… Последняя ошибка: {error}"
        }
        L10nKey::RemoteStripReconnectingAttemptWhy => {
            "Переподключение к {machine}… (попытка {count}). Последняя ошибка: {error}"
        }
        L10nKey::RemoteStripPreempted => "Эта рабочая область открыта на {by}",
        L10nKey::RemoteStripFailed => "Нет соединения с {machine}: {error}",
        L10nKey::RemoteStripRouteLost => {
            "Профиль подключения к {machine} больше не существует. Переподключение невозможно"
        }
        L10nKey::RemoteRouteParkedHint => {
            "Профиль подключения удалён, поэтому автоматически переподключиться нельзя. Удалённый сеанс сохранился: подключитесь с новым профилем, и он вернётся в список рабочих областей."
        }
        L10nKey::RemoteNoticePreempted => "Открыто в другом месте. Ввод не действует",
        L10nKey::RemoteNoticeDisconnected => "Нет соединения. Ввод не действует",
        L10nKey::RemoteActionRetryNow => "Повторить сейчас",
        L10nKey::RemoteActionTakeBack => "Вернуть управление",
        L10nKey::PaneLeasedBy => "Используется на {by}, размер подогнан под его экран",
        L10nKey::RemoteActionConnect => "Подключиться",
        L10nKey::RemoteActionRetry => "Повторить",
        L10nKey::RemoteActionRemoveEntry => "Удалить запись",
        L10nKey::RemoteNoConnectionDetails => {
            "Окно показывает рабочую область на {machine}, но у tty7 нет данных для подключения. Проверьте, что её профиль SSH или запись в ~/.ssh/config ещё существует."
        }
        L10nKey::RemoteThisComputer => "этот компьютер",
        L10nKey::RemoteProfileGone => "удалённый профиль",
        L10nKey::RemoteRestartTitle => "Перезапустить сервер tty7 на «{machine}»?",
        L10nKey::RemoteRestartBody => {
            "Все оболочки на {machine} завершатся, включая не показанные в этом окне. Рабочие области и расположение сохранятся и восстановятся с новыми оболочками."
        }
        L10nKey::RemoteReplaceBody => {
            "tty7 установит совместимый сервер на {machine} и запустит его.\n\nВсе сеансы на {machine} завершатся, включая не подключённые к этому окну."
        }
        L10nKey::RemoteRestartFailedTitle => "Сервер tty7 на «{machine}» не перезапущен",
        L10nKey::RemoteRestartFailedBody => {
            "{error}\n\nОставшиеся сеансы используют старую сборку. Если сеансов уже нет, переподключение запустит сервер этой сборки."
        }
        L10nKey::RemoteHostUnreachable => "не удалось подключиться к {machine}: {error}",
        L10nKey::RemoteInstallTitle => "Установить сервер tty7 на «{machine}»?",
        L10nKey::RemoteInstallDetail => {
            "tty7 запишет исполняемый файл сервера на {machine}, чтобы открывать там рабочие области. Другие файлы на {machine} не меняются, sudo не используется.\n\n{path_label} {path}\n{version_label} {version}\n{size_label} {size}\n{from_label} {from}\n{sha_label} {sha256}\n\n{silent_upgrades}"
        }
        L10nKey::RemoteInstallPathLabel => "Путь",
        L10nKey::RemoteInstallVersionLabel => "Версия",
        L10nKey::RemoteInstallSizeLabel => "Размер",
        L10nKey::RemoteInstallFromLabel => "Источник",
        L10nKey::RemoteInstallShaLabel => "SHA-256",
        L10nKey::RemoteInstallSilentUpgrades => {
            "Последующие обновления на этом компьютере устанавливаются без запроса."
        }
        L10nKey::RemoteInstallBytes => "байт",
        L10nKey::RemoteMismatchTitle => "Обновить сервер tty7 на «{machine}»?",
        L10nKey::RemoteMismatchDetail => {
            "На {machine} работает сервер {running}, несовместимый с клиентом {wanted}. Подходящий сервер уже установлен, но сеансы используют работающий.\n\n{replace_server} заменит его на {wanted} и завершит все его сеансы.\n{cancel} сохранит текущее состояние {machine}. Окно не подключится."
        }
        L10nKey::RemoteMismatchUnknownBuild => "неизвестная сборка",
        L10nKey::RemoteMismatchUnknownBuildFromExe => "неизвестная сборка (из {exe})",
        L10nKey::RemoteMismatchReplaceServer => "Обновить сервер",
        L10nKey::RemoteMismatchDowngradeServer => "Заменить сервер",
        L10nKey::RemoteServerOutdated => {
            "На {machine} работает старый сервер tty7 ({build}), несовместимый с этой копией tty7. Обновите его, чтобы подключиться."
        }
        L10nKey::RemoteServerTooNew => {
            "На {machine} работает сервер tty7 ({build}) новее этой копии tty7. Обновите tty7 на этом компьютере или замените удалённый сервер совместимой версией."
        }
        L10nKey::RemoteDaemonStartFailed => "Не удалось запустить локальный сервер tty7: {error}",
        L10nKey::RemoteDaemonUnreachable => {
            "не удалось подключиться к локальному серверу tty7: {error}"
        }
        L10nKey::RemoteDaemonTooOld => {
            "Локальный сервер слишком старый, чтобы перезапустить сервер на {machine}. Выйдите из tty7 (сервер остановится), откройте его снова и повторите попытку."
        }
        L10nKey::RemoteProfileMissing => "этот сохранённый профиль SSH больше не существует",
        L10nKey::RemoteAliasMissing => "«{alias}» больше нет в ~/.ssh/config",
        L10nKey::RemoteWslNoSsh => "У рабочей области WSL нет SSH-соединения",
        L10nKey::RemoteLocalStdioNoSsh => "У локальной рабочей области --stdio нет SSH-соединения",
        L10nKey::RemoteHostNotTty7 => "{machine} ответил, но не как сервер tty7: {error}",
        L10nKey::RemoteWorkspaceListFailed => {
            "Соединение с {machine} установлено, но не удалось получить список рабочих областей: {error}"
        }
        L10nKey::RemoteServerRestartFailed => {
            "не удалось перезапустить сервер tty7 на {machine}: {error}"
        }
        L10nKey::RemoteNoRouteToHost => "У tty7 больше нет способа подключиться к {machine}",
        L10nKey::RemoteMachineTreeUnexpectedReply => {
            "Сервер ответил на запрос дерева компьютера: {reply}"
        }
        L10nKey::RemoteMismatchVersionFromExe => "{version} (из {exe})",
        L10nKey::AppNoRunningCodingAgent => {
            "Работающий агент не найден. Сначала запустите агента (claude, codex, …) в панели."
        }
        L10nKey::SwitcherThisComputer => "Этот компьютер",
        L10nKey::SwitcherStartingServer => "Запуск сервера tty7…",
        L10nKey::SwitcherDownloadingServerWithTotal => "Загрузка сервера tty7… {done} / {total}",
        L10nKey::SwitcherDownloadingServerNoTotal => "Загрузка сервера tty7… {done}",
        L10nKey::SwitcherCopyingServer => "Копирование сервера tty7… {done} / {total}",
        L10nKey::SwitcherThisWindow => "Это окно",
        L10nKey::SwitcherOpen => "Открыть",
        L10nKey::SwitcherOffline => "Не в сети",
        L10nKey::SwitcherDisconnect => "Отключить",
        L10nKey::SwitcherEditHost => "Изменить хост…",
        L10nKey::SwitcherSaveAsHost => "Сохранить как SSH-хост…",
        L10nKey::SshSaveDroppedJumpHost => {
            "Промежуточный хост не сохранён. Сохранённый хост подключается через другой сохранённый хост."
        }
        L10nKey::SwitcherOpenInNewWindow => "Открыть в новом окне",
        L10nKey::SwitcherRename => "Переименовать…",
        L10nKey::SwitcherPickAWorkspace => "Выберите рабочую область, чтобы увидеть её вкладки.",
        L10nKey::SwitcherNoTabs => "В этой рабочей области нет вкладок.",
        L10nKey::SwitcherNoTabMatch => "Вкладки не найдены.",
        L10nKey::SwitcherTabsAfterOpening => "Откройте рабочую область, чтобы увидеть её вкладки.",
        L10nKey::SwitcherOpenToManage => {
            "Откройте рабочую область, чтобы переименовать или остановить её."
        }
        L10nKey::SwitcherConnectToUse => {
            "Подключитесь к компьютеру, чтобы открыть на нём рабочую область."
        }
        L10nKey::SwitcherOrphanPanes => "Фоновые панели: оболочки, работающие вне окон:",
        L10nKey::SwitcherTabCount => "Вкладок: {n}",
        L10nKey::SwitcherTabCountOne => "1 вкладка",
        L10nKey::SwitcherActiveTab => "Текущая",
        L10nKey::SwitcherHoldToSwitch => "Tab: выбор · отпустите, чтобы перейти",
        L10nKey::SwitcherTabToCrossColumns => "Tab: переход между столбцами",
        L10nKey::SwitcherHintNavigate => "Навигация",
        L10nKey::SwitcherHintOpen => "Открыть",
        L10nKey::SearchHintNextScope => "Следующий раздел",
        L10nKey::PanelSearchInContents => "В содержимом файлов",
        L10nKey::PanelFilesNameMatches => "Имена файлов",
        L10nKey::SwitcherHintNewWindow => "Новое окно",
        L10nKey::SwitcherLocalHost => "локальный",
        L10nKey::SwitcherConnectingTo => "Подключение к {machine}…",
        L10nKey::SwitcherFormName => "Имя",
        L10nKey::SwitcherFormHost => "Хост",
        L10nKey::SwitcherFormNamePlaceholder => "Имя рабочей области",
        L10nKey::SwitcherFormBack => "Назад",
        L10nKey::SwitcherFormCreate => "Создать",
        L10nKey::SwitcherFormPickHint => "↑↓ выбор · Enter подтвердить · Esc закрыть",
        L10nKey::SshPromptPasswordFor => "Пароль для {user}@{host}",
        L10nKey::SshPromptPassphraseFor => "Парольная фраза для {key_path}",
        L10nKey::SshPromptTwoFactor => "Двухфакторная аутентификация",
        L10nKey::SshPromptUnknownHost => "Неизвестный хост {host}",
        L10nKey::SshPromptHostKeyChanged => "Ключ хоста ИЗМЕНИЛСЯ. Возможна подмена соединения",
        L10nKey::SshPromptHostKeyChangedBody => {
            "Ключ хоста отличается от ранее принятого. Это может быть атакой."
        }
        L10nKey::SshPromptConnect => "Подключиться",
        L10nKey::SshPromptUnlock => "Разблокировать",
        L10nKey::SshPromptSubmit => "Отправить",
        L10nKey::GitOpFailed => "Ошибка git {op}",
        L10nKey::IoDenied => "Нет прав доступа.",
        L10nKey::IoGone => "Объект больше не существует.",
        L10nKey::IoNoSpace => "На диске нет свободного места.",
        L10nKey::IoReadOnly => "Это место доступно только для чтения.",
        L10nKey::IoBusy => "Объект открыт другой программой.",
        L10nKey::IoTimedOut => "Компьютер не ответил вовремя.",
        L10nKey::TreeWindowOpenedEmpty => {
            "Сервер не передал вкладки окна, поэтому оно открылось пустым. Ничего не потеряно: вкладки вернутся, когда сервер ответит. Если ответа нет, выполните «Перезапустить сервер tty7» из палитры команд."
        }
        L10nKey::CmdGroupTabsPanes => "Вкладки и панели",
        L10nKey::CmdGroupWorkspaces => "Рабочие области",
        L10nKey::CmdGroupView => "Вид",
        L10nKey::CmdGroupGit => "Git",
        L10nKey::CmdGroupTerminal => "Терминал",
        L10nKey::CmdGroupSsh => "SSH",
        L10nKey::CmdGroupAgents => "Агенты",
        L10nKey::CmdGroupApplication => "Приложение",
        L10nKey::CmdNewTab => "Новая вкладка",
        L10nKey::CmdNewWindow => "Новое окно",
        L10nKey::CmdNewWorktreeTab => "Новая вкладка worktree…",
        L10nKey::CmdNewWorktreeTabSubtitle => "отдельная рабочая копия на новой ветке",
        L10nKey::CmdNewGroup => "Новая группа",
        L10nKey::CmdNewGroupSubtitle => "пустая закреплённая группа на боковой панели",
        L10nKey::CmdOpenFolderAsGroup => "Открыть папку как группу…",
        L10nKey::CmdOpenFolderAsGroupSubtitle => "закрепить папку и объединять её вкладки",
        L10nKey::CmdRenameTab => "Переименовать вкладку…",
        L10nKey::CmdSplitRight => "Разделить вправо",
        L10nKey::CmdSplitDown => "Разделить вниз",
        L10nKey::CmdZoomPane => "Развернуть панель",
        L10nKey::CmdNextPane => "Следующая панель",
        L10nKey::CmdPreviousPane => "Предыдущая панель",
        L10nKey::CmdFocusPaneLeft => "Перейти к панели слева",
        L10nKey::CmdFocusPaneRight => "Перейти к панели справа",
        L10nKey::CmdFocusPaneUp => "Перейти к панели сверху",
        L10nKey::CmdFocusPaneDown => "Перейти к панели снизу",
        L10nKey::CmdResizePaneLeft => "Изменить размер влево",
        L10nKey::CmdResizePaneRight => "Изменить размер вправо",
        L10nKey::CmdResizePaneUp => "Изменить размер вверх",
        L10nKey::CmdResizePaneDown => "Изменить размер вниз",
        L10nKey::CmdSwapPaneNext => "Поменять местами со следующей панелью",
        L10nKey::CmdSwapPanePrevious => "Поменять местами с предыдущей панелью",
        L10nKey::CmdNextTab => "Следующая вкладка",
        L10nKey::CmdPreviousTab => "Предыдущая вкладка",
        L10nKey::CmdMoveTabLeft => "Переместить вкладку влево",
        L10nKey::CmdMoveTabRight => "Переместить вкладку вправо",
        L10nKey::CmdRecentTabSwitcher => "Недавние вкладки",
        L10nKey::CmdRecentTabSwitcherReverse => "Недавние вкладки (назад)",
        L10nKey::CmdCopyWorkingDirectory => "Копировать рабочую папку",
        L10nKey::CmdCopySessionId => "Копировать ID сеанса",
        L10nKey::CmdCopySessionIdSubtitle => "ID сеанса самого агента",
        L10nKey::CmdNewAgentTab => "Новая вкладка агента",
        L10nKey::CmdNewAgentTabSubtitle => {
            "открыть последнего использованного агента в новой вкладке"
        }
        L10nKey::CmdForkSession => "Форк сеанса",
        L10nKey::CmdForkSessionSubtitle => "продолжить копию сеанса агента в новой вкладке",
        L10nKey::CmdMarkTabAsUnread => "Отметить непрочитанной",
        L10nKey::CmdHibernateTab => "Перевести вкладку в спящий режим",
        L10nKey::CmdHibernateTabSubtitle => {
            "остановить процессы и освободить память, выбор вкладки возобновляет её"
        }
        L10nKey::CmdClosePaneTab => "Закрыть панель / вкладку",
        L10nKey::CmdCloseWindow => "Закрыть окно",
        L10nKey::CmdCloseWindowSubtitle => "оболочки продолжат работать",
        L10nKey::CmdCloseOtherTabs => "Закрыть другие вкладки",
        L10nKey::CmdCloseTabsToTheRight => "Закрыть вкладки справа",
        L10nKey::CmdReopenClosedTab => "Снова открыть закрытую вкладку",
        L10nKey::CmdNewWorkspace => "Новая рабочая область…",
        L10nKey::CmdSwitchWorkspace => "Сменить рабочую область…",
        L10nKey::CmdRenameWorkspace => "Переименовать рабочую область…",
        L10nKey::CmdStopWorkspace => "Остановить рабочую область…",
        L10nKey::CmdStopWorkspaceSubtitle => "завершить оболочки, сохранить расположение",
        L10nKey::CmdDeleteWorkspace => "Удалить рабочую область…",
        L10nKey::CmdDeleteWorkspaceSubtitle => "завершить оболочки и удалить расположение",
        L10nKey::CmdShowLeftSidebar => "Показать левую панель",
        L10nKey::CmdHideLeftSidebar => "Скрыть левую панель",
        L10nKey::CmdHideRightPanel => "Скрыть правую панель",
        L10nKey::CmdShowRightPanel => "Показать правую панель",
        L10nKey::CmdShowCodePanel => "Показать панель кода",
        L10nKey::CmdTabBarMoveToTop => "Вкладки: переместить наверх",
        L10nKey::CmdTabBarMoveToLeftSidebar => "Вкладки: переместить влево",
        L10nKey::CmdRightPanelInfo => "Правая панель: сведения",
        L10nKey::CmdRightPanelChanges => "Правая панель: изменения",
        L10nKey::CmdRightPanelFiles => "Правая панель: файлы",
        L10nKey::CmdRightPanelSearch => "Правая панель: поиск",
        L10nKey::CmdRightPanelGitHub => "Правая панель: GitHub",
        L10nKey::CmdChangeTheme => "Сменить тему…",
        L10nKey::CmdResetFontSize => "Сбросить размер шрифта",
        L10nKey::CmdEnterFullScreen => "Перейти в полноэкранный режим",
        L10nKey::CmdToggleDiffViewMode => "Сменить вид различий: единый / рядом",
        L10nKey::CmdDocumentDock => "Документ: закрепить рядом с терминалом",
        L10nKey::CmdDocumentFill => "Документ: развернуть на всё окно",
        L10nKey::CmdToggleDocumentFill => "Документ: переключить на всё окно / рядом",
        L10nKey::CmdDocumentWidthThird => "Документ: треть ширины",
        L10nKey::CmdDocumentWidthHalf => "Документ: половина ширины",
        L10nKey::CmdDocumentWidthTwoThirds => "Документ: две трети ширины",
        L10nKey::CmdToggleDocumentPreview => "Документ: показать или скрыть просмотр Markdown",
        L10nKey::CmdToggleDocumentWrap => "Документ: включить или выключить перенос строк",
        L10nKey::CmdEditorTransformUppercase => "Документ: преобразовать в верхний регистр",
        L10nKey::CmdEditorTransformLowercase => "Документ: преобразовать в нижний регистр",
        L10nKey::CmdEditorTransformTitleCase => "Документ: начать каждое слово с прописной буквы",
        L10nKey::CmdEditorTrimTrailingWhitespace => "Документ: убрать конечные пробелы",
        L10nKey::CmdEditorJoinLines => "Документ: объединить строки",
        L10nKey::CmdEditorRemoveSurroundingBrackets => "Документ: убрать внешние скобки",
        L10nKey::CmdGitCommit => "Git: создать коммит",
        L10nKey::CmdGitStageAll => "Git: добавить всё в индекс",
        L10nKey::CmdGitUnstageAll => "Git: очистить индекс",
        L10nKey::CmdGitDiscardAll => "Git: отменить все изменения",
        L10nKey::CmdGitDiscardAllSubtitle => {
            "Удаляет все незакоммиченные изменения в рабочем дереве."
        }
        L10nKey::CmdGitCheckoutTo => "Git: перейти на…",
        L10nKey::CmdGitCreateBranch => "Git: создать ветку…",
        L10nKey::CmdGitSync => "Git: синхронизировать",
        L10nKey::CmdGitSyncSubtitle => "Получить, затем отправить.",
        L10nKey::CmdGitPush => "Git: отправить",
        L10nKey::CmdGitPull => "Git: получить",
        L10nKey::CmdGitFetch => "Git: извлечь",
        L10nKey::CmdGitToggleGraph => "Git: показать/скрыть историю",
        L10nKey::CmdClearScrollback => "Очистить историю вывода",
        L10nKey::CmdFindInTerminal => "Найти в терминале…",
        L10nKey::CmdToggleComposer => "Показать/скрыть ввод сообщения",
        L10nKey::ComposerPlaceholder => "Сообщение {agent} · @ файлы / команды",
        L10nKey::ComposerPlaceholderFiles => "Сообщение {agent} · @ файлы",
        L10nKey::ComposerSendTip => "Отправить (Enter) · Shift+Enter для новой строки",
        L10nKey::ComposerAttach => "Прикрепить файлы или изображения",
        L10nKey::ComposerMenuCommands => "Команды",
        L10nKey::ComposerMenuFiles => "Файлы",
        L10nKey::ComposerCmdProject => "Команда проекта",
        L10nKey::ComposerCmdUser => "Команда пользователя",
        L10nKey::ComposerAgentAsking => {
            "{agent} задал вопрос. Ответьте в терминале, затем отправьте сообщение."
        }
        L10nKey::ComposerModeDefault => "Спрашивать перед правками",
        L10nKey::ComposerModeAcceptEdits => "Разрешать правки",
        L10nKey::ComposerModePlan => "Планирование",
        L10nKey::ComposerModeBypass => "Без запросов разрешений",
        L10nKey::ComposerModeAuto => "Автоматически",
        L10nKey::ComposerModeTip => "Режим разрешений · Shift+Tab переключает",
        L10nKey::ComposerModel => "Модель",
        L10nKey::ComposerModelTip => "Сменить модель",
        L10nKey::ComposerModelDefault => "По умолчанию (рекомендуется)",
        L10nKey::ComposerEffort => "Глубина рассуждений",
        L10nKey::ComposerEffortTip => "Глубина рассуждений · нажмите, чтобы сменить",
        L10nKey::CmdFindNext => "Найти следующее",
        L10nKey::CmdFindPrevious => "Найти предыдущее",
        L10nKey::CmdCopy => "Копировать",
        L10nKey::CmdCut => "Вырезать",
        L10nKey::CmdPaste => "Вставить",
        L10nKey::CmdAlternatePaste => "Вставить (вне полноэкранных приложений)",
        L10nKey::CmdSelectAll => "Выделить всё",
        L10nKey::CmdSshAddConnection => "SSH: добавить соединение…",
        L10nKey::CmdSshManageProfiles => "SSH: управлять профилями…",
        L10nKey::CmdSshReconnect => "SSH: переподключить",
        L10nKey::CmdSshRemoteFiles => "SSH: удалённые файлы",
        L10nKey::CmdSshPortForwarding => "SSH: проброс портов",
        L10nKey::CmdSshSaveConnection => "SSH: сохранить как хост…",
        L10nKey::CmdSshSaveConnectionSubtitle => "Сохранить это подключение как хост.",
        L10nKey::CmdSshConnectWithInput => "SSH: подключиться к {input}",
        L10nKey::CmdAgentSendSelection => "Агент: отправить выделение",
        L10nKey::CmdAgentSendSelectionSubtitle => "передать выделение работающему агенту",
        L10nKey::CmdAgentSendGitDiffForReview => "Агент: отправить различия Git на проверку",
        L10nKey::CmdAgentSendGitDiffSubtitle => "передать различия Git работающему агенту",
        L10nKey::CmdSettings => "Настройки…",
        L10nKey::CmdKeyboardShortcuts => "Сочетания клавиш",
        L10nKey::CmdAboutTty7 => "О tty7",
        L10nKey::CmdCheckForUpdates => "Проверить обновления…",
        L10nKey::CmdDocumentation => "Документация",
        L10nKey::CmdJoinDiscord => "Сообщество в Discord",
        L10nKey::CmdReportIssue => "Сообщить о проблеме…",
        L10nKey::CmdRestartServer => "Перезапустить сервер tty7…",
        L10nKey::CmdRestartServerSubtitle => "завершить все оболочки, сохранить расположение",
        L10nKey::CmdQuitTty7 => "Выйти из tty7",
        L10nKey::CmdQuitTty7Subtitle => "остановить сервер и завершить все оболочки",
        L10nKey::CmdQuickConnect => "Подключиться к «{target}»",
        L10nKey::CmdQuickConnectSaveProfile => "Сохранить «{target}» как профиль…",
        L10nKey::CmdRecent => "Недавние",
        L10nKey::AppRestartServerTitle => "Перезапустить сервер tty7?",
        L10nKey::AppRestartServerFailed => "Не удалось перезапустить сервер tty7: {error}",
        L10nKey::AppRestartServerMismatchDetail => {
            "Сервер tty7 с работающими оболочками использует протокол {protocol} (сборка v{build}), приложение использует {ours}. Поэтому вкладки недоступны.\n\nВыйти: сервер tty7 и оболочки продолжат работать.\nПерезапустить: вкладки восстановятся с новыми оболочками, все текущие процессы завершатся."
        }
        L10nKey::AppRestartServerDialectDetail => {
            "Сервер tty7 с работающими оболочками использует версию управления v{dialect} (сборка v{build}), приложение использует v{ours}. Поэтому все окна открываются пустыми.\n\nВыйти: сервер tty7 и оболочки продолжат работать.\nПерезапустить: вкладки восстановятся с новыми оболочками, все текущие процессы завершатся."
        }
        L10nKey::AppRestartServerDialectNewerDetail => {
            "Сервер tty7 с работающими оболочками использует версию управления v{dialect} (сборка v{build}), а приложение использует v{ours}. Поэтому все окна открываются пустыми.\n\nВыйти и установить новую сборку: проблема исчезнет, оболочки сохранятся.\nПерезапустить: вкладки восстановятся с новыми оболочками, все текущие процессы завершатся."
        }
        L10nKey::AppRestartServerOldDetail => {
            "Сервер tty7 с работающими оболочками выпущен до проверки совместимости версий. Приложение не может определить его протокол.\n\nВыйти: сервер tty7 и оболочки продолжат работать.\nПерезапустить: вкладки восстановятся с новыми оболочками, все текущие процессы завершатся."
        }
        L10nKey::AppRestart => "Перезапустить",
        L10nKey::AppRestartServerNoServer => {
            "У {label} нет собственного сервера: это программа на этом компьютере, запущенная через --stdio. Остановите её рабочую область."
        }
        L10nKey::AppRestartServerBody => {
            "Все оболочки на этом компьютере завершатся. Вкладки и расположение сохранятся и восстановятся с новыми оболочками."
        }
        L10nKey::ConfigQuarantinedStartup => {
            "Не удалось разобрать config.json. tty7 использует настройки по умолчанию и сохранил содержимое рядом в config.json.corrupt. После исправления tty7 перечитает файл. До этого изменения настроек не сохраняются."
        }
        L10nKey::ConfigQuarantinedReload => {
            "Не удалось разобрать изменённый config.json. tty7 сохранил действующие настройки и отложил содержимое в config.json.corrupt. После исправления tty7 перечитает файл. Сохранение настройки до исправления перезапишет его."
        }
        L10nKey::ConfigUnreadableStartup => {
            "Не удалось прочитать config.json. tty7 использует настройки по умолчанию и оставил файл без изменений. После исправления прав или содержимого tty7 перечитает файл. До этого изменения настроек не сохраняются."
        }
        L10nKey::ConfigUnreadableReload => {
            "Не удалось прочитать config.json. tty7 сохранил действующие настройки и оставил файл без изменений. После исправления прав или содержимого tty7 перечитает файл. Сохранение настройки до исправления перезапишет его."
        }
        L10nKey::AppWorktreeRemoveDetailDirty => {
            "В worktree закрытой вкладки по пути {path} есть незакоммиченные изменения. Перед удалением они сохранятся в refs/tty7/trash/{name}."
        }
        L10nKey::AppWorktreeRemoveDetailClean => "Worktree закрытой вкладки по пути {path} чистый.",
        L10nKey::AppWorktreeRemoveTitle => "Удалить worktree «{branch}»?",
        L10nKey::AppWorktreeDiscardAndRemove => "Отменить изменения и удалить",
        L10nKey::AppWorktreeRemove => "Удалить worktree",
        L10nKey::AppWorktreeKeep => "Оставить",
        L10nKey::AppReopenTabFailed => "Не удалось вернуть вкладку: терминал не запустился",
        L10nKey::AppOpenTerminalFailed => "Не удалось открыть терминал: {error}",
        L10nKey::AppTabsNotRestored => "Не удалось восстановить вкладки прошлого сеанса: {count}",
        L10nKey::AppFullscreenEntered => "Полный экран. Чтобы выйти, нажмите {key}",
        L10nKey::AppFullscreenEnteredNoKey => "Полный экран. Кнопки окна скрыты до выхода",
        L10nKey::LaunchWorkspacesLeftRunning => {
            "Восстановлено только это окно. Рабочие области ({count}) ещё работают в фоне. Откройте их на боковой панели."
        }
        L10nKey::AppSshConnectionFailed => "Ошибка SSH-подключения: {error}",
        L10nKey::AppSshReconnectFailed => "Ошибка переподключения SSH: {error}",
        L10nKey::AppSplitPaneFailed => "Не удалось разделить панель: {error}",
        L10nKey::PaneDragHandleTooltip => "Перетащите, чтобы переместить панель",
        L10nKey::AppWorktreeRemoved => "Worktree «{branch}» удалён",
        L10nKey::AppWorktreeRemoveFailed => "Не удалось удалить worktree: {error}",
        L10nKey::WorktreePromptAgent => "Запустить",
        L10nKey::WorktreePromptShell => "Оболочка",
        L10nKey::WorktreePromptTask => "Задача",
        L10nKey::WorktreePromptSetup => "Сначала запускается .tty7/setup",
        L10nKey::WorktreePromptSetupHint => {
            "Нет .tty7/setup. Добавьте его, чтобы выполнять `{command}` в новых worktree"
        }
        L10nKey::AppWorktreeSetupTitle => "Запустить сценарий настройки репозитория?",
        L10nKey::AppWorktreeSetupDetail => {
            "{path} запустится в новой вкладке раньше всего остального. Подтверждайте, только если доверяете репозиторию. tty7 спросит снова, если сценарий изменится."
        }
        L10nKey::AppWorktreeSetupRun => "Запустить настройку",
        L10nKey::AppWorktreeSetupSkip => "Пропустить",
        L10nKey::AppWorktreeNotCarried => "Не скопировано из .worktreeinclude: {paths}",
        L10nKey::AppWorktreeRemovedBranchKept => {
            "Worktree удалён, ветка «{branch}» с невлитыми коммитами сохранена"
        }
        L10nKey::AppPaneNoCodingAgent => "В этой панели не работает агент",
        L10nKey::AppForkNoCommand => "У tty7 нет команды форка для {name}",
        L10nKey::AppForkLocalOnly => "Форк сеансов {name} недоступен из SSH или WSL",
        L10nKey::AppForkNoSessionId => {
            "tty7 ещё не получил ID сеанса {name} в этой панели. Установите хуки агента в разделе «Настройки → Интеграции»"
        }
        L10nKey::AppForkSessionIdNotToken => "ID сеанса {name} имеет неверный формат токена",
        L10nKey::AppForkMidTurn => "{name} ещё отвечает. Текущий ответ не попадёт в форк",
        L10nKey::AppTabNoWorkingDirectory => "У этой вкладки ещё нет рабочей папки",
        L10nKey::AppNothingSelected => "Ничего не выделено. Сначала выделите вывод терминала.",
        L10nKey::AppPaneNoKnownDirectory => "Папка этой панели неизвестна.",
        L10nKey::AppNoUncommittedChanges => {
            "В {cwd} нет незакоммиченных изменений (или это не репозиторий Git)."
        }
        L10nKey::AppCmdAgentLaunchTitle => "Агент: {name}",
        L10nKey::AppNoAgentOnPath => "В PATH этого компьютера не найден агент",
        L10nKey::AppNoAgentSeenHere => {
            "В этой рабочей области ещё не запускался агент. Запустите его один раз вручную, и он появится здесь"
        }
        L10nKey::AppAgentLaunchSaved => "{name} теперь запускается так: {command}",
        L10nKey::AppAgentLaunchArgsUnknown => "{name} не сообщил аргументы запуска",
        L10nKey::AppCmdShellTitle => "Оболочка: {title}",
        L10nKey::AppPlaceholderDescription => "описание",
        L10nKey::AppPlaceholderSshQuickConnect => "user@host  или  user@host:port",
        L10nKey::AppPlaceholderLoginShell => "оболочка входа",
        L10nKey::AppPlaceholderNone => "нет",
        L10nKey::AppPlaceholderOpenInDefaultApp => "открыть в приложении по умолчанию",
        L10nKey::AppThemeColorBackground => "Фон",
        L10nKey::AppThemeColorForeground => "Текст",
        L10nKey::AppThemeColorAccent => "Акцент",
        L10nKey::AppThemeColorCursor => "Курсор",
        L10nKey::AppThemeColorSelection => "Выделение",
        L10nKey::AppThemeAnsiBlack => "Чёрный",
        L10nKey::AppThemeAnsiRed => "Красный",
        L10nKey::AppThemeAnsiGreen => "Зелёный",
        L10nKey::AppThemeAnsiYellow => "Жёлтый",
        L10nKey::AppThemeAnsiBlue => "Синий",
        L10nKey::AppThemeAnsiMagenta => "Пурпурный",
        L10nKey::AppThemeAnsiCyan => "Голубой",
        L10nKey::AppThemeAnsiWhite => "Белый",
        L10nKey::AppThemeAnsiBrightBlack => "Яркий чёрный",
        L10nKey::AppThemeAnsiBrightRed => "Яркий красный",
        L10nKey::AppThemeAnsiBrightGreen => "Яркий зелёный",
        L10nKey::AppThemeAnsiBrightYellow => "Яркий жёлтый",
        L10nKey::AppThemeAnsiBrightBlue => "Яркий синий",
        L10nKey::AppThemeAnsiBrightMagenta => "Яркий пурпурный",
        L10nKey::AppThemeAnsiBrightCyan => "Яркий голубой",
        L10nKey::AppThemeAnsiBrightWhite => "Яркий белый",
        L10nKey::AppAgentHooksThisComputer => "Этот компьютер",
        L10nKey::AppAgentHooksRemoteMachine => "Удалённый компьютер",
        L10nKey::AppAgentHooksNoHomeDir => {
            "tty7 не определил домашнюю папку этого компьютера. Установка невозможна."
        }
        L10nKey::AppAgentHooksOffline => {
            "Нет соединения с компьютером, поэтому настройки агентов нельзя прочитать или изменить. Откройте на нём рабочую область и вернитесь сюда."
        }
        L10nKey::AppAgentHooksHomeDirUnresolved => "не удалось определить домашнюю папку",
        L10nKey::AppAgentHooksOpFailed => "Ошибка: {error}",
        L10nKey::AppAgentHooksInstalled => "Установлено",
        L10nKey::AppAgentHooksInstalledEnableCodexThere => {
            "Установлено. Один раз выполните `codex features enable hooks` на том компьютере"
        }
        L10nKey::AppAgentHooksInstalledCodexEnableFailed => {
            "Установлено, но не удалось выполнить `codex features enable hooks` ({error}). Выполните команду вручную один раз"
        }
        L10nKey::AppAgentHooksRemoved => "Удалено",
        L10nKey::AppAgentHooksNothingInstalled => "Ничего не установлено, удалять нечего",
        L10nKey::AppAgentHooksNoTty7Hooks => "Хуки tty7 не найдены, удалять нечего",
        L10nKey::AppAgentHooksInstallFailed => "Не удалось установить хуки: {error}",
        L10nKey::AppAgentHooksRemoveFailed => "Не удалось удалить хуки: {error}",
        L10nKey::AppKeybindingDisplacedNote => {
            "{action} получил сочетание от {previous}. У прежней команды теперь нет сочетания."
        }
        L10nKey::AppLocalServerName => "локальный сервер",
        L10nKey::AppSshParseUnbalancedQuotes => "Непарные кавычки в команде SSH",
        L10nKey::AppSshParseNoRemoteCommands => "Удалённые команды здесь не поддерживаются",
        L10nKey::AppSshParseFlagNeedsValue => "Для -{flag} нужно значение",
        L10nKey::AppSshParseInvalidPort => "Неверный порт «{value}»",
        L10nKey::AppSshParseUnsupportedOption => "Неподдерживаемый параметр «{option}»",
        L10nKey::AppSshParseEnterHost => "Введите хост для подключения",
        L10nKey::AppSshParseBadHost => "Неверный хост «{host}»",
        L10nKey::AppMenuMinimize => "Свернуть",
        L10nKey::AppMenuZoom => "Изменить масштаб",
        L10nKey::SwitcherStatusRestarting => "перезапуск…",
        L10nKey::SwitcherStatusInstalling => "установка…",
        L10nKey::SwitcherStatusConnecting => "подключение…",
        L10nKey::SwitcherStatusConnectFailed => "не удалось подключиться",
        L10nKey::SwitcherStatusNotConnected => "нет соединения",
        L10nKey::SwitcherStatusReconnecting => "переподключение…",
        L10nKey::SwitcherStatusTakenOver => "управление передано",
        L10nKey::SettingsFontDefault => "По умолчанию (как основной)",
        L10nKey::SettingsUiFontDefault => "По умолчанию (системный)",
        L10nKey::ForwardDescriptionPlaceholder => "для чего это",
        L10nKey::SettingsShellDefaultLoginShell => "оболочка входа",
        L10nKey::SettingsShellDetected => "Найденные оболочки",
        L10nKey::SftpErrorUnexpectedReply => "неожиданный ответ: {reply}",
        L10nKey::SftpErrorUnsafeRemoteName => "отклонено небезопасное удалённое имя {name}",
        L10nKey::SftpErrorNoFreeLocalName => {
            "В «Загрузках» нет свободного имени для {name}. Переместите или удалите старые копии"
        }
        L10nKey::SftpReplaceTitle => "Заменить существующие файлы?",
        L10nKey::SftpReplaceBody => "{names} уже есть в этой папке. Загрузка перезапишет их.",
        L10nKey::Replace => "Заменить",
        L10nKey::SftpErrorInvalidOctalMode => "неверные права в восьмеричной записи",
        L10nKey::PaneRestoredScreenBanner => {
            "экран восстановлен, оболочка новая: ничего из того, что выше, уже не работает"
        }
        L10nKey::AppRestartServerBodyInPlace => {
            "Сервер tty7 заменит себя этой сборкой на месте. Оболочки продолжат работать, окно переподключится через мгновение. Панели встроенного SSH-клиента tty7 закроются, их потребуется открыть снова."
        }
        L10nKey::SettingsDaemonStaleDescInPlace => {
            "tty7 обновлён. Сервер может заменить себя на месте, сохранив оболочки. Закроются только встроенные SSH-панели."
        }
        L10nKey::SettingsPerPaneHistory => "Отдельная история команд для каждой панели",
        L10nKey::SettingsPerPaneHistoryDescription => {
            "↑ перебирает историю только этой панели. Для bash и zsh."
        }
        L10nKey::IntegrationNoticeBlocked => {
            "«{wrapper}» перехватывает сообщения оболочки в этой панели. Встроенное дополнение и меню Ctrl+R недоступны. Поиск по истории самой оболочки работает."
        }
        L10nKey::IntegrationNoticeNotEngaged => {
            "Интеграция оболочки tty7 не включилась в этой панели. Встроенное дополнение и меню Ctrl+R недоступны. Возможные причины: свои аргументы запуска оболочки, обёртка PTY или неподдерживаемая оболочка."
        }
        L10nKey::PaneTitleDisconnected => "{title}: нет соединения",
        L10nKey::PaneTitleProcessExited => "{title}: процесс завершён",
        L10nKey::LoopbackForwardFailed => "Не удалось пробросить :{port}: {error}",
        L10nKey::TrayTooltipAgents => "tty7: {parts}",
        L10nKey::TrayAgentSep => ", ",
        L10nKey::CursorShapeBlock => "Блок",
        L10nKey::CursorShapeBar => "Черта",
        L10nKey::CursorShapeUnderline => "Подчёркивание",
        L10nKey::PromptCursorShapeFollow => "Как основной",
        L10nKey::PaletteTryDifferentSearch => "Попробуйте изменить запрос.",
        L10nKey::CompletionListingRemote => "чтение удалённого списка…",
        L10nKey::CompletionRemoteListingFailed => "не удалось прочитать удалённый список: {error}",
        L10nKey::CmdUpdateLocalServer => "Обновить сервер tty7 здесь…",
        L10nKey::CmdUpdateLocalServerSubtitle => "перезапустить с версией этого приложения",
        L10nKey::CmdUpdateRemoteServer => "Обновить сервер tty7 на «{machine}»…",
        L10nKey::CmdUpdateRemoteServerSubtitle => {
            "переустановить эту сборку сервера и завершить все его сеансы"
        }
        L10nKey::AppLocalServerAlreadyCurrent => {
            "Сервер tty7 на этом компьютере уже использует эту сборку ({build})."
        }
        L10nKey::RemoteUpdateBody => {
            "tty7 установит сервер этой сборки на {machine}, даже поверх совместимой версии, и перезапустит его.\n\nВсе сеансы на {machine} завершатся, включая не подключённые к этому окну."
        }
        L10nKey::RemoteUpdateNeedsLocalServer => {
            "Локальный сервер tty7 слишком старый, чтобы обновить сервер на {machine}. Сначала обновите сервер на этом компьютере и повторите попытку."
        }
        L10nKey::GitHubIssues => "Задачи",
        L10nKey::GitHubPulls => "Запросы на слияние",
        L10nKey::GitHubOpen => "Открыто",
        L10nKey::GitHubClosed => "Закрыто",
        L10nKey::GitHubMerged => "Слито",
        L10nKey::GitHubDraft => "Черновик",
        L10nKey::GitHubNotPlanned => "Не запланировано",
        L10nKey::GitHubRefresh => "Обновить",
        L10nKey::GitHubOpenOnGitHub => "Открыть на GitHub",
        L10nKey::GitHubOpenInGraphite => "Открыть в Graphite",
        L10nKey::GitHubShowRemote => "Задачи из",
        L10nKey::GitHubLoadMore => "Загрузить ещё",
        L10nKey::GitHubNoRemote => "Нет удалённого репозитория GitHub",
        L10nKey::GitHubNoRemoteHint => {
            "Ни один удалённый репозиторий (remote) не указывает на github.com."
        }
        L10nKey::GitHubNoIssues => "Задачи не найдены.",
        L10nKey::GitHubNoPulls => "Запросы на слияние не найдены.",
        L10nKey::GitHubSignInHint => "Войдите командой `gh auth login` в терминале и обновите.",
        L10nKey::GitHubNotFoundSignedOut => {
            "GitHub не нашёл репозиторий. Если он закрытый, сначала войдите."
        }
        L10nKey::GitHubNotFoundSignedIn => {
            "GitHub не нашёл репозиторий или ни у одного аккаунта gh нет доступа к нему."
        }
        L10nKey::GitHubUnauthorized => "GitHub отклонил сохранённые данные входа.",
        L10nKey::GitHubRateLimited => "Лимит запросов GitHub исчерпан.",
        L10nKey::GitHubRateLimitResetIn => "Сброс через {n} мин.",
        L10nKey::GitHubRateLimitSignedOut => {
            "Без входа GitHub разрешает 60 запросов в час. Войдите через `gh auth login`, чтобы лимит стал больше."
        }
        L10nKey::GitHubForbidden => "GitHub отклонил запрос.",
        L10nKey::GitHubNetworkError => "Не удалось подключиться к GitHub.",
        L10nKey::GitHubHttpError => "GitHub ответил ошибкой ({code}).",
        L10nKey::GitHubDecodeError => "tty7 не смог прочитать ответ GitHub.",
        L10nKey::GitHubMoreOnGitHub => "Ещё на GitHub",
        L10nKey::GitHubNoDescription => "Описание отсутствует.",
        L10nKey::GitHubFilterByLabel => "Только эта метка",
        L10nKey::GitHubClearLabel => "Сбросить фильтр меток",
        L10nKey::GitHubImage => "изображение",
        L10nKey::GitHubComments => "Комментариев: {count}",
        L10nKey::GitHubCommits => "Коммитов: {count}",
        L10nKey::GitHubOpenedAt => "открыто {when}",
        L10nKey::GitHubUpdatedAt => "обновлено {when}",
        L10nKey::GitHubChecks => "Проверки",
        L10nKey::GitHubChecksPassed => "Пройдено {passed} из {total}",
        L10nKey::GitHubChecksNoneCounted => "Нет проверок с результатом",
        L10nKey::GitHubCheckPassed => "Пройдена",
        L10nKey::GitHubCheckFailed => "Ошибка",
        L10nKey::GitHubCheckPending => "Выполняется",
        L10nKey::GitHubCheckSkipped => "Пропущена",
        L10nKey::GitHubReviews => "Рецензии",
        L10nKey::GitHubReviewApproved => "Одобрено",
        L10nKey::GitHubReviewChangesRequested => "Запрошены изменения",
        L10nKey::GitHubReviewCommented => "Оставлен комментарий",
        L10nKey::GitHubReviewRequested => "Запрошена",
        L10nKey::GitHubReadyToMerge => "Готово к слиянию",
        L10nKey::GitHubMergeConflicts => "Конфликты слияния",
        L10nKey::GitHubReviewRequired => "Нужна рецензия",
        L10nKey::GitHubBehindBase => "Отстаёт от базовой ветки",
        L10nKey::GitHubMergeBlocked => "Заблокировано защитой ветки",
        L10nKey::GitHubThisBranch => "Эта ветка",
        L10nKey::GitHubChecksFailing => "Проверок с ошибкой: {count}",
        L10nKey::GitHubWaitingOnChecks => "Ожидание проверок: {count}",
        L10nKey::GitHubShowLess => "Показать меньше",
        L10nKey::GitHubShowAllFiles => "Показать все файлы ({count})",
        L10nKey::GitHubPassedCount => "Пройдено: {count}",
        L10nKey::GitHubSkippedCount => "Пропущено: {count}",
        L10nKey::GitHubShowFullText => "Показать полный текст",
        L10nKey::GitHubShowHiddenComments => "Показать ещё комментарии ({count})",
        L10nKey::GitHubShowAllReviewers => "Показать всех рецензентов ({count})",
        L10nKey::CmdCopyRaw => "Копировать как есть",
        L10nKey::SettingsSearchCopyJoinWrappedKeywords => {
            "буфер обмена копирование перенос склеить строки claude clipboard copy wrap unwrap join lines raw"
        }
        L10nKey::SettingsCopyJoinWrapped => "Склеивать перенесённые строки при копировании",
        L10nKey::SettingsCopyJoinWrappedDesc => {
            "В панели Claude Code склеивать строки, которые Claude перенёс у края панели, чтобы абзац вставлялся одной строкой. ⌘⌥C или «Копировать как есть» копируют их как на экране"
        }
    })
}

pub fn translate_variant_ru(key: L10nKey, branch: &'static str) -> Option<&'static str> {
    let value = match (key, branch) {
        (L10nKey::SettingsMatchCount, "zero") => "Нет совпадений",
        (L10nKey::SettingsMatchCount, "one") => "{count} совпадение",
        (L10nKey::SettingsMatchCount, "few") => "{count} совпадения",
        (L10nKey::SettingsMatchCount, "many") => "{count} совпадений",
        (L10nKey::SettingsMatchCount, "other") => "{count} совпадений",
        (L10nKey::PanelSearchResultCount, "zero") => "Нет результатов",
        (L10nKey::PanelSearchResultCount, "one") => "{count} результат",
        (L10nKey::PanelSearchResultCount, "few") => "{count} результата",
        (L10nKey::PanelSearchResultCount, "many") => "{count} результатов",
        (L10nKey::PanelSearchResultCount, "other") => "{count} результатов",
        (L10nKey::PanelSearchFileCount, "zero") => "нет файлов",
        (L10nKey::PanelSearchFileCount, "one") => "{count} файл",
        (L10nKey::PanelSearchFileCount, "few") => "{count} файла",
        (L10nKey::PanelSearchFileCount, "many") => "{count} файлов",
        (L10nKey::PanelSearchFileCount, "other") => "{count} файлов",
        (L10nKey::SettingsRestoreChanged, "zero") => "Сбросить изменения",
        (L10nKey::SettingsRestoreChanged, "one") => "Сбросить {count} изменение",
        (L10nKey::SettingsRestoreChanged, "few") => "Сбросить {count} изменения",
        (L10nKey::SettingsRestoreChanged, "many") => "Сбросить {count} изменений",
        (L10nKey::SettingsRestoreChanged, "other") => "Сбросить {count} изменений",
        (L10nKey::SettingsMoreHosts, "zero") => "Других хостов нет",
        (L10nKey::SettingsMoreHosts, "one") => "Ещё {count} хост",
        (L10nKey::SettingsMoreHosts, "few") => "Ещё {count} хоста",
        (L10nKey::SettingsMoreHosts, "many") => "Ещё {count} хостов",
        (L10nKey::SettingsMoreHosts, "other") => "Ещё {count} хостов",
        (L10nKey::SettingsMoreAgents, "zero") => "Других агентов нет",
        (L10nKey::SettingsMoreAgents, "one") => "Доступен ещё {count} агент",
        (L10nKey::SettingsMoreAgents, "few") => "Доступны ещё {count} агента",
        (L10nKey::SettingsMoreAgents, "many") => "Доступно ещё {count} агентов",
        (L10nKey::SettingsMoreAgents, "other") => "Доступно ещё {count} агентов",
        (L10nKey::SettingsAliasesLinked, "zero") => "Псевдонимы ещё не связаны.",
        (L10nKey::SettingsAliasesLinked, "one") => "Связан {count} псевдоним.",
        (L10nKey::SettingsAliasesLinked, "few") => "Связано {count} псевдонима.",
        (L10nKey::SettingsAliasesLinked, "many") => "Связано {count} псевдонимов.",
        (L10nKey::SettingsAliasesLinked, "other") => "Связано {count} псевдонимов.",
        (L10nKey::SettingsImportSummary, "zero") => {
            "Новых хостов нет. Обновлено: {updated}, уже актуальны: {unchanged}"
        }
        (L10nKey::SettingsImportSummary, "one") => {
            "Добавлен {count} хост. Обновлено: {updated}, уже актуальны: {unchanged}"
        }
        (L10nKey::SettingsImportSummary, "few") => {
            "Добавлено {count} хоста. Обновлено: {updated}, уже актуальны: {unchanged}"
        }
        (L10nKey::SettingsImportSummary, "many") => {
            "Добавлено {count} хостов. Обновлено: {updated}, уже актуальны: {unchanged}"
        }
        (L10nKey::SettingsImportSummary, "other") => {
            "Добавлено {count} хостов. Обновлено: {updated}, уже актуальны: {unchanged}"
        }
        (L10nKey::SettingsImportIgnored, "zero") => {
            "Для всех параметров файла есть настройки tty7."
        }
        (L10nKey::SettingsImportIgnored, "one") => {
            "В tty7 нет настройки для {count} параметра. Он оставлен в файле: {options}"
        }
        (L10nKey::SettingsImportIgnored, "few") => {
            "В tty7 нет настроек для {count} параметров. Они оставлены в файле: {options}"
        }
        (L10nKey::SettingsImportIgnored, "many") => {
            "В tty7 нет настроек для {count} параметров. Они оставлены в файле: {options}"
        }
        (L10nKey::SettingsImportIgnored, "other") => {
            "В tty7 нет настроек для {count} параметров. Они оставлены в файле: {options}"
        }
        (L10nKey::SettingsRulesOpenedWithConnection, "zero") => {
            "Нет правил для запуска при подключении"
        }
        (L10nKey::SettingsRulesOpenedWithConnection, "one") => {
            "{count} правило, запускается при подключении"
        }
        (L10nKey::SettingsRulesOpenedWithConnection, "few") => {
            "{count} правила, запускаются при подключении"
        }
        (L10nKey::SettingsRulesOpenedWithConnection, "many") => {
            "{count} правил, запускаются при подключении"
        }
        (L10nKey::SettingsRulesOpenedWithConnection, "other") => {
            "{count} правил, запускаются при подключении"
        }
        (L10nKey::SettingsOfflineMachines, "zero") => {
            "Других сохранённых компьютеров без соединения нет."
        }
        (L10nKey::SettingsOfflineMachines, "one") => {
            "Ещё {count} сохранённый компьютер не подключён. Откройте на нём рабочую область, чтобы установить хуки."
        }
        (L10nKey::SettingsOfflineMachines, "few") => {
            "Ещё {count} сохранённых компьютера не подключены. Откройте рабочую область на нужном компьютере, чтобы установить хуки."
        }
        (L10nKey::SettingsOfflineMachines, "many") => {
            "Ещё {count} сохранённых компьютеров не подключены. Откройте рабочую область на нужном компьютере, чтобы установить хуки."
        }
        (L10nKey::SettingsOfflineMachines, "other") => {
            "Ещё {count} сохранённых компьютеров не подключены. Откройте рабочую область на нужном компьютере, чтобы установить хуки."
        }
        (L10nKey::SettingsForgetPasswordSharedBody, "zero") => {
            "Другие профили не используют {endpoint}."
        }
        (L10nKey::SettingsForgetPasswordSharedBody, "one") => {
            "Ещё {count} профиль хоста использует {endpoint}. Для этого подключения тоже потребуется снова ввести пароль."
        }
        (L10nKey::SettingsForgetPasswordSharedBody, "few") => {
            "Ещё {count} профиля хоста используют {endpoint}. Для этих подключений тоже потребуется снова ввести пароль."
        }
        (L10nKey::SettingsForgetPasswordSharedBody, "many") => {
            "Ещё {count} профилей хоста используют {endpoint}. Для этих подключений тоже потребуется снова ввести пароль."
        }
        (L10nKey::SettingsForgetPasswordSharedBody, "other") => {
            "Ещё {count} профилей хоста используют {endpoint}. Для этих подключений тоже потребуется снова ввести пароль."
        }
        (L10nKey::SettingsDeleteProfileCascade, "zero") => {
            "Нет сохранённых удалённых рабочих областей с адресом {endpoint}."
        }
        (L10nKey::SettingsDeleteProfileCascade, "one") => {
            "Сохранённая удалённая рабочая область ({count}) с адресом {endpoint} тоже удалится с этого компьютера. Удалённый сеанс продолжит работать и вернётся в список после подключения с новым профилем."
        }
        (L10nKey::SettingsDeleteProfileCascade, "few") => {
            "Сохранённые удалённые рабочие области ({count}) с адресом {endpoint} тоже удалятся с этого компьютера. Удалённые сеансы продолжат работать и вернутся в список после подключения с новым профилем."
        }
        (L10nKey::SettingsDeleteProfileCascade, "many") => {
            "Сохранённые удалённые рабочие области ({count}) с адресом {endpoint} тоже удалятся с этого компьютера. Удалённые сеансы продолжат работать и вернутся в список после подключения с новым профилем."
        }
        (L10nKey::SettingsDeleteProfileCascade, "other") => {
            "Сохранённые удалённые рабочие области ({count}) с адресом {endpoint} тоже удалятся с этого компьютера. Удалённые сеансы продолжат работать и вернутся в список после подключения с новым профилем."
        }
        (L10nKey::SftpReplaceBody, "zero") => "{names}: нет файлов для замены.",
        (L10nKey::SftpReplaceBody, "one") => {
            "{names} уже есть в этой папке. Загрузка перезапишет файл."
        }
        (L10nKey::SftpReplaceBody, "few") => {
            "{names} уже есть в этой папке. Загрузка перезапишет файлы."
        }
        (L10nKey::SftpReplaceBody, "many") => {
            "{names} уже есть в этой папке. Загрузка перезапишет файлы."
        }
        (L10nKey::SftpReplaceBody, "other") => {
            "{names} уже есть в этой папке. Загрузка перезапишет файлы."
        }
        (L10nKey::CloseTabsTitle, "zero") => "Нет вкладок для закрытия",
        (L10nKey::CloseTabsTitle, "one") => "Закрыть {count} вкладку?",
        (L10nKey::CloseTabsTitle, "few") => "Закрыть {count} вкладки?",
        (L10nKey::CloseTabsTitle, "many") => "Закрыть {count} вкладок?",
        (L10nKey::CloseTabsTitle, "other") => "Закрыть {count} вкладок?",
        (L10nKey::AppTabsNotRestored, "zero") => "Все вкладки прошлого сеанса восстановлены",
        (L10nKey::AppTabsNotRestored, "one") => {
            "Не удалось восстановить {count} вкладку прошлого сеанса"
        }
        (L10nKey::AppTabsNotRestored, "few") => {
            "Не удалось восстановить {count} вкладки прошлого сеанса"
        }
        (L10nKey::AppTabsNotRestored, "many") => {
            "Не удалось восстановить {count} вкладок прошлого сеанса"
        }
        (L10nKey::AppTabsNotRestored, "other") => {
            "Не удалось восстановить {count} вкладок прошлого сеанса"
        }
        (L10nKey::LaunchWorkspacesLeftRunning, "zero") => {
            "Это окно восстановлено. Других рабочих областей в фоне нет."
        }
        (L10nKey::LaunchWorkspacesLeftRunning, "one") => {
            "Восстановлено только это окно. Ещё {count} рабочая область работает в фоне. Откройте её на боковой панели."
        }
        (L10nKey::LaunchWorkspacesLeftRunning, "few") => {
            "Восстановлено только это окно. Ещё {count} рабочие области работают в фоне. Откройте их на боковой панели."
        }
        (L10nKey::LaunchWorkspacesLeftRunning, "many") => {
            "Восстановлено только это окно. Ещё {count} рабочих областей работают в фоне. Откройте их на боковой панели."
        }
        (L10nKey::LaunchWorkspacesLeftRunning, "other") => {
            "Восстановлено только это окно. Ещё {count} рабочих областей работают в фоне. Откройте их на боковой панели."
        }
        (L10nKey::ScmFilesChanged, "zero") => "Нет изменённых файлов",
        (L10nKey::ScmFilesChanged, "one") => "Изменён {count} файл",
        (L10nKey::ScmFilesChanged, "few") => "Изменено {count} файла",
        (L10nKey::ScmFilesChanged, "many") => "Изменено {count} файлов",
        (L10nKey::ScmFilesChanged, "other") => "Изменено {count} файлов",
        (L10nKey::ScmStagedFileCount, "zero") => "Нет файлов в индексе",
        (L10nKey::ScmStagedFileCount, "one") => "В индексе {count} файл",
        (L10nKey::ScmStagedFileCount, "few") => "В индексе {count} файла",
        (L10nKey::ScmStagedFileCount, "many") => "В индексе {count} файлов",
        (L10nKey::ScmStagedFileCount, "other") => "В индексе {count} файлов",
        (L10nKey::PanelMoreChangedFiles, "zero") => "Других изменённых файлов нет.",
        (L10nKey::PanelMoreChangedFiles, "one") => {
            "…и ещё {count} изменённый файл. Посмотрите его командой git diff."
        }
        (L10nKey::PanelMoreChangedFiles, "few") => {
            "…и ещё {count} изменённых файла. Посмотрите их командой git diff."
        }
        (L10nKey::PanelMoreChangedFiles, "many") => {
            "…и ещё {count} изменённых файлов. Посмотрите их командой git diff."
        }
        (L10nKey::PanelMoreChangedFiles, "other") => {
            "…и ещё {count} изменённых файлов. Посмотрите их командой git diff."
        }
        (L10nKey::DiffChangedFiles, "zero") => "Нет изменённых файлов",
        (L10nKey::DiffChangedFiles, "one") => "{count} изменённый файл",
        (L10nKey::DiffChangedFiles, "few") => "{count} изменённых файла",
        (L10nKey::DiffChangedFiles, "many") => "{count} изменённых файлов",
        (L10nKey::DiffChangedFiles, "other") => "{count} изменённых файлов",
        (L10nKey::DiffUntrackedCount, "zero") => " · нет неотслеживаемых",
        (L10nKey::DiffUntrackedCount, "one") => " · {count} неотслеживаемый",
        (L10nKey::DiffUntrackedCount, "few") => " · {count} неотслеживаемых",
        (L10nKey::DiffUntrackedCount, "many") => " · {count} неотслеживаемых",
        (L10nKey::DiffUntrackedCount, "other") => " · {count} неотслеживаемых",
        (L10nKey::DiffMoreFiles, "zero") => "Других изменённых файлов нет.",
        (L10nKey::DiffMoreFiles, "one") => {
            "…и ещё {count} изменённый файл. Посмотрите его командой git diff в терминале."
        }
        (L10nKey::DiffMoreFiles, "few") => {
            "…и ещё {count} изменённых файла. Посмотрите их командой git diff в терминале."
        }
        (L10nKey::DiffMoreFiles, "many") => {
            "…и ещё {count} изменённых файлов. Посмотрите их командой git diff в терминале."
        }
        (L10nKey::DiffMoreFiles, "other") => {
            "…и ещё {count} изменённых файлов. Посмотрите их командой git diff в терминале."
        }
        (L10nKey::DiffUntrackedHeader, "zero") => "Неотслеживаемые файлы (0)",
        (L10nKey::DiffUntrackedHeader, "one") => "Неотслеживаемые файлы ({count})",
        (L10nKey::DiffUntrackedHeader, "few") => "Неотслеживаемые файлы ({count})",
        (L10nKey::DiffUntrackedHeader, "many") => "Неотслеживаемые файлы ({count})",
        (L10nKey::DiffUntrackedHeader, "other") => "Неотслеживаемые файлы ({count})",
        (L10nKey::DiffMoreUntracked, "zero") => "Других неотслеживаемых файлов нет.",
        (L10nKey::DiffMoreUntracked, "one") => {
            "…и ещё {count} файл. Посмотрите его командой git status в терминале."
        }
        (L10nKey::DiffMoreUntracked, "few") => {
            "…и ещё {count} файла. Посмотрите их командой git status в терминале."
        }
        (L10nKey::DiffMoreUntracked, "many") => {
            "…и ещё {count} файлов. Посмотрите их командой git status в терминале."
        }
        (L10nKey::DiffMoreUntracked, "other") => {
            "…и ещё {count} файлов. Посмотрите их командой git status в терминале."
        }
        (L10nKey::DiffUntrackedSummary, "zero") => "Нет неотслеживаемых",
        (L10nKey::DiffUntrackedSummary, "one") => "{count} неотслеживаемый",
        (L10nKey::DiffUntrackedSummary, "few") => "{count} неотслеживаемых",
        (L10nKey::DiffUntrackedSummary, "many") => "{count} неотслеживаемых",
        (L10nKey::DiffUntrackedSummary, "other") => "{count} неотслеживаемых",
        (L10nKey::HomeTimeMinutesAgo, "zero") => "только что",
        (L10nKey::HomeTimeMinutesAgo, "one") => "{count} минуту назад",
        (L10nKey::HomeTimeMinutesAgo, "few") => "{count} минуты назад",
        (L10nKey::HomeTimeMinutesAgo, "many") => "{count} минут назад",
        (L10nKey::HomeTimeMinutesAgo, "other") => "{count} минут назад",
        (L10nKey::HomeTimeHoursAgo, "zero") => "меньше часа назад",
        (L10nKey::HomeTimeHoursAgo, "one") => "{count} час назад",
        (L10nKey::HomeTimeHoursAgo, "few") => "{count} часа назад",
        (L10nKey::HomeTimeHoursAgo, "many") => "{count} часов назад",
        (L10nKey::HomeTimeHoursAgo, "other") => "{count} часов назад",
        (L10nKey::HomeTimeDaysAgo, "zero") => "сегодня",
        (L10nKey::HomeTimeDaysAgo, "one") => "{count} день назад",
        (L10nKey::HomeTimeDaysAgo, "few") => "{count} дня назад",
        (L10nKey::HomeTimeDaysAgo, "many") => "{count} дней назад",
        (L10nKey::HomeTimeDaysAgo, "other") => "{count} дней назад",
        (L10nKey::HomeTimeWeeksAgo, "zero") => "меньше недели назад",
        (L10nKey::HomeTimeWeeksAgo, "one") => "{count} неделю назад",
        (L10nKey::HomeTimeWeeksAgo, "few") => "{count} недели назад",
        (L10nKey::HomeTimeWeeksAgo, "many") => "{count} недель назад",
        (L10nKey::HomeTimeWeeksAgo, "other") => "{count} недель назад",
        (L10nKey::HomeTimeMonthsAgo, "zero") => "меньше месяца назад",
        (L10nKey::HomeTimeMonthsAgo, "one") => "{count} месяц назад",
        (L10nKey::HomeTimeMonthsAgo, "few") => "{count} месяца назад",
        (L10nKey::HomeTimeMonthsAgo, "many") => "{count} месяцев назад",
        (L10nKey::HomeTimeMonthsAgo, "other") => "{count} месяцев назад",
        (L10nKey::WindowStopShells, "zero") => "tty7 забудет расположение и рабочие папки окна.",
        (L10nKey::WindowStopShells, "one") => "{count} работающая оболочка будет завершена.",
        (L10nKey::WindowStopShells, "few") => "{count} работающие оболочки будут завершены.",
        (L10nKey::WindowStopShells, "many") => "{count} работающих оболочек будет завершено.",
        (L10nKey::WindowStopShells, "other") => "{count} работающих оболочек будет завершено.",
        (L10nKey::WindowDeleteShells, "zero") => "tty7 забудет расположение и рабочие папки окна.",
        (L10nKey::WindowDeleteShells, "one") => {
            "{count} работающая оболочка будет завершена, расположение удалится."
        }
        (L10nKey::WindowDeleteShells, "few") => {
            "{count} работающие оболочки будут завершены, расположение удалится."
        }
        (L10nKey::WindowDeleteShells, "many") => {
            "{count} работающих оболочек будет завершено, расположение удалится."
        }
        (L10nKey::WindowDeleteShells, "other") => {
            "{count} работающих оболочек будет завершено, расположение удалится."
        }
        (L10nKey::GitHubComments, "zero") => "Нет комментариев",
        (L10nKey::GitHubComments, "one") => "{count} комментарий",
        (L10nKey::GitHubComments, "few") => "{count} комментария",
        (L10nKey::GitHubComments, "many") => "{count} комментариев",
        (L10nKey::GitHubComments, "other") => "{count} комментариев",
        (L10nKey::GitHubCommits, "zero") => "Нет коммитов",
        (L10nKey::GitHubCommits, "one") => "{count} коммит",
        (L10nKey::GitHubCommits, "few") => "{count} коммита",
        (L10nKey::GitHubCommits, "many") => "{count} коммитов",
        (L10nKey::GitHubCommits, "other") => "{count} коммитов",
        (L10nKey::GitHubChecksFailing, "zero") => "Нет проверок с ошибками",
        (L10nKey::GitHubChecksFailing, "one") => "{count} проверка с ошибкой",
        (L10nKey::GitHubChecksFailing, "few") => "{count} проверки с ошибкой",
        (L10nKey::GitHubChecksFailing, "many") => "{count} проверок с ошибкой",
        (L10nKey::GitHubChecksFailing, "other") => "{count} проверок с ошибкой",
        (L10nKey::GitHubWaitingOnChecks, "zero") => "Нет ожидаемых проверок",
        (L10nKey::GitHubWaitingOnChecks, "one") => "Ожидание {count} проверки",
        (L10nKey::GitHubWaitingOnChecks, "few") => "Ожидание {count} проверок",
        (L10nKey::GitHubWaitingOnChecks, "many") => "Ожидание {count} проверок",
        (L10nKey::GitHubWaitingOnChecks, "other") => "Ожидание {count} проверок",
        (L10nKey::GitHubShowAllFiles, "zero") => "Нет файлов",
        (L10nKey::GitHubShowAllFiles, "one") => "Показать {count} файл",
        (L10nKey::GitHubShowAllFiles, "few") => "Показать все {count} файла",
        (L10nKey::GitHubShowAllFiles, "many") => "Показать все {count} файлов",
        (L10nKey::GitHubShowAllFiles, "other") => "Показать все {count} файлов",
        (L10nKey::GitHubPassedCount, "zero") => "Ни одна не пройдена",
        (L10nKey::GitHubPassedCount, "one") => "Пройдена {count} проверка",
        (L10nKey::GitHubPassedCount, "few") => "Пройдено {count} проверки",
        (L10nKey::GitHubPassedCount, "many") => "Пройдено {count} проверок",
        (L10nKey::GitHubPassedCount, "other") => "Пройдено {count} проверок",
        (L10nKey::GitHubSkippedCount, "zero") => "Ни одна не пропущена",
        (L10nKey::GitHubSkippedCount, "one") => "Пропущена {count} проверка",
        (L10nKey::GitHubSkippedCount, "few") => "Пропущено {count} проверки",
        (L10nKey::GitHubSkippedCount, "many") => "Пропущено {count} проверок",
        (L10nKey::GitHubSkippedCount, "other") => "Пропущено {count} проверок",
        (L10nKey::GitHubShowHiddenComments, "zero") => "Других комментариев нет",
        (L10nKey::GitHubShowHiddenComments, "one") => "Показать ещё {count} комментарий",
        (L10nKey::GitHubShowHiddenComments, "few") => "Показать ещё {count} комментария",
        (L10nKey::GitHubShowHiddenComments, "many") => "Показать ещё {count} комментариев",
        (L10nKey::GitHubShowHiddenComments, "other") => "Показать ещё {count} комментариев",
        (L10nKey::GitHubShowAllReviewers, "zero") => "Нет рецензентов",
        (L10nKey::GitHubShowAllReviewers, "one") => "Показать {count} рецензента",
        (L10nKey::GitHubShowAllReviewers, "few") => "Показать всех {count} рецензентов",
        (L10nKey::GitHubShowAllReviewers, "many") => "Показать всех {count} рецензентов",
        (L10nKey::GitHubShowAllReviewers, "other") => "Показать всех {count} рецензентов",
        _ => return None,
    };
    Some(value)
}
